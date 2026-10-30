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
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use agi_engine::{Answer, Engine, EngineError, EngineEvent};
use agi_protocol::domain::{
    ActivityEntry, ActivityKind, Balance, ContextPressure, DeliveredFile, Interaction,
    InteractionKind, ModelInfo, PermissionPreset, Question, Record, RecordKind, Session,
    SessionKey, SessionStats, Wallet, Workspace,
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

/// How long a folded usage history is reused.
///
/// Folding it means reading a page of records for every session, which on a host
/// with three hundred of them is the most expensive thing this adapter does. The
/// home page asks again whenever it draws, and nobody needs second-level
/// freshness on "how much did I do in September".
const USAGE_CACHE_TTL: Duration = Duration::from_secs(60);

/// How long a short fold may answer other callers, so one burst of asks is one
/// pass over the logs.
const USAGE_ATTEMPT_TTL: Duration = Duration::from_secs(5);

/// The fold written for the next launch, and when it was made.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct UsageCacheFile {
    folded_at: i64,
    days: Vec<agi_protocol::api::UsageDay>,
}

impl UsageCacheFile {
    /// Age in milliseconds; a clock that went backwards reads as fresh.
    fn age_ms(&self) -> i64 {
        now_ms().saturating_sub(self.folded_at).max(0)
    }
}

/// Epoch milliseconds.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

/// How many messages one usage page asks for.
///
/// `session/page` is a page, not a stream. Measured on this host, a page of two
/// thousand messages covers a whole session for all but three of 312, and the
/// longest is 4675 messages.
/// Records asked for per page while folding usage.
///
/// A page is one WebSocket frame, and a streamed assistant message carries a few
/// kilobytes of stream chunks: two thousand of those is a seventeen-megabyte
/// frame, past the transport's limit, and the connection dies mid-fold. Five
/// hundred keeps the frame small and costs a few more round trips.
const USAGE_PAGE: usize = 500;

/// How many pages one session's usage fold reads before it gives up.
///
/// Eight pages is 16 thousand messages, four times the longest session measured
/// here, and the cap is what stops a corrupted `beforeSeq` from turning one fold
/// into an unbounded walk.
const USAGE_PAGES_MAX: usize = 24;

/// How many sessions one usage fold reads.
///
/// The chart is a year of days, and the host's own list is what fills them.
/// Sorting newest first means a machine with more sessions than this loses the
/// oldest ones, which are outside a 365-day heatmap in practice — and the cap is
/// what keeps one window opening from spending a minute on the link.
const USAGE_SESSION_LIMIT: usize = 600;

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
    /// The folded usage history, and when it was folded.
    ///
    /// Kept on the engine rather than in a task because the only thing that
    /// invalidates it is time: a record that arrives after a fold is at most a
    /// minute of history, and the fold it belongs to is one request away.
    usage_cache: Mutex<Option<(std::time::Instant, usize, Vec<agi_protocol::api::UsageDay>)>>,
    /// A weak handle to self, so `refresh_sessions(&self)` can start a task that
    /// outlives the call. `Arc::new_cyclic` fills it in after construction.
    me: Weak<DshEngine>,
    /// Where the folded history is kept between launches, when somebody set it.
    usage_cache_path: Mutex<Option<PathBuf>>,
    /// One fold at a time. Two windows asking at once should cost one pass over
    /// the logs, not two.
    usage_fold_lock: tokio::sync::Mutex<()>,
    /// The last fold that came back short, with when it was made.
    ///
    /// Incomplete folds are not cached for a minute — but they are worth reusing
    /// for a few seconds, or a burst of requests after one expensive pass folds
    /// the same three hundred sessions again each time.
    usage_attempt: Mutex<Option<(std::time::Instant, Vec<agi_protocol::api::UsageDay>)>>,
    /// One background refresh at a time.
    usage_refreshing: std::sync::atomic::AtomicBool,
    /// Tells the socket pump to drop the plugin's connection.
    ///
    /// The desktop does not dial the plugin — the plugin dials in — so the only
    /// way to make it reconnect is to hang up on it and let its own retry loop
    /// bring it back.
    kick: broadcast::Sender<()>,
}

