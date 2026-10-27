//! The DSH adapter: the server side of DSH Link v1.
//!
//! The direction is the opposite of the other two engines. Codex and Claude Code
//! are things this application spawns; DSH is a process this application cannot
//! reach into, so its helper plugin dials us. This module therefore implements
//! *accepting* a link — handshake, frame loop, request correlation — and then the
//! usual engine methods on top of it.
//!
//! ### Why the transport is not in here
//!
//! `serve` takes two channels of text frames rather than a socket. The HTTP
//! server owns the socket, and the engine owns the protocol; keeping them apart is
//! what lets the interesting half be tested with no network at all.
//!
//! ### What "connected" means
//!
//! A plugin is attached and has answered the handshake. When it goes away, every
//! pending call fails, every cached interaction is withdrawn, and the UI says so.
//! The engine deliberately does not retry: it is not the one dialling.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use agi_engine::{Answer, Engine, EngineError, EngineEvent};
use agi_protocol::domain::{
    ContextPressure, Interaction, InteractionKind, Question, Record, Session, SessionKey,
};
use agi_protocol::engine::{Capabilities, EngineId};
use agi_protocol::link::{
    ApprovalFrame, DesktopFrame, EventFrame, LinkFrame, NoticeFrame, QuestionFrame, ResultFrame,
    StreamEndFrame, StreamItemFrame, UpstreamError,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::projection::{self, Projected};

/// How long a unary call may take before it is failed.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a stream may take to produce its opening frame.
const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the first session list may keep retrying while DSH composes.
///
/// Measured on a real host: the strict descriptors are registered during
/// composition, but the service behind `session/list` (`sessionController`) comes
/// up later. The gateway then answers `service-unavailable` — not because the
/// endpoint is missing, but because it cannot dispatch yet. There is no API to
/// wait on a service, so the only honest gate is trying again.
const COMPOSE_GRACE: Duration = Duration::from_secs(60);

/// How many sessions get their projections read after a list.
const ENRICH_LIMIT: usize = 40;

/// How long to wait between projection reads.
const ENRICH_SPACING: Duration = Duration::from_millis(60);

/// Records kept per session in memory. Beyond this the oldest are dropped; a
/// conversation view pages backwards from the engine when it needs them again.
const MAX_CACHED_RECORDS: usize = 4_000;

/// Everything the frame loop mutates.
struct Inner {
    /// The channel to the socket writer, while a plugin is attached.
    out: Option<mpsc::Sender<String>>,
    /// Unary calls waiting for their `result`.
    pending: HashMap<String, oneshot::Sender<Result<Value, UpstreamError>>>,
    /// Open streams, keyed by call id.
    streams: HashMap<String, mpsc::Sender<Value>>,
    sessions: HashMap<String, Session>,
    records: HashMap<String, VecDeque<Record>>,
    interactions: HashMap<String, Interaction>,
    /// A one-line description of the attached plugin.
    detail: Option<String>,
    plugin_version: Option<String>,
    endpoints: usize,
}

impl Inner {
    fn new() -> Self {
        Self {
            out: None,
            pending: HashMap::new(),
            streams: HashMap::new(),
            sessions: HashMap::new(),
            records: HashMap::new(),
            interactions: HashMap::new(),
            detail: None,
            plugin_version: None,
            endpoints: 0,
        }
    }
}

/// The DSH engine.
pub struct DshEngine {
    /// The token the plugin must present, from the endpoint file.
    token: String,
    events: broadcast::Sender<EngineEvent>,
    inner: Mutex<Inner>,
    next_id: AtomicU64,
    /// A weak handle to self, so `refresh_sessions(&self)` can start a task that
    /// outlives the call. `Arc::new_cyclic` fills it in after construction.
    me: Weak<DshEngine>,
}

impl DshEngine {
    /// Build an engine that accepts a plugin presenting `token`.
    pub fn new(token: impl Into<String>) -> Arc<Self> {
        let (events, _) = broadcast::channel(512);
        let mut inner = Inner::new();
        // Said out loud from the first snapshot: an engine with no detail reads
        // like an engine that failed, not one that is waiting.
        inner.detail = Some("no plugin attached".to_string());
        Arc::new_cyclic(|me| Self {
            token: token.into(),
            events,
            inner: Mutex::new(inner),
            next_id: AtomicU64::new(1),
            me: me.clone(),
        })
    }

    /// The broadcast channel every subscriber listens on.
    pub fn events(&self) -> broadcast::Sender<EngineEvent> {
        self.events.clone()
    }

    /// Lock without letting one panicking task poison the engine.
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emit(&self, event: EngineEvent) {
        // A send with no receivers is the normal state before the core
        // subscribes, not an error.
        let _ = self.events.send(event);
    }

    fn next_id(&self) -> String {
        format!("dsh-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Whether a plugin is attached.
    pub fn connected(&self) -> bool {
        self.inner().out.is_some()
    }

    /// The plugin's version, once one has said hello.
    pub fn plugin_version(&self) -> Option<String> {
        self.inner().plugin_version.clone()
    }

    /// Write one frame to the plugin.
    async fn send(&self, frame: DesktopFrame) -> Result<(), EngineError> {
        let sender = self.inner().out.clone();
        let Some(sender) = sender else {
            return Err(EngineError::NotConnected {
                engine: EngineId::Dsh,
            });
        };
        let text = serde_json::to_string(&frame)
            .map_err(|error| EngineError::upstream(format!("could not encode a frame: {error}")))?;
        sender
            .send(text)
            .await
            .map_err(|_| EngineError::NotConnected {
                engine: EngineId::Dsh,
            })
    }

    /// One unary call with an explicit deadline.
    async fn request_with(
        &self,
        frame: DesktopFrame,
        id: String,
        timeout: Duration,
    ) -> Result<Value, EngineError> {
        let (tx, rx) = oneshot::channel();
        {
            self.inner().pending.insert(id.clone(), tx);
        }
        if let Err(error) = self.send(frame).await {
            self.inner().pending.remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(upstream))) => Err(EngineError::Upstream {
                code: upstream.code,
                message: upstream.message,
                details: upstream.details,
            }),
            Ok(Err(_)) => Err(EngineError::upstream("the plugin dropped the call")),
            Err(_) => {
                self.inner().pending.remove(&id);
                Err(EngineError::Timeout("endpoint call"))
            }
        }
    }

    /// Call an endpoint with the `request` wrapper DSH's descriptors use.
    async fn call(&self, endpoint: &str, args: Value) -> Result<Value, EngineError> {
        self.call_with(endpoint, args, REQUEST_TIMEOUT).await
    }

    /// Call an endpoint, allowing a longer wait than the default.
    async fn call_with(
        &self,
        endpoint: &str,
        args: Value,
        timeout: Duration,
    ) -> Result<Value, EngineError> {
        let id = self.next_id();
        self.request_with(
            DesktopFrame::invoke(id.clone(), endpoint, args),
            id,
            timeout,
        )
        .await
    }

    /// Read the opening frame of a stream, then cancel it.
    ///
    /// `session/follow` is read exactly this way: its first frame carries the log
    /// cut a page is paged by, and pulling a second frame is what activates a cold
    /// session's agent. One frame, then close.
    async fn stream_opening(&self, endpoint: &str, args: Value) -> Result<Value, EngineError> {
        let id = self.next_id();
        let (tx, mut rx) = mpsc::channel::<Value>(8);
        {
            self.inner().streams.insert(id.clone(), tx);
        }
        if let Err(error) = self
            .send(DesktopFrame::stream(id.clone(), endpoint, args))
            .await
        {
            self.inner().streams.remove(&id);
            return Err(error);
        }
        let first = tokio::time::timeout(STREAM_OPEN_TIMEOUT, rx.recv()).await;
        let _ = self
            .send(DesktopFrame::Cancel {
                v: agi_protocol::link::LINK_VERSION,
                id: id.clone(),
            })
            .await;
        self.inner().streams.remove(&id);

        match first {
            Ok(Some(value)) => Ok(value),
            Ok(None) => Err(EngineError::upstream(
                "the stream closed before its opening frame",
            )),
            Err(_) => Err(EngineError::Timeout("stream opening frame")),
        }
    }

    /// Take over one accepted WebSocket for as long as it lives.
    ///
    /// `incoming` yields text frames from the plugin, `outgoing` accepts the
    /// frames to write back. Returns when the plugin disconnects or presents a
    /// token this launch does not accept.
    pub async fn serve(
        self: Arc<Self>,
        mut incoming: mpsc::Receiver<String>,
        outgoing: mpsc::Sender<String>,
    ) {
        // The handshake is the only frame allowed to be unauthenticated.
        let Some(first) = incoming.recv().await else {
            return;
        };
        let Some(LinkFrame::Hello(hello)) = LinkFrame::parse(&first) else {
            return;
        };
        if hello.token != self.token {
            tracing::warn!("dsh link: refused a hello with the wrong token");
            return;
        }

        let welcome = DesktopFrame::welcome(format!("AGI Mizar {}", env!("CARGO_PKG_VERSION")));
        if let Ok(text) = serde_json::to_string(&welcome) {
            if outgoing.send(text).await.is_err() {
                return;
            }
        }

        let detail = format!(
            "plugin {} · {} endpoints",
            hello.plugin.version,
            hello.endpoints.len()
        );
        {
            let mut inner = self.inner();
            inner.out = Some(outgoing.clone());
            inner.plugin_version = Some(hello.plugin.version.clone());
            inner.endpoints = hello.endpoints.len();
            inner.detail = Some(detail.clone());
        }
        tracing::info!(
            "dsh link: plugin {} attached on {} ({} endpoints, {} services)",
            hello.plugin.version,
            hello.dsh.platform,
            hello.endpoints.len(),
            hello.services.len()
        );
        self.emit(EngineEvent::StatusChanged {
            connected: true,
            detail: Some(detail),
        });

        // Draw the list immediately: a window opened before the plugin attached
        // would otherwise show an empty engine until something else changed.
        //
        // Concurrently, not awaited. The answer arrives as a frame, and frames
        // are only delivered by the loop below — awaiting this call here would
        // wait for a reply that only starts flowing once we stop waiting.
        {
            let engine = Arc::clone(&self);
            tokio::spawn(async move {
                if let Err(error) = engine.refresh_sessions().await {
                    tracing::warn!("dsh link: could not list sessions yet — {error}");
                }
            });
        }

        while let Some(text) = incoming.recv().await {
            let Some(frame) = LinkFrame::parse(&text) else {
                // A frame that fails to parse is otherwise a silence, and a
                // silence on this link looks like the host hanging.
                tracing::warn!(
                    "dsh link: dropped a {} byte frame that did not parse — head {}",
                    text.len(),
                    &text[..text.len().min(160)]
                );
                continue;
            };
            match frame {
                LinkFrame::Event(event) => self.on_event(event),
                LinkFrame::Result(result) => self.on_result(result),
                LinkFrame::StreamItem(item) => self.on_stream_item(item),
                LinkFrame::StreamEnd(end) => self.on_stream_end(end),
                LinkFrame::Approval(frame) => self.on_approval(frame),
                LinkFrame::Question(frame) => self.on_question(frame),
                LinkFrame::Notice(frame) => self.on_notice(frame),
                // A second hello, or a frame from a newer plugin: ignored.
                _ => {}
            }
        }

        self.detach();
    }

    /// The plugin went away: fail everything that was waiting.
    fn detach(&self) {
        let (pending, interactions, detail) = {
            let mut inner = self.inner();
            inner.out = None;
            inner.streams.clear();
            inner.detail = Some("plugin disconnected".to_string());
            (
                std::mem::take(&mut inner.pending),
                std::mem::take(&mut inner.interactions),
                inner.detail.clone(),
            )
        };
        for (_, sender) in pending {
            let _ = sender.send(Err(UpstreamError {
                code: "DISCONNECTED".to_string(),
                message: "the plugin disconnected".to_string(),
                details: None,
            }));
        }
        // An approval is one-shot and tied to an open turn; withdrawing it here
        // is what keeps a vanished plugin from hanging an agent on our side.
        for (id, _) in interactions {
            self.emit(EngineEvent::InteractionResolved { id });
        }
        tracing::info!("dsh link: plugin detached");
        self.emit(EngineEvent::StatusChanged {
            connected: false,
            detail,
        });
    }

    /// One committed record or projection change.
    fn on_event(&self, event: EventFrame) {
        let Some(session_id) = event.session_id.clone() else {
            return;
        };

        if event.name == "projection/changed" {
            self.on_projection(&session_id, &event.payload);
            return;
        }

        let Some(projected) = projection::project(&event.payload) else {
            return;
        };
        self.apply(&session_id, projected);
    }

    /// A projection unit changed: title, model, permissions, running flag.
    fn on_projection(&self, session_id: &str, payload: &Value) {
        let key = payload
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let value = payload.get("value").unwrap_or(&Value::Null);

        let updated = {
            let mut inner = self.inner();
            let Some(session) = inner.sessions.get_mut(session_id) else {
                return;
            };
            match key {
                "title" => session.title = value.as_str().map(str::to_string),
                "modelSelection" => {
                    session.model = value
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }
                _ => return,
            }
            session.clone()
        };
        self.emit(EngineEvent::SessionUpdated(updated));
    }

    /// Apply one projected event to the caches and announce it.
    fn apply(&self, session_id: &str, projected: Projected) {
        let (session, record) = {
            let mut inner = self.inner();

            // The session first, in its own scope: holding the `entry` borrow
            // while touching `inner.records` is two mutable borrows of one map.
            let session = {
                let session = inner
                    .sessions
                    .entry(session_id.to_string())
                    .or_insert_with(|| {
                        Session::bare(SessionKey::new(EngineId::Dsh, session_id), projected.time)
                    });

                if let Some(running) = projected.running {
                    session.running = running;
                    if running {
                        session.turns = session.turns.saturating_add(1);
                    }
                }
                session.updated_at = projected.time;
                session.clone()
            };

            let record = projected.record();
            if let Some(record) = record.clone() {
                let bucket = inner.records.entry(session_id.to_string()).or_default();
                bucket.push_back(record);
                while bucket.len() > MAX_CACHED_RECORDS {
                    bucket.pop_front();
                }
            }
            (session, record)
        };

        self.emit(EngineEvent::SessionUpdated(session.clone()));
        if let Some(record) = record {
            self.emit(EngineEvent::RecordAppended {
                session: session.key,
                record,
            });
        }
    }

    fn on_result(&self, result: ResultFrame) {
        let sender = self.inner().pending.remove(&result.id);
        if let Some(sender) = sender {
            let _ = sender.send(result.into_result());
        }
    }

    fn on_stream_item(&self, item: StreamItemFrame) {
        let inner = self.inner();
        if let Some(sender) = inner.streams.get(&item.id) {
            // Bounded channel: a plugin that outruns its reader loses items rather
            // than growing the process. The opening-frame reader takes one.
            let _ = sender.try_send(item.value.unwrap_or(Value::Null));
        }
    }

    fn on_stream_end(&self, end: StreamEndFrame) {
        self.inner().streams.remove(&end.id);
    }

    fn on_approval(&self, frame: ApprovalFrame) {
        let interaction = Interaction {
            id: frame.id.clone(),
            session: frame
                .request
                .get("agent")
                .and_then(|agent| agent.get("id"))
                .and_then(Value::as_str)
                .map(|id| SessionKey::new(EngineId::Dsh, id)),
            kind: InteractionKind::Approval,
            title: frame
                .request
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("approval")
                .to_string(),
            detail: frame
                .request
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string),
            requested_at: now_ms(),
            questions: Vec::new(),
        };
        self.inner()
            .interactions
            .insert(interaction.id.clone(), interaction.clone());
        self.emit(EngineEvent::InteractionRequested(interaction));
    }

    fn on_question(&self, frame: QuestionFrame) {
        let questions = read_questions(&frame.request);
        if questions.is_empty() {
            // Nothing answerable: delegating is the honest answer, and a card
            // with no options would be a card nobody can act on.
            tracing::debug!("dsh: a question batch arrived with no questions");
            return;
        }
        let title = questions
            .first()
            .map(|question| question.text.clone())
            .unwrap_or_else(|| "question".to_string());
        let interaction = Interaction {
            id: frame.id.clone(),
            session: frame
                .request
                .get("agent")
                .and_then(|agent| agent.get("id"))
                .and_then(Value::as_str)
                .map(|id| SessionKey::new(EngineId::Dsh, id)),
            kind: InteractionKind::Question,
            title,
            detail: None,
            requested_at: now_ms(),
            questions,
        };
        self.inner()
            .interactions
            .insert(interaction.id.clone(), interaction.clone());
        self.emit(EngineEvent::InteractionRequested(interaction));
    }

    fn on_notice(&self, frame: NoticeFrame) {
        self.emit(EngineEvent::Notice {
            level: frame.level,
            message: frame.message,
        });
    }

    /// Re-read the session list and announce the difference.
    ///
    /// Retried while the host is still composing: a plugin attaches as soon as
    /// it can dial, which can be before the services behind the endpoints exist.
    async fn refresh_sessions(&self) -> Result<Vec<Session>, EngineError> {
        let deadline = tokio::time::Instant::now() + COMPOSE_GRACE;
        let mut wait = Duration::from_millis(250);
        // Longer than the default: the first list of a few hundred sessions is
        // the slowest call this plugin makes, and a timeout there looks like a
        // broken host rather than a big list.
        const LIST_TIMEOUT: Duration = Duration::from_secs(90);
        let value = loop {
            match self
                .call_with("session/list", json!({ "_request": {} }), LIST_TIMEOUT)
                .await
            {
                Ok(value) => break value,
                Err(error) if is_not_composed(&error) && tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(Duration::from_secs(2));
                }
                Err(error) => return Err(error),
            }
        };
        let items = value
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let parsed: Vec<Session> = items.iter().filter_map(parse_session).collect();
        tracing::info!(
            "dsh: session/list answered {} rows, {} parsed",
            items.len(),
            parsed.len()
        );
        if !items.is_empty() && parsed.is_empty() {
            // The shape changed, or we are reading the wrong keys. One row is
            // enough to say which.
            tracing::warn!("dsh: first row looked like {}", items[0]);
        }
        let mut removed: Vec<SessionKey> = Vec::new();
        {
            let mut inner = self.inner();
            for session in &parsed {
                inner
                    .sessions
                    .insert(session.key.id.clone(), session.clone());
            }
            let known: Vec<String> = inner.sessions.keys().cloned().collect();
            for id in known {
                if !parsed.iter().any(|session| session.key.id == id) {
                    inner.sessions.remove(&id);
                    inner.records.remove(&id);
                    removed.push(SessionKey::new(EngineId::Dsh, id));
                }
            }
        }

        for session in &parsed {
            self.emit(EngineEvent::SessionUpdated(session.clone()));
        }
        for key in removed {
            self.emit(EngineEvent::SessionRemoved(key));
        }

        // Then, in the background, give the newest rows the things a list needs
        // to be readable. Sequentially and slowly: three hundred projection reads
        // at once would be a stampede on the host for data nobody has scrolled to.
        if let Some(engine) = self.me.upgrade() {
            let newest: Vec<String> = parsed
                .iter()
                .take(ENRICH_LIMIT)
                .map(|session| session.key.id.clone())
                .collect();
            tokio::spawn(async move {
                for id in newest {
                    if !engine.connected() {
                        return;
                    }
                    engine.enrich(&id).await;
                    tokio::time::sleep(ENRICH_SPACING).await;
                }
            });
        }
        Ok(parsed)
    }

    /// Read one page from DSH: the log cut from `follow`, then `page` below it.
    async fn fetch_page(
        &self,
        session: &SessionKey,
        limit: usize,
        before: Option<i64>,
    ) -> Result<Vec<Record>, EngineError> {
        let address = json!({ "kind": "session", "sessionId": session.raw() });
        let opening = self
            .stream_opening(
                "session/follow",
                json!({ "request": { "address": address } }),
            )
            .await?;

        // The cut is `cursor` on a snapshot frame; `throughSeq` is accepted too
        // because that is what the same number is called on the page request.
        let cut = opening
            .get("cursor")
            .and_then(Value::as_i64)
            .or_else(|| opening.get("throughSeq").and_then(Value::as_i64));
        let Some(cut) = cut else {
            return Ok(Vec::new());
        };

        let mut request = json!({
            "address": address,
            "throughSeq": cut,
            "maxMessages": limit,
        });
        if let Some(before) = before {
            request["beforeSeq"] = json!(before);
        }
        let page = self
            .call("session/page", json!({ "request": request }))
            .await?;

        let raw = page
            .get("records")
            .or_else(|| page.get("items"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let records: Vec<Record> = raw
            .iter()
            .filter_map(projection::project)
            .filter_map(|projected| projected.record())
            .collect();

        // A page read is the best cache seed there is: it is the same records the
        // firehose would have produced, for a session we may have joined late.
        {
            let mut inner = self.inner();
            let bucket = inner.records.entry(session.id.clone()).or_default();
            for record in &records {
                if !bucket.iter().any(|cached| cached.seq == record.seq) {
                    bucket.push_back(record.clone());
                }
            }
            while bucket.len() > MAX_CACHED_RECORDS {
                bucket.pop_front();
            }
        }

        Ok(records)
    }

    /// Fill in what `session/list` does not carry: the title, the model, and how
    /// full the context window is.
    ///
    /// `session/list` answers identity and timing; everything a person recognises
    /// a session by lives in its projections. Reading them costs one call per
    /// session, so it happens for the rows somebody can actually see rather than
    /// for all three hundred at attach.
    async fn enrich(&self, session_id: &str) {
        let value = match self
            .call(
                "session/projections",
                json!({ "request": { "sessionId": session_id } }),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                tracing::debug!("dsh: no projections for {session_id} — {error}");
                return;
            }
        };
        let Some(values) = value.get("values") else {
            return;
        };

        let title = values
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|title| !title.trim().is_empty());
        // `modelSelection` is `{lastUsed, next}`, each `{provider, model}`: the
        // model in force and the one the next turn would use. Reading the wrong
        // level is why every row showed no model at all.
        let model = values
            .get("modelSelection")
            .and_then(|selection| {
                selection
                    .get("lastUsed")
                    .or_else(|| selection.get("next"))
                    .or(Some(selection))
            })
            .and_then(|selection| selection.get("model"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let turns = values
            .get("sessionStats")
            .and_then(|stats| stats.get("turns"))
            .and_then(Value::as_u64)
            .and_then(|turns| u32::try_from(turns).ok());
        let context = values.get("contextPressure").and_then(read_pressure);

        if title.is_none() && model.is_none() && context.is_none() {
            return;
        }
        let updated = {
            let mut inner = self.inner();
            let Some(session) = inner.sessions.get_mut(session_id) else {
                return;
            };
            if title.is_some() {
                session.title = title;
            }
            if model.is_some() {
                session.model = model;
            }
            if context.is_some() {
                session.context = context;
            }
            if let Some(turns) = turns {
                session.turns = turns;
            }
            session.clone()
        };
        self.emit(EngineEvent::SessionUpdated(updated));
    }
}

/// Read a `user-questions/request` payload into the shared model.
///
/// The host's shape is flat and has no call id: `{questions: [...], agent: {...}}`.
/// A batch with no usable question is empty, which the caller treats as "nothing
/// this layer can answer" and delegates.
fn read_questions(request: &Value) -> Vec<Question> {
    let raw = request
        .get("questions")
        .or_else(|| {
            request
                .get("request")
                .and_then(|inner| inner.get("questions"))
        })
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    raw.iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(Value::as_str)?;
            if id.is_empty() {
                return None;
            }
            let options = entry
                .get("options")
                .and_then(Value::as_array)
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|option| option.get("label").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Some(Question {
                id: id.to_string(),
                text: entry
                    .get("question")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                header: entry
                    .get("header")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                detail: entry
                    .get("detail")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                options,
                multi_select: entry
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// Read a `contextPressure` projection value.
///
/// Every field is optional on the host's side: a session that has not run has no
/// reading, and a partial one is still worth drawing.
fn read_pressure(value: &Value) -> Option<ContextPressure> {
    let window = value.get("contextWindow").and_then(Value::as_u64)?;
    if window == 0 {
        return None;
    }
    let used = value
        .get("pressureTokens")
        .and_then(Value::as_u64)
        .or_else(|| value.get("projectedTokens").and_then(Value::as_u64))
        .unwrap_or(0);
    Some(ContextPressure {
        window,
        used,
        percent: (used as f64 / window as f64) * 100.0,
    })
}

/// Whether a failure is "the host is still composing" rather than a real error.
fn is_not_composed(error: &EngineError) -> bool {
    match error {
        EngineError::Upstream { code, message, .. } => {
            code.contains("service-unavailable")
                || code.contains("invocation-unavailable")
                || message.contains("is unavailable")
        }
        // A call that has not answered yet is not a call that failed: the host is
        // still bringing the service up, and the next attempt may well work.
        EngineError::Timeout(_) => true,
        _ => false,
    }
}

/// Read one row of `session/list` into the shared model.
fn parse_session(value: &Value) -> Option<Session> {
    let id = value
        .get("sessionId")
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)?;
    let updated_at = value
        .get("updatedAt")
        .and_then(Value::as_i64)
        .unwrap_or_else(now_ms);

    Some(Session {
        key: SessionKey::new(EngineId::Dsh, id),
        cwd: value.get("cwd").and_then(Value::as_str).map(str::to_string),
        title: value
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string),
        model: value.get("model").and_then(|model| match model {
            Value::String(text) => Some(text.clone()),
            Value::Object(map) => map.get("model").and_then(Value::as_str).map(str::to_string),
            _ => None,
        }),
        updated_at,
        running: value
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        parent: value
            .get("parentSessionId")
            .and_then(Value::as_str)
            .map(str::to_string),
        origin: value
            .get("origin")
            .and_then(Value::as_str)
            .map(str::to_string),
        turns: value
            .get("turns")
            .and_then(Value::as_u64)
            .and_then(|turns| u32::try_from(turns).ok())
            .unwrap_or(0),
        // `session/list` carries none of these; `enrich` fills them in.
        context: None,
    })
}

/// Epoch milliseconds, without pulling in a date library for one call.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[async_trait]
impl Engine for DshEngine {
    fn id(&self) -> EngineId {
        EngineId::Dsh
    }

    fn capabilities(&self) -> Capabilities {
        // What the 0.9.0 plugin can actually do today. Attachments, fork, archive
        // and file preview are passthroughs that exist in DSH but are not wired
        // here yet; a capability that is off hides a button instead of failing a
        // click.
        Capabilities {
            attachments: false,
            file_preview: false,
            fork: false,
            archive: false,
            activity: true,
            balance: false,
            models: true,
            permissions: true,
        }
    }

    fn status(&self) -> (bool, Option<String>) {
        let inner = self.inner();
        (inner.out.is_some(), inner.detail.clone())
    }

    async fn sessions(&self) -> Result<Vec<Session>, EngineError> {
        if !self.connected() {
            return Err(EngineError::NotConnected {
                engine: EngineId::Dsh,
            });
        }
        self.refresh_sessions().await
    }

    async fn records(
        &self,
        session: &SessionKey,
        limit: usize,
        before: Option<i64>,
    ) -> Result<Vec<Record>, EngineError> {
        if session.engine != EngineId::Dsh {
            return Err(EngineError::UnknownSession(session.clone()));
        }

        let cached: Vec<Record> = {
            let inner = self.inner();
            inner
                .records
                .get(&session.id)
                .map(|bucket| {
                    // `Filter` is not `ExactSizeIterator`, so a double-ended
                    // take-and-reverse does not compile; taking the tail of a
                    // collected window says the same thing more plainly.
                    let mut window: Vec<Record> = bucket
                        .iter()
                        .filter(|record| before.is_none_or(|cut| record.seq < cut))
                        .cloned()
                        .collect();
                    let start = window.len().saturating_sub(limit);
                    window.split_off(start)
                })
                .unwrap_or_default()
        };
        if !cached.is_empty() {
            return Ok(cached);
        }

        if !self.connected() {
            return Err(EngineError::NotConnected {
                engine: EngineId::Dsh,
            });
        }
        self.fetch_page(session, limit, before).await
    }

    async fn prompt(&self, session: &SessionKey, text: &str) -> Result<(), EngineError> {
        if session.engine != EngineId::Dsh {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        let request_id = format!("agi-{}", uuid::Uuid::new_v4());
        self.call(
            "session/prompt",
            json!({
                "request": {
                    "requestId": request_id,
                    "sessionId": session.raw(),
                    "mode": "queue",
                    "content": [{ "type": "text", "text": text }],
                }
            }),
        )
        .await?;
        Ok(())
    }

    async fn cancel(&self, session: &SessionKey) -> Result<(), EngineError> {
        if session.engine != EngineId::Dsh {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        self.call(
            "session/cancel",
            json!({ "request": { "sessionId": session.raw() } }),
        )
        .await?;
        Ok(())
    }

    async fn answer(&self, id: &str, answer: Answer) -> Result<(), EngineError> {
        let kind = {
            let inner = self.inner();
            inner
                .interactions
                .get(id)
                .map(|interaction| interaction.kind)
        };
        let Some(kind) = kind else {
            return Err(EngineError::upstream(format!(
                "no pending interaction {id}"
            )));
        };

        let frame = match kind {
            InteractionKind::Approval => {
                let outcome = answer.outcome.unwrap_or_else(|| "rejected".to_string());
                DesktopFrame::approval_answer(id, outcome)
            }
            InteractionKind::Question => {
                DesktopFrame::question_answer(id, answer.answers.unwrap_or_default())
            }
        };
        self.send(frame).await?;
        self.inner().interactions.remove(id);
        self.emit(EngineEvent::InteractionResolved { id: id.to_string() });
        Ok(())
    }

    async fn set_model(&self, session: &SessionKey, model: &str) -> Result<(), EngineError> {
        if session.engine != EngineId::Dsh {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        // DSH selects by provider *and* model, and its catalog is the only place
        // that pair comes from. The shared model carries one string, so it is
        // `provider/model`; a bare model cannot be resolved here without guessing.
        let Some((provider, name)) = model.split_once('/') else {
            return Err(EngineError::upstream("a DSH model is named provider/model"));
        };
        self.call(
            "session/selectModel",
            json!({
                "request": {
                    "sessionId": session.raw(),
                    "provider": provider,
                    "model": name,
                }
            }),
        )
        .await?;
        Ok(())
    }

    async fn set_permission(&self, session: &SessionKey, preset: &str) -> Result<(), EngineError> {
        if session.engine != EngineId::Dsh {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        // DSH has no Remote setter for a preset; the switch is a command.
        let value = self
            .call(
                "commands/execute",
                json!({
                    "agentId": session.raw(),
                    "line": format!("/permission {preset}"),
                    "submittedAttachments": [],
                }),
            )
            .await?;
        let kind = value
            .get("result")
            .and_then(|result| result.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("success");
        if kind == "error" {
            let text = value
                .get("result")
                .and_then(|result| result.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("the preset was refused");
            return Err(EngineError::upstream(text));
        }
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }
}