impl DshEngine {
    /// Build an engine that accepts a plugin presenting `token`.
    pub fn new(token: impl Into<String>) -> Arc<Self> {
        let (events, _) = broadcast::channel(512);
        let (kick, _) = broadcast::channel(4);
        let mut inner = Inner::new();
        // Said out loud from the first snapshot: an engine with no detail reads
        // like an engine that failed, not one that is waiting.
        inner.detail = Some("no plugin attached".to_string());
        Arc::new_cyclic(|me| Self {
            token: token.into(),
            events,
            inner: Mutex::new(inner),
            next_id: AtomicU64::new(1),
            usage_cache: Mutex::new(None),
            usage_cache_path: Mutex::new(None),
            usage_fold_lock: tokio::sync::Mutex::new(()),
            usage_attempt: Mutex::new(None),
            usage_refreshing: std::sync::atomic::AtomicBool::new(false),
            me: me.clone(),
            kick,
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

    /// Keep the folded history in this file, so the first window of the next
    /// launch does not start from zero while hundreds of sessions are re-read.
    pub fn set_usage_cache_path(&self, path: PathBuf) {
        *self
            .usage_cache_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
    }

    /// Watch for a request to hang up on the plugin.
    pub fn kick_receiver(&self) -> broadcast::Receiver<()> {
        self.kick.subscribe()
    }

    /// Forget the current link after the socket pump hung up on it.
    ///
    /// `serve` cannot run its own cleanup when the pump drops its future, so the
    /// pump calls this instead: without it the engine keeps claiming a
    /// connection whose writer is gone, and folds an empty history into the
    /// cache for the next minute.
    pub fn forget_link(&self) {
        *self
            .usage_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.detach();
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

    /// Call an in-process service the desktop is allowed to reach.
    async fn call_service(
        &self,
        service: &str,
        method: &str,
        args: Vec<Value>,
    ) -> Result<Value, EngineError> {
        let id = self.next_id();
        self.request_with(
            DesktopFrame::service(id.clone(), service, method, args),
            id,
            REQUEST_TIMEOUT,
        )
        .await
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

    /// Read one raw page from DSH: the log cut from `follow`, then `page` below
    /// it, as the plugin sent them.
    ///
    /// Untouched records, because two callers need different halves of one: the
    /// conversation view reads what the projection keeps, and the usage fold
    /// reads the provider's `usage` object it drops.
    async fn raw_page(
        &self,
        session: &SessionKey,
        limit: usize,
        before: Option<i64>,
    ) -> Result<Vec<Value>, EngineError> {
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

        Ok(page
            .get("records")
            .or_else(|| page.get("items"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Read one page of records for the conversation view.
    async fn fetch_page(
        &self,
        session: &SessionKey,
        limit: usize,
        before: Option<i64>,
    ) -> Result<Vec<Record>, EngineError> {
        let raw = self.raw_page(session, limit, before).await?;

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

    /// Read one projection snapshot for a session.
    async fn projections(&self, session: &SessionKey) -> Result<Value, EngineError> {
        let value = self
            .call(
                "session/projections",
                json!({ "request": { "sessionId": session.raw() } }),
            )
            .await?;
        Ok(value.get("values").cloned().unwrap_or(Value::Null))
    }

    /// A short fold from the last few seconds, if there was one.
    fn recent_attempt(&self) -> Option<Vec<agi_protocol::api::UsageDay>> {
        let attempt = self
            .usage_attempt
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match attempt.as_ref() {
            Some((at, days)) if at.elapsed() < USAGE_ATTEMPT_TTL => Some(days.clone()),
            _ => None,
        }
    }

    /// The last fold written to disk, with how old it is.
    fn read_usage_cache(&self) -> Option<(Vec<agi_protocol::api::UsageDay>, i64)> {
        let path = self
            .usage_cache_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()?;
        let text = std::fs::read_to_string(path).ok()?;
        let stored: UsageCacheFile = serde_json::from_str(&text).ok()?;
        let age = stored.age_ms();
        Some((stored.days, age))
    }

    /// Write one complete fold for the next launch.
    fn write_usage_cache(&self, days: &[agi_protocol::api::UsageDay]) {
        let path = self
            .usage_cache_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(path) = path else {
            return;
        };
        let stored = UsageCacheFile {
            folded_at: now_ms(),
            days: days.to_vec(),
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_vec(&stored) {
            let _ = std::fs::write(path, text);
        }
    }

    /// Fold again in the background, at most once at a time.
    fn refresh_usage_in_background(&self) {
        use std::sync::atomic::Ordering;
        if self.usage_refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(engine) = self.me.upgrade() else {
            self.usage_refreshing
                .store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        };
        tokio::spawn(async move {
            let _fold = engine.usage_fold_lock.lock().await;
            // A foreground fold may have finished while this one waited for the
            // lock; folding again would be the duplicate the lock exists to stop.
            if engine.cached_usage().is_some() || engine.recent_attempt().is_some() {
                engine.usage_refreshing.store(false, Ordering::SeqCst);
                return;
            }
            if let Ok((days, complete)) = engine.fold_usage().await {
                if complete {
                    engine.write_usage_cache(&days);
                    let listed = engine.inner().sessions.len();
                    *engine
                        .usage_cache
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                        Some((std::time::Instant::now(), listed, days));
                }
            }
            engine.usage_refreshing.store(false, Ordering::SeqCst);
        });
    }

    /// The folded usage history, when it is still fresh *and* was folded from
    /// as many sessions as the engine knows about now.
    ///
    /// The second half matters at startup: the dashboard is the first screen,
    /// and it asks before the session list has finished arriving. A fold made
    /// from four sessions is not an answer for a host with eighty-one, and
    /// caching it for a minute would show a wrong number for a minute.
    fn cached_usage(&self) -> Option<Vec<agi_protocol::api::UsageDay>> {
        let listed = self.inner().sessions.len();
        let cache = self
            .usage_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match cache.as_ref() {
            Some((folded_at, folded_listed, days))
                if folded_at.elapsed() < USAGE_CACHE_TTL && *folded_listed == listed =>
            {
                Some(days.clone())
            }
            _ => None,
        }
    }

    /// Fold every session's records into days of activity.
    ///
    /// Newest session first, so a host with more sessions than
    /// [`USAGE_SESSION_LIMIT`] spends its budget on the ones whose days the chart
    /// actually shows.
    ///
    /// A delegated child's session is folded too: the operator did not start it,
    /// but its turns ran on the same account, and the chart is a picture of work
    /// rather than of clicks.
    async fn fold_usage(&self) -> Result<(Vec<agi_protocol::api::UsageDay>, bool), EngineError> {
        // Ask the engine for its own list rather than reading whatever has been
        // folded into memory so far. The dashboard is the first screen, and the
        // in-memory map fills progressively while a few hundred sessions are
        // enriched: folding it early is how a host with eighty-one sessions
        // answered "four" and cached that for a minute.
        let mut sessions: Vec<Session> = self.sessions().await?;
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        sessions.truncate(USAGE_SESSION_LIMIT);

        let mut days = agi_engine::UsageDays::new();
        // A fold that could not read every session is an answer for now, not for
        // the next minute: caching it is how the dashboard showed a quarter of
        // the history until the cache expired.
        let mut every_session_read = true;
        let mut failed = 0usize;
        let mut truncated = 0usize;
        let mut unreadable = 0usize;
        for session in &sessions {
            // One session that cannot be paged must not blank the whole chart.
            let mut usage = crate::history::SessionUsage::default();
            let mut before: Option<i64> = None;
            let mut complete = false;
            for _ in 0..USAGE_PAGES_MAX {
                let records = match self.raw_page(&session.key, USAGE_PAGE, before).await {
                    Ok(records) => records,
                    Err(error) => {
                        // A session the host will not open a stream for is not a
                        // failed read: subagent sessions and archived ones answer
                        // exactly this, and counting them as failures keeps the
                        // whole fold out of the cache forever.
                        if is_unaddressable(&error) {
                            unreadable += 1;
                            complete = true;
                        } else {
                            failed += 1;
                        }
                        tracing::debug!("dsh: no records for {} — {error}", session.key);
                        break;
                    }
                };
                if records.is_empty() {
                    complete = true;
                    break;
                }
                usage.absorb(&mut days, &records, session.model.as_deref());

                // A short page is the start of the history: there is nothing
                // below it to read.
                if records.len() < USAGE_PAGE {
                    complete = true;
                    break;
                }
                // Page backwards from the oldest record this page carried; a
                // cursor that does not move would loop forever.
                let oldest = records.iter().filter_map(record_seq).min();
                if oldest.is_none() || oldest == before {
                    break;
                }
                before = oldest;
            }
            if !complete {
                truncated += 1;
                every_session_read = false;
                // Said once per fold, because a silent truncation reads as a
                // cheaper month than the operator had.
                tracing::debug!(
                    "dsh: {} is longer than {} pages; its oldest usage is not in this fold",
                    session.key,
                    USAGE_PAGES_MAX
                );
            }
            usage.finish(&mut days);
        }
        if !every_session_read || failed > 0 {
            tracing::warn!(
                "dsh: usage fold incomplete — {failed} failed, {truncated} truncated, of {}",
                sessions.len()
            );
        }
        if unreadable > 0 {
            tracing::debug!("dsh: {unreadable} sessions the host would not open");
        }
        Ok((days.finish(), every_session_read))
    }
}

/// Whether an error means "this session has no stream to read" rather than
/// "reading it failed".
fn is_unaddressable(error: &EngineError) -> bool {
    let text = error.to_string();
    text.contains("stream closed before its opening frame")
        || text.contains("NOT_FOUND")
        || text.contains("UNKNOWN_SESSION")
}

/// The log sequence number a page record carries, wrapped or bare.
fn record_seq(record: &Value) -> Option<i64> {
    projection::event_of(record)?.get("seq")?.as_i64()
}

/// Read a list of `{currency, balance}` rows.
/// Read a wallet list, or `None` when the provider sent something malformed.
///
/// Dropping a bad row would turn "this answer is not trustworthy" into "the
/// account has one fewer currency", which is a different claim.
fn read_wallets(value: Option<&Value>, required: bool) -> Option<Vec<Wallet>> {
    let rows = value?.as_array()?;
    if required && rows.is_empty() {
        return None;
    }
    rows.iter()
        .map(|row| {
            Some(Wallet {
                currency: row.get("currency")?.as_str()?.to_string(),
                amount: row.get("balance")?.as_str()?.to_string(),
            })
        })
        .collect()
}

/// The first non-empty line, cut to something a feed can show.
fn first_line(text: Option<&str>) -> String {
    let line = text
        .unwrap_or_default()
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= 160 {
        line.to_string()
    } else {
        line.chars().take(157).collect::<String>() + "…"
    }
}

/// A short stable suffix for an activity id.
fn kind_slug(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Thinking => "think",
        ActivityKind::Command => "cmd",
        ActivityKind::Output => "out",
        ActivityKind::Tool => "tool",
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
    // Projected first, matching DSH's own client and the phone's usage frame:
    // the two readings reach the phone through different calls, and a figure
    // that changes depending on which one arrived last is a figure nobody
    // trusts.
    let used = value
        .get("projectedTokens")
        .and_then(Value::as_u64)
        .or_else(|| value.get("pressureTokens").and_then(Value::as_u64))
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

    async fn reconnect(&self) -> Result<(), EngineError> {
        {
            let mut inner = self.inner();
            inner.detail = Some("reconnecting".to_string());
        }
        // Nobody listening means nobody attached, which is already the state the
        // caller wanted; the plugin's own retry loop will attach when it can.
        let _ = self.kick.send(());
        Ok(())
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

    /// Every day of activity this host has recorded, folded from the sessions it
    /// lists.
    ///
    /// The records are read raw rather than projected: each `assistant/message`
    /// carries the provider's own `data.usage`, and that is the only place a
    /// token count can be placed on the day it was spent. [`crate::history`]
    /// documents the shape and the fold's rules.
    ///
    /// The fold is cached for [`USAGE_CACHE_TTL`] because it pages every session
    /// it can see — on a host with three hundred sessions this is the most
    /// expensive thing the adapter does, and nobody needs second-level freshness
    /// on "how much did I do in September".
    async fn usage_history(&self) -> Result<Vec<agi_protocol::api::UsageDay>, EngineError> {
        // Answers that do not need the link come first: the window opens before
        // the plugin dials in, and a stored fold is as true then as it was when
        // it was written.
        if let Some(days) = self.cached_usage() {
            return Ok(days);
        }
        if let Some((days, age)) = self.read_usage_cache() {
            if self.connected() && age > USAGE_CACHE_TTL.as_millis() as i64 {
                self.refresh_usage_in_background();
            }
            return Ok(days);
        }
        if !self.connected() {
            return Err(EngineError::NotConnected {
                engine: EngineId::Dsh,
            });
        }
        if let Some(days) = self.recent_attempt() {
            return Ok(days);
        }

        // One fold at a time: a second caller waits here and then answers from
        // the attempt cache above instead of paging every session again.
        let _fold = self.usage_fold_lock.lock().await;
        if let Some(days) = self.cached_usage() {
            return Ok(days);
        }
        if let Some(days) = self.recent_attempt() {
            return Ok(days);
        }
        let (days, complete) = self.fold_usage().await?;
        if complete {
            self.write_usage_cache(&days);
            let listed = self.inner().sessions.len();
            let mut cache = self
                .usage_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *cache = Some((std::time::Instant::now(), listed, days.clone()));
        } else {
            *self
                .usage_attempt
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some((std::time::Instant::now(), days.clone()));
        }
        Ok(days)
    }

    fn supports_attachments(&self) -> bool {
        true
    }

    async fn prompt(&self, session: &SessionKey, text: &str) -> Result<(), EngineError> {
        self.prompt_with(session, text, &[]).await
    }

    async fn prompt_with(
        &self,
        session: &SessionKey,
        text: &str,
        attachments: &[agi_protocol::domain::PromptAttachment],
    ) -> Result<(), EngineError> {
        if session.engine != EngineId::Dsh {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        let mut content = Vec::new();
        if !text.trim().is_empty() {
            content.push(json!({ "type": "text", "text": text }));
        }
        for attachment in attachments {
            match attachment.kind {
                agi_protocol::domain::AttachmentKind::Image => {
                    // A photograph travels inline: DSH turns the bytes into a
                    // durable reference of its own, and the phone never needs a
                    // receipt for one.
                    content.push(json!({
                        "type": "image",
                        "data": agi_protocol::b64::encode(&attachment.bytes),
                        "mediaType": attachment.media_type,
                        "name": attachment.name,
                    }));
                }
                agi_protocol::domain::AttachmentKind::File => {
                    // The fallback the plugin used when the host has no
                    // `fileUploads`: a path the agent's own tools can open.
                    let path = attachment.path.clone().unwrap_or_default();
                    content.push(json!({
                        "type": "text",
                        "text": format!("[File: {}]\n{}", attachment.name, path),
                    }));
                }
            }
        }
        if content.is_empty() {
            return Err(EngineError::upstream(
                "a prompt needs text or an attachment",
            ));
        }
        let request_id = format!("agi-{}", uuid::Uuid::new_v4());
        self.call(
            "session/prompt",
            json!({
                "request": {
                    "requestId": request_id,
                    "sessionId": session.raw(),
                    "mode": "queue",
                    "content": content,
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

    async fn activity(
        &self,
        session: &SessionKey,
        limit: usize,
    ) -> Result<Vec<ActivityEntry>, EngineError> {
        // Built from the record cache rather than a second endpoint: the records
        // already carry the tool calls and their output, and a live feed that
        // disagrees with the transcript underneath it is worse than none.
        let records = {
            let inner = self.inner();
            inner.records.get(&session.id).cloned().unwrap_or_default()
        };
        let Some(latest) = records.iter().filter_map(|record| record.turn).max() else {
            return Ok(Vec::new());
        };

        let mut entries: Vec<ActivityEntry> = records
            .iter()
            .filter(|record| record.turn == Some(latest))
            .filter_map(|record| {
                let tool = record.tool.as_ref()?;
                let kind = match record.kind {
                    RecordKind::ToolCall => {
                        if matches!(tool.name.as_str(), "bash" | "shell" | "exec" | "run") {
                            ActivityKind::Command
                        } else {
                            ActivityKind::Tool
                        }
                    }
                    RecordKind::ToolResult => ActivityKind::Output,
                    _ => return None,
                };
                let text = match kind {
                    ActivityKind::Output => first_line(tool.output.as_deref()),
                    _ => first_line(tool.arguments.as_deref()),
                };
                Some(ActivityEntry {
                    id: format!("{}-{}", record.seq, kind_slug(kind)),
                    seq: record.seq,
                    at: record.time,
                    kind,
                    title: if tool.name.is_empty() {
                        "output".to_string()
                    } else {
                        tool.name.clone()
                    },
                    text,
                    turn: record.turn,
                })
            })
            .collect();

        // Only the tail is worth showing: the feed answers "what now", not "what
        // ever".
        if entries.len() > limit {
            entries.drain(..entries.len() - limit);
        }
        Ok(entries)
    }

    async fn stats(&self, session: &SessionKey) -> Result<SessionStats, EngineError> {
        let values = self.projections(session).await?;
        let mut stats = SessionStats::default();
        if let Some(summary) = values.get("sessionStats") {
            stats.turns = summary.get("turns").and_then(Value::as_u64).unwrap_or(0) as u32;
            stats.steps = summary.get("steps").and_then(Value::as_u64).unwrap_or(0) as u32;
            stats.llm_ms = summary.get("llmMs").and_then(Value::as_i64).unwrap_or(0);
            stats.tool_ms = summary.get("toolMs").and_then(Value::as_i64).unwrap_or(0);
        }
        if let Some(usage) = values.get("tokenUsage") {
            // The bug this replaced was silent for months: the keys this build
            // read did not exist, and a missing number is indistinguishable from
            // a zero one. If DSH renames a bucket again, say so once per read
            // rather than drawing a plausible zero.
            let known = [
                "uncachedInputTokens",
                "cacheReadTokens",
                "cacheWriteTokens",
                "outputTokens",
            ];
            if usage.is_object() && !known.iter().any(|key| usage.get(key).is_some()) {
                tracing::warn!(
                    "dsh: tokenUsage carries none of the keys this build reads — {usage}"
                );
            }
            let (input, output) = projection::token_usage(usage);
            stats.input_tokens = input;
            stats.output_tokens = output;
        }
        Ok(stats)
    }

    async fn turn_usage(
        &self,
        session: &SessionKey,
    ) -> Result<std::collections::HashMap<u32, u64>, EngineError> {
        // One page is enough for the footers on screen: the conversation reads
        // the newest records, and a turn older than the page is a turn the
        // operator has scrolled past. Reading the whole log to label a footer
        // would be the tail wagging the dog.
        let records = self.raw_page(session, USAGE_PAGE, None).await?;
        let mut totals = std::collections::HashMap::new();
        crate::history::fold_per_turn(&mut totals, &records, None);
        Ok(totals)
    }

    async fn usage(
        &self,
        session: &SessionKey,
    ) -> Result<agi_protocol::usage::SessionUsage, EngineError> {
        let values = self.projections(session).await?;
        Ok(crate::usage::from_projections(&values))
    }

    async fn fork(&self, session: &SessionKey, at_seq: i64) -> Result<SessionKey, EngineError> {
        if at_seq < 0 {
            return Err(EngineError::Upstream {
                code: "MALFORMED_FRAME".to_string(),
                message: "a branch boundary is a log position, not a negative number".to_string(),
                details: None,
            });
        }
        // The boundary has to be a completed turn, and this is checked here
        // rather than trusted to DSH: a stale client that remembers an old seq
        // would otherwise branch a conversation in the middle of a step, and the
        // fork would look successful.
        let page = self.records(session, 1000, Some(at_seq + 1)).await?;
        match page.iter().find(|record| record.seq == at_seq) {
            Some(record) if record.kind == RecordKind::TurnEnd => {}
            _ => {
                return Err(EngineError::Upstream {
                    code: "MALFORMED_FRAME".to_string(),
                    message: "the selected turn boundary could not be verified; refresh the conversation and try again"
                        .to_string(),
                    details: None,
                });
            }
        }
        let value = self
            .call(
                "session/fork",
                json!({ "request": { "sessionId": session.raw(), "atSeq": at_seq } }),
            )
            .await?;
        let id = value
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| EngineError::upstream("session/fork returned no session id"))?;
        Ok(SessionKey::new(session.engine, id))
    }

    async fn model_catalog(&self) -> Result<agi_protocol::catalog::ModelCatalog, EngineError> {
        // The same call the shipped web UI makes; the catalog is passed through
        // rather than curated, because `auto` appears and disappears with the
        // integration mounted and `custom` is never a selectable row.
        let value = self.call("session/modelCatalog", json!({})).await?;
        Ok(crate::catalog::model_catalog_from(&value))
    }

    async fn permission_catalog(
        &self,
    ) -> Result<agi_protocol::catalog::PermissionCatalog, EngineError> {
        let value = self.call("permissionPresets/catalog", json!({})).await?;
        Ok(crate::catalog::permission_catalog_from(&value))
    }

    async fn session_cwd(&self, session: &SessionKey) -> Result<String, EngineError> {
        if session.engine != EngineId::Dsh {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        // A file preview asks for the working directory once per slice, and
        // `sessions()` re-reads the whole list on the host every time — the
        // slowest call this engine makes. The cache is kept current by the event
        // feed, so it answers first, and the network only sees the question once.
        if let Some(cwd) = {
            let inner = self.inner();
            inner
                .sessions
                .get(&session.id)
                .and_then(|row| row.cwd.clone())
        } {
            return Ok(cwd);
        }
        let sessions = self.sessions().await?;
        sessions
            .into_iter()
            .find(|candidate| candidate.key == *session)
            .and_then(|candidate| candidate.cwd)
            .ok_or_else(|| EngineError::Upstream {
                code: "FILE_FORBIDDEN".to_string(),
                message: "this session has no working directory to read from".to_string(),
                details: None,
            })
    }

    async fn deliverables(&self, session: &SessionKey) -> Result<Vec<DeliveredFile>, EngineError> {
        // A delivered file is a `present` tool call, which is the one place the
        // model names what it made. Reading it out of the records keeps this in
        // step with the transcript.
        let records = {
            let inner = self.inner();
            inner.records.get(&session.id).cloned().unwrap_or_default()
        };
        let mut files: Vec<DeliveredFile> = Vec::new();
        for record in records
            .iter()
            .filter(|record| record.kind == RecordKind::ToolCall)
        {
            let Some(tool) = record.tool.as_ref() else {
                continue;
            };
            if !tool.name.contains("present") {
                continue;
            }
            let Some(arguments) = tool.arguments.as_deref() else {
                continue;
            };
            let Ok(parsed) = serde_json::from_str::<Value>(arguments) else {
                continue;
            };
            let Some(list) = parsed.get("files").and_then(Value::as_array) else {
                continue;
            };
            for entry in list {
                let Some(path) = entry.get("path").and_then(Value::as_str) else {
                    continue;
                };
                let name = path.rsplit('/').next().unwrap_or(path).to_string();
                if files.iter().any(|file| file.path == path) {
                    continue;
                }
                files.push(DeliveredFile {
                    path: path.to_string(),
                    name,
                    description: entry
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
        }
        Ok(files)
    }

    async fn models(&self) -> Result<Vec<ModelInfo>, EngineError> {
        // Measured shape: `{groups: [{id, models: [{id, name, reasoning:
        // {efforts: [{id, name}]}}]}]}`. The group id is the provider, and a
        // model is addressed as `provider/model`.
        let value = self.call("session/modelCatalog", json!({})).await?;
        let mut models = Vec::new();
        for group in value
            .get("groups")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let provider = group
                .get("id")
                .or_else(|| group.get("provider"))
                .and_then(Value::as_str)
                .unwrap_or("");
            for model in group
                .get("models")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                let Some(name) = model
                    .get("id")
                    .or_else(|| model.get("model"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                models.push(ModelInfo {
                    id: format!("{provider}/{name}"),
                    label: model
                        .get("name")
                        .or_else(|| model.get("label"))
                        .and_then(Value::as_str)
                        .unwrap_or(name)
                        .to_string(),
                    provider: Some(provider.to_string()),
                    reasoning: model
                        .get("reasoning")
                        .and_then(|reasoning| reasoning.get("efforts"))
                        .and_then(Value::as_array)
                        .map(|efforts| {
                            efforts
                                .iter()
                                .filter_map(|effort| match effort {
                                    Value::String(text) => Some(text.clone()),
                                    other => {
                                        other.get("id").and_then(Value::as_str).map(str::to_string)
                                    }
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                });
            }
        }
        Ok(models)
    }

    async fn permissions(&self) -> Result<Vec<PermissionPreset>, EngineError> {
        // Measured shape: `{options: [{name, value}], defaultPreset}`.
        let value = self.call("permissionPresets/catalog", json!({})).await?;
        let options = value
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(options
            .iter()
            .filter_map(|option| {
                let id = option
                    .get("value")
                    .or_else(|| option.get("id"))
                    .and_then(Value::as_str)?;
                Some(PermissionPreset {
                    id: id.to_string(),
                    label: option
                        .get("name")
                        .or_else(|| option.get("label"))
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_string(),
                    description: option
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
            })
            .collect())
    }

    async fn create(&self, cwd: Option<&str>) -> Result<SessionKey, EngineError> {
        let request = match cwd {
            Some(cwd) => json!({ "request": { "cwd": cwd } }),
            None => json!({ "request": {} }),
        };
        let value = self.call("session/create", request).await?;
        let id = value
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| EngineError::upstream("session/create returned no id"))?;
        let session = SessionKey::new(EngineId::Dsh, id);
        self.refresh_sessions().await?;
        Ok(session)
    }

    async fn rename(&self, session: &SessionKey, title: &str) -> Result<(), EngineError> {
        self.call(
            "session/rename",
            json!({ "request": { "sessionId": session.raw(), "title": title } }),
        )
        .await?;
        self.enrich(&session.id).await;
        Ok(())
    }

    async fn archive(&self, session: &SessionKey, stop_activity: bool) -> Result<(), EngineError> {
        self.call_service(
            "workspaceRegistry",
            "archiveSession",
            vec![
                json!(session.raw()),
                json!({ "stopActivity": stop_activity }),
            ],
        )
        .await?;
        {
            let mut inner = self.inner();
            inner.sessions.remove(&session.id);
            inner.records.remove(&session.id);
        }
        self.emit(EngineEvent::SessionRemoved(session.clone()));
        Ok(())
    }

    async fn balance(&self) -> Result<Balance, EngineError> {
        // The provider's own account endpoints, never the API-key REST balance
        // endpoint: the credential in this profile is the operator's session, and
        // a second credential path would be a second thing to leak.
        let state = match self.call("account/getState", json!({})).await {
            Ok(value) => value,
            // No account provider in this composition is a normal state, not a
            // failure: the panel simply has nothing to draw.
            Err(_) => return Ok(Balance::Unsupported),
        };
        match state.get("status").and_then(Value::as_str) {
            Some("signed-out") => return Ok(Balance::SignedOut),
            Some("credential-stored") => {}
            _ => return Ok(Balance::Unavailable),
        }

        let value = self
            .call(
                "account/getBalance",
                json!({
                    "client": {
                        "version": env!("CARGO_PKG_VERSION"),
                        "locale": "en",
                        "timezoneOffsetSeconds": 0,
                    }
                }),
            )
            .await?;
        if value.get("status").and_then(Value::as_str) != Some("ready") {
            // The account may have signed out between the two calls; asking once
            // more is what tells "sign in" from "try again". The ready path must
            // not do this: a second state read would let an A-to-B switch happen
            // after the balance was proven, without proving whose balance it was.
            let after = self.call("account/getState", json!({})).await?;
            return Ok(
                if after.get("status").and_then(Value::as_str) == Some("signed-out") {
                    Balance::SignedOut
                } else {
                    Balance::Unavailable
                },
            );
        }

        let (Some(wallets), Some(bonus)) = (
            read_wallets(value.get("value"), true),
            read_wallets(value.get("bonusWallets"), false),
        ) else {
            return Ok(Balance::Unavailable);
        };

        Ok(Balance::Ready {
            wallets,
            bonus,
            // The provider's own link, not a constant: a configurable platform is
            // not necessarily the official one, and the phone checks this value.
            top_up_url: state
                .get("links")
                .and_then(|links| links.get("topUpUrl"))
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    async fn workspaces(&self) -> Result<Vec<Workspace>, EngineError> {
        // Measured shape: `[{id, host, record: {path, title, sessionIds}}]` — the
        // useful half is under `record`, not on the row.
        let value = self
            .call_service("workspaceRegistry", "list", Vec::new())
            .await?;
        let list = value
            .as_array()
            .cloned()
            .or_else(|| value.get("items").and_then(Value::as_array).cloned())
            .unwrap_or_default();

        Ok(list
            .iter()
            .filter_map(|workspace| {
                let id = workspace.get("id").and_then(Value::as_str)?;
                let record = workspace.get("record").unwrap_or(workspace);
                Some(Workspace {
                    id: id.to_string(),
                    path: record
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: record
                        .get("title")
                        .or_else(|| record.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    session_ids: record
                        .get("sessionIds")
                        .and_then(Value::as_array)
                        .map(|ids| {
                            ids.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        }),
                })
            })
            .collect())
    }

    fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }
}
