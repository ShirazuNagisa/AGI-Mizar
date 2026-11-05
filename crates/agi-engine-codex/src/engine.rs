//! The Codex adapter.
//!
//! Codex is the one engine that is a *process we own*: `codex app-server` is
//! spawned, spoken to over stdio, and restarted when it dies. Everything here is
//! either a request to that child or a fold of what it volunteered.
//!
//! ### Approvals are requests, not notifications
//!
//! Codex stops a tool by sending *us* a request with an `id` and waiting. That
//! makes it the cleanest of the three engines to answer: the reply is the answer,
//! there is no waterfall to join and no broker to race. The only work is keeping
//! the JSON-RPC id until the operator decides.
//!
//! ### A dead child is a disconnect, not an error
//!
//! When the app-server exits, every cached thread view is worthless — a new
//! server has a different set of loaded threads — so the cache is dropped and the
//! supervisor spawns another. The UI sees a status change, not a stack of failed
//! calls.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use agi_engine::{Answer, Engine, EngineError, EngineEvent};
use agi_protocol::domain::{
    ActivityEntry, ActivityKind, DeliveredFile, Interaction, InteractionKind, ModelInfo,
    PermissionPreset, Question, Record, RecordKind, Session, SessionKey, Workspace,
};
use agi_protocol::engine::{Capabilities, EngineId};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::gui_state;
use crate::projection;
use crate::rpc::{AppServer, AppServerConfig, RpcEvent};

/// How many thread rows one list request asks for.
const THREAD_PAGE: u64 = 100;

/// How many turns one history read asks for.
const TURN_PAGE: u64 = 100;

/// How many models one list request asks for.
const MODEL_PAGE: u64 = 100;

/// How many records one activity feed may look back over.
const ACTIVITY_PAGE: usize = 200;

/// First restart delay, and the ceiling.
const RESTART_BASE: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(30);

/// How often to re-read `thread/list` for threads another window created.
///
/// The app-server only pushes events for turns it is running itself. A Codex
/// desktop window has its own app-server child, so its new thread would never
/// appear in this window without a poll. Five seconds is fast enough for a
/// session list and cheap enough for a local SQLite read.
const GUI_REFRESH: Duration = Duration::from_secs(5);

/// How long a session created or touched locally survives a `thread/list` that
/// does not mention it.
///
/// `thread/list` is only eventually consistent: a thread created a moment ago
/// (or a running turn's thread) can be missing from one page even though it
/// exists. Removing it then loses the `running` edge the judge listens for, and
/// re-inserting it from the next status change would lose its title and cwd.
const THREAD_REMOVAL_GRACE_MS: i64 = 10_000;

/// One approval Codex is waiting on.
struct PendingApproval {
    /// The JSON-RPC id to answer.
    json_id: Value,
    /// The method, which decides the shape of the answer.
    method: String,
    /// The request's `permissions`, echoed back when a permissions ask is granted.
    permissions: Value,
    /// For a question batch, the ids to answer, in the order asked.
    questions: Option<Vec<String>>,
}

/// One approval the Codex desktop window is waiting on.
struct GuiApproval {
    session: SessionKey,
    /// JSON-RPC ids can be strings or numbers; keep the exact value.
    request_id: Value,
    method: String,
    /// Question ids for `item/tool/requestUserInput`, in the order asked.
    questions: Option<Vec<String>>,
}

struct State {
    connected: bool,
    detail: Option<String>,
    sessions: HashMap<String, Session>,
    /// What the app-server said about itself at handshake.
    version: Option<String>,
}

impl State {
    fn new() -> Self {
        Self {
            connected: false,
            detail: Some("starting codex app-server".to_string()),
            sessions: HashMap::new(),
            version: None,
        }
    }
}

/// The Codex engine.
pub struct CodexEngine {
    server: Arc<AppServer>,
    events: broadcast::Sender<EngineEvent>,
    state: Mutex<State>,
    approvals: Mutex<HashMap<String, PendingApproval>>,
    next_local: AtomicU64,
    /// Parsed rollouts, per file, so a fold reads only what changed.
    usage_cache: Mutex<agi_engine::usage::ScanCache>,
    /// Files the running turns changed, by thread, newest last.
    deliverables: Mutex<HashMap<String, Vec<DeliveredFile>>>,
    /// Turns this window started through the Codex desktop bridge, by thread.
    ///
    /// The GUI owns the turn; this id is what `interrupt-turn` needs. It is
    /// deliberately only for turns AGI Mizar started — interrupting a turn
    /// another client started is not this bridge's claim.
    gui_turns: Mutex<HashMap<String, String>>,
    /// Running turns started by this adapter on the CLI app-server, by thread.
    ///
    /// `turn/steer` is turn-scoped: the app-server requires `expectedTurnId`.
    /// Keeping the id from `turn/start` avoids a second round trip and, more
    /// importantly, makes "steer" answer a real current turn rather than guess.
    cli_turns: Mutex<HashMap<String, String>>,
    /// Watchers already polling the GUI snapshot for a thread.
    gui_watches: Mutex<HashSet<String>>,
    /// Pending GUI approvals, keyed by the interaction id shown to clients.
    gui_approvals: Mutex<HashMap<String, GuiApproval>>,
    /// Records folded from the last GUI snapshot, for live turns whose
    /// app-server copy has not been written yet.
    gui_records: Mutex<HashMap<String, Vec<Record>>>,
    /// A weak self so a prompt can spawn its own snapshot watcher.
    me: Weak<Self>,
}

impl CodexEngine {
    /// Build the engine. Nothing is spawned until [`CodexEngine::spawn`].
    pub fn new(config: AppServerConfig) -> Arc<Self> {
        let (events, _) = broadcast::channel(512);
        Arc::new_cyclic(|me| Self {
            server: AppServer::new(config),
            events,
            state: Mutex::new(State::new()),
            approvals: Mutex::new(HashMap::new()),
            next_local: AtomicU64::new(1),
            usage_cache: Mutex::new(agi_engine::usage::ScanCache::new()),
            deliverables: Mutex::new(HashMap::new()),
            gui_turns: Mutex::new(HashMap::new()),
            cli_turns: Mutex::new(HashMap::new()),
            gui_watches: Mutex::new(HashSet::new()),
            gui_approvals: Mutex::new(HashMap::new()),
            gui_records: Mutex::new(HashMap::new()),
            me: me.clone(),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn approvals(&self) -> MutexGuard<'_, HashMap<String, PendingApproval>> {
        self.approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emit(&self, event: EngineEvent) {
        let _ = self.events.send(event);
    }

    /// Start the supervisor: spawn, listen, restart.
    ///
    /// Runs until the process ends. Called once, from a task the caller owns.
    pub async fn run(self: Arc<Self>) {
        let mut backoff = RESTART_BASE;
        loop {
            // Subscribed before starting: a notification that arrives during the
            // handshake must not be missed.
            let mut events = self.server.events().subscribe();

            match self.server.start().await {
                Ok(info) => {
                    backoff = RESTART_BASE;
                    let version = info.get("userAgent").and_then(Value::as_str).map(|agent| {
                        agent
                            .split('/')
                            .nth(1)
                            .unwrap_or(agent)
                            .split_whitespace()
                            .next()
                            .unwrap_or(agent)
                            .to_string()
                    });
                    let detail = Some(match &version {
                        Some(version) => format!("app-server {version}"),
                        None => "app-server ready".to_string(),
                    });
                    {
                        let mut state = self.state();
                        state.connected = true;
                        state.detail = detail.clone();
                        state.version = version;
                    }
                    self.emit(EngineEvent::StatusChanged {
                        connected: true,
                        detail,
                    });
                    if let Err(error) = self.refresh_sessions().await {
                        tracing::warn!("codex: could not list threads yet — {error}");
                    }
                }
                Err(error) => {
                    let detail = Some(format!("{error}"));
                    {
                        let mut state = self.state();
                        state.connected = false;
                        state.detail = detail.clone();
                    }
                    tracing::warn!("codex: start failed — {error}");
                    self.emit(EngineEvent::StatusChanged {
                        connected: false,
                        detail,
                    });
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(RESTART_MAX);
                    continue;
                }
            }

            // Serve until the child goes away. The interval is the only way
            // this app-server learns about threads created by another Codex
            // window: they share the on-disk store, not this process's events.
            let mut gui_refresh = tokio::time::interval(GUI_REFRESH);
            gui_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = gui_refresh.tick() => {
                        if let Err(error) = self.refresh_sessions().await {
                            tracing::debug!("codex: periodic thread/list failed — {error}");
                        }
                    }
                    event = events.recv() => match event {
                        Ok(RpcEvent::Notification { method, params }) => {
                            self.on_notification(&method, params);
                        }
                        Ok(RpcEvent::ServerRequest { id, method, params }) => {
                            self.on_server_request(id, method, params);
                        }
                        Ok(RpcEvent::Exited { reason }) => {
                            tracing::warn!("codex: app-server exited — {reason}");
                            self.detach(&reason);
                            break;
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!("codex: fell {skipped} events behind");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                }
            }

            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(RESTART_MAX);
        }
    }

    /// The child went away: forget everything it told us.
    fn detach(&self, reason: &str) {
        let approvals: Vec<String> = {
            let mut approvals = self.approvals();
            let ids = approvals.keys().cloned().collect();
            approvals.clear();
            ids
        };
        {
            let mut state = self.state();
            state.connected = false;
            state.detail = Some(format!("app-server exited: {reason}"));
            // A new server has a different set of loaded threads, so every cached
            // row is stale.
            state.sessions.clear();
        }
        for id in approvals {
            self.emit(EngineEvent::InteractionResolved { id });
        }
        let detail = self.state().detail.clone();
        self.emit(EngineEvent::StatusChanged {
            connected: false,
            detail,
        });
    }

    /// Fold one notification.
    fn on_notification(self: &Arc<Self>, method: &str, params: Value) {
        let thread_id = params.get("threadId").and_then(Value::as_str);

        match method {
            "turn/started" => {
                if let Some(id) = thread_id {
                    self.set_running(id, true);
                }
            }
            "turn/completed" => {
                if let Some(id) = thread_id {
                    self.set_running(id, false);
                }
            }
            // Current Codex app-servers announce the turn lifecycle through the
            // thread status (`active` while a turn runs, `idle` after it), not
            // only through `turn/started` / `turn/completed`. A failed or
            // reconnecting turn may never send the latter, and a thread created
            // a moment ago may not be in `thread/list` yet — both leave the
            // kernel without the running transition the judge waits for.
            "thread/status/changed" => {
                if let Some(id) = thread_id {
                    let running = params
                        .get("status")
                        .and_then(|status| status.get("type"))
                        .and_then(Value::as_str)
                        == Some("active");
                    self.set_running(id, running);
                }
            }
            "item/completed" => {
                // Deliberately not appended as a record here. A Codex
                // notification carries no sequence number, and inventing one
                // would collide with the numbering a history read produces — so
                // the row is marked stale instead, and the transcript comes from
                // one source: `thread/turns/list`.
                if let Some(id) = thread_id {
                    self.touch(id);
                }
            }
            "item/fileChange/patchUpdated" => {
                if let Some(id) = thread_id {
                    let changes = params
                        .get("changes")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let mut known = self
                        .deliverables
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let files = known.entry(id.to_string()).or_default();
                    for change in changes {
                        let Some(path) = change.get("path").and_then(Value::as_str) else {
                            continue;
                        };
                        let name = std::path::Path::new(path)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or(path)
                            .to_string();
                        let description = change
                            .get("kind")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        if let Some(existing) = files.iter_mut().find(|file| file.path == path) {
                            existing.description = description;
                            continue;
                        }
                        files.push(DeliveredFile {
                            path: path.to_string(),
                            name,
                            description,
                        });
                    }
                }
            }
            "thread/name/updated"
            | "thread/started"
            | "thread/archived"
            | "thread/deleted"
            | "thread/unarchived" => {
                // The list is the cheapest way to be exactly right about a row
                // that just changed; a partial update would need a second reader
                // of the same fields.
                self.spawn_refresh();
            }
            _ => {}
        }
    }

    /// Re-read the thread list without blocking the notification loop.
    fn spawn_refresh(self: &Arc<Self>) {
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = engine.refresh_sessions().await {
                tracing::debug!("codex: refresh after a thread event failed — {error}");
            }
        });
    }

    /// Mark one session as touched, so the list redraws.
    fn touch(&self, thread_id: &str) {
        let session = {
            let mut state = self.state();
            let Some(session) = state.sessions.get_mut(thread_id) else {
                return;
            };
            session.updated_at = now_ms();
            session.clone()
        };
        self.emit(EngineEvent::SessionUpdated(session));
    }

    /// Mark one cached session as running or not.
    ///
    /// A status notification can arrive before the thread is in the cache (the
    /// create path, or a turn that begins faster than `thread/list` settles), so
    /// a missing session is inserted instead of dropping the transition: the
    /// kernel's judge is driven by exactly this `running` edge.
    fn set_running(&self, thread_id: &str, running: bool) {
        let session = {
            let mut state = self.state();
            let session = state
                .sessions
                .entry(thread_id.to_string())
                .or_insert_with(|| {
                    Session::bare(SessionKey::new(EngineId::Codex, thread_id), now_ms())
                });
            let was_running = session.running;
            session.running = running;
            // `turn/started` and `thread/status/changed` can both report the
            // beginning of one turn; only count the edge, not every report.
            if running && !was_running {
                session.turns = session.turns.saturating_add(1);
            }
            session.updated_at = now_ms();
            session.clone()
        };
        self.emit(EngineEvent::SessionUpdated(session));
    }
    /// A tool that wants the operator's words, not a yes or no.
    ///
    /// Codex waits on the JSON-RPC id until the answers come back, so the
    /// questions are kept in the same pending map as approvals: both are a
    /// request the operator has to settle.
    fn on_question_request(self: &Arc<Self>, id: Value, params: Value) {
        let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
            let server = Arc::clone(&self.server);
            tokio::spawn(async move {
                let _ = server
                    .respond_error(id, -32602, "requestUserInput without a threadId")
                    .await;
            });
            return;
        };

        let mut ids = Vec::new();
        let mut questions = Vec::new();
        for raw in params
            .get("questions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let Some(question_id) = raw.get("id").and_then(Value::as_str) else {
                continue;
            };
            ids.push(question_id.to_string());
            questions.push(agi_protocol::domain::Question {
                id: question_id.to_string(),
                text: raw
                    .get("question")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                header: raw
                    .get("header")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                detail: None,
                options: raw
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|options| {
                        options
                            .iter()
                            .filter_map(|option| {
                                option
                                    .get("label")
                                    .or_else(|| option.get("value"))
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                multi_select: false,
            });
        }

        let local = format!("cx-{}", self.next_local.fetch_add(1, Ordering::Relaxed));
        let interaction = Interaction {
            id: local.clone(),
            session: Some(SessionKey::new(EngineId::Codex, thread_id)),
            kind: InteractionKind::Question,
            title: "question".to_string(),
            detail: None,
            requested_at: now_ms(),
            questions,
        };
        self.approvals().insert(
            local,
            PendingApproval {
                json_id: id,
                method: "item/tool/requestUserInput".to_string(),
                permissions: Value::Null,
                questions: Some(ids),
            },
        );
        self.emit(EngineEvent::InteractionRequested(interaction));
    }

    /// An approval, or something this adapter does not answer.
    fn on_server_request(self: &Arc<Self>, id: Value, method: String, params: Value) {
        if method == "item/tool/requestUserInput" {
            self.on_question_request(id, params);
            return;
        }
        let is_approval = method.contains("requestApproval")
            || method == "execCommandApproval"
            || method == "applyPatchApproval";
        if !is_approval {
            // Refusing is the honest answer: the tool fails and the operator sees
            // why, rather than a request that hangs forever.
            let server = Arc::clone(&self.server);
            tokio::spawn(async move {
                let _ = server
                    .respond_error(id, -32601, "AGI Mizar does not answer this request")
                    .await;
            });
            self.emit(EngineEvent::Notice {
                level: "warn".to_string(),
                message: format!("Codex asked for {method}, which this build does not handle"),
            });
            return;
        }

        let local = format!("cx-{}", self.next_local.fetch_add(1, Ordering::Relaxed));
        let thread_id = params
            .get("threadId")
            .or_else(|| params.get("conversationId"))
            .and_then(Value::as_str)
            .map(str::to_string);

        let title = match method.as_str() {
            "item/commandExecution/requestApproval" | "execCommandApproval" => "shell",
            "item/fileChange/requestApproval" | "applyPatchApproval" => "apply_patch",
            _ => "permissions",
        };

        let detail = params
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                params.get("command").map(|command| match command {
                    Value::String(text) => text.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                    other => other.to_string(),
                })
            });

        let interaction = Interaction {
            id: local.clone(),
            session: thread_id.map(|id| SessionKey::new(EngineId::Codex, id)),
            kind: InteractionKind::Approval,
            title: title.to_string(),
            detail,
            requested_at: now_ms(),
            questions: Vec::new(),
        };

        self.approvals().insert(
            local,
            PendingApproval {
                json_id: id,
                method,
                permissions: params.get("permissions").cloned().unwrap_or(Value::Null),
                questions: None,
            },
        );
        self.emit(EngineEvent::InteractionRequested(interaction));
    }

    /// Re-read the thread list into the cache and announce the difference.
    async fn refresh_sessions(&self) -> Result<Vec<Session>, EngineError> {
        let mut sessions = list_threads(&self.server, false).await?;
        // The app-server only reports turns it is running. A Codex desktop
        // window owns a different child; its live turn and its title live in
        // the shared store, so overlay those facts without writing anything.
        let gui = gui_state::read();
        for session in &mut sessions {
            let id = &session.key.id;
            if gui.desktop.contains(id) {
                session.source = Some("codex_desktop".to_string());
                if session.title.is_none() {
                    session.title = gui.titles.get(id).cloned();
                }
            }
            if gui.running.contains(id) {
                session.running = true;
            }
        }
        let mut changed = Vec::new();
        let mut removed = Vec::new();
        {
            let mut state = self.state();
            let known: Vec<String> = state.sessions.keys().cloned().collect();
            for session in &sessions {
                let previous = state
                    .sessions
                    .insert(session.key.id.clone(), session.clone());
                if previous.as_ref() != Some(session) {
                    changed.push(session.clone());
                }
            }
            let now = now_ms();
            for id in known {
                if sessions.iter().any(|session| session.key.id == id) {
                    continue;
                }
                // A brand-new thread or one whose turn is still running may be
                // absent from this page of `thread/list`. Dropping it here would
                // erase the very running edge the judge waits for (and the
                // next status change would rebuild the row without its title).
                let keep = state
                    .sessions
                    .get(&id)
                    .map(|session| {
                        session.running
                            || now.saturating_sub(session.updated_at) < THREAD_REMOVAL_GRACE_MS
                    })
                    .unwrap_or(false);
                if keep {
                    continue;
                }
                state.sessions.remove(&id);
                removed.push(SessionKey::new(EngineId::Codex, id));
            }
        }
        // A five-second poll must be quiet when nothing changed; otherwise the
        // window redraws every session twice a tick and the phone sees a stream
        // of identical rows.
        for session in changed {
            self.emit(EngineEvent::SessionUpdated(session));
        }
        for key in removed {
            self.emit(EngineEvent::SessionRemoved(key));
        }
        Ok(sessions)
    }

    fn is_desktop_session(&self, session: &SessionKey) -> bool {
        self.state()
            .sessions
            .get(&session.id)
            .and_then(|row| row.source.as_deref())
            == Some("codex_desktop")
    }

    fn gui_error(message: impl Into<String>) -> EngineError {
        EngineError::Upstream {
            code: "CODEX_DESKTOP_CONTROL".to_string(),
            message: message.into(),
            details: None,
        }
    }

    fn mark_gui_running(&self, session: &SessionKey, running: bool) {
        let changed = {
            let mut state = self.state();
            state.sessions.get_mut(&session.id).map(|row| {
                row.running = running;
                row.updated_at = now_ms();
                row.clone()
            })
        };
        if let Some(row) = changed {
            self.emit(EngineEvent::SessionUpdated(row));
        }
    }

    /// Watch one GUI thread while a turn is in flight.
    ///
    /// The desktop app does not push app-server notifications to this process.
    /// A short snapshot poll is enough to pick up its turn status and pending
    /// approvals, and it naturally stops when the GUI closes or the turn ends.
    fn spawn_gui_watch(&self, session: SessionKey) {
        let Some(engine) = self.me.upgrade() else {
            return;
        };
        {
            let mut watches = self
                .gui_watches
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !watches.insert(session.id.clone()) {
                return;
            }
        }
        tokio::spawn(async move {
            let mut quiet = 0u8;
            let mut saw_activity = false;
            loop {
                match crate::gui_ipc::snapshot(&session.id).await {
                    Ok(state) => {
                        let has_state = state
                            .get("turns")
                            .and_then(Value::as_array)
                            .is_some_and(|turns| !turns.is_empty())
                            || state
                                .get("requests")
                                .and_then(Value::as_array)
                                .is_some_and(|requests| !requests.is_empty());
                        saw_activity |= has_state;
                        if engine.apply_gui_snapshot(&session, &state) {
                            quiet = 0;
                        } else {
                            quiet = quiet.saturating_add(1);
                        }
                    }
                    Err(_) => quiet = quiet.saturating_add(1),
                }
                // The desktop can take several seconds to materialise the
                // first turn (the model is still connecting), so an empty
                // snapshot right after start-turn must not end the watcher.
                // Once a turn has appeared, a short quiet window is enough.
                let limit = if saw_activity { 4 } else { 120 };
                if quiet >= limit {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(700)).await;
            }
            engine
                .gui_watches
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&session.id);
            engine
                .gui_turns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&session.id);
            engine.remove_gui_approvals(&session.id);
        });
    }

    /// Fold one desktop conversation snapshot into session state and approvals.
    fn apply_gui_snapshot(&self, session: &SessionKey, state: &Value) -> bool {
        let turns = state.get("turns").and_then(Value::as_array);
        let running = turns
            .map(|turns| {
                turns
                    .iter()
                    .any(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
            })
            .unwrap_or(false);

        let mut active = HashSet::new();
        for request in state
            .get("requests")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let Some(request_id) = request.get("id") else {
                continue;
            };
            let Some(method) = request.get("method").and_then(Value::as_str) else {
                continue;
            };
            let is_question = method == "item/tool/requestUserInput";
            if !method.ends_with("requestApproval") && !is_question {
                continue;
            }
            let id_text = request_id
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| request_id.to_string());
            let local = format!("gui-{id_text}");
            active.insert(local.clone());
            let (questions, question_ids) = if is_question {
                gui_questions(request.get("params"))
            } else {
                (Vec::new(), Vec::new())
            };
            let already = {
                let mut approvals = self
                    .gui_approvals
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if approvals.contains_key(&local) {
                    true
                } else {
                    approvals.insert(
                        local.clone(),
                        GuiApproval {
                            session: session.clone(),
                            request_id: request_id.clone(),
                            method: method.to_string(),
                            questions: is_question.then_some(question_ids),
                        },
                    );
                    false
                }
            };
            if !already {
                let kind = if is_question {
                    InteractionKind::Question
                } else {
                    InteractionKind::Approval
                };
                let title = if is_question {
                    questions
                        .first()
                        .map(|question| question.text.clone())
                        .unwrap_or_else(|| "question".to_string())
                } else {
                    gui_approval_title(method)
                };
                self.emit(EngineEvent::InteractionRequested(Interaction {
                    id: local,
                    session: Some(session.clone()),
                    kind,
                    title,
                    detail: gui_approval_detail(request.get("params")),
                    requested_at: now_ms(),
                    questions,
                }));
            }
        }

        let stale = {
            let approvals = self
                .gui_approvals
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            approvals
                .iter()
                .filter(|(id, pending)| pending.session.id == session.id && !active.contains(*id))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        };
        for id in stale {
            self.gui_approvals
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            self.emit(EngineEvent::InteractionResolved { id });
        }

        // Keep a snapshot-derived record cache for a live GUI turn. The
        // app-server copy appears after the turn is written, so a phone that
        // opens the conversation mid-turn would otherwise see nothing.
        if let Some(turns) = turns.filter(|turns| !turns.is_empty()) {
            let incoming = projection::records_from_turns(turns);
            let new_records = {
                let mut cache = self
                    .gui_records
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let last = cache
                    .get(&session.id)
                    .and_then(|records| records.last())
                    .map(|record| record.seq)
                    .unwrap_or(0);
                let fresh = incoming
                    .iter()
                    .filter(|record| record.seq > last)
                    .cloned()
                    .collect::<Vec<_>>();
                cache.insert(session.id.clone(), incoming);
                fresh
            };
            for record in new_records {
                self.emit(EngineEvent::RecordAppended {
                    session: session.clone(),
                    record,
                });
            }
        }

        let active_approvals = !active.is_empty();
        if !running && !active_approvals {
            self.gui_turns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&session.id);
        }

        let changed = {
            let mut state = self.state();
            state.sessions.get_mut(&session.id).and_then(|row| {
                if row.running == running {
                    None
                } else {
                    row.running = running;
                    row.updated_at = now_ms();
                    Some(row.clone())
                }
            })
        };
        if let Some(row) = changed {
            self.emit(EngineEvent::SessionUpdated(row));
        }
        running || active_approvals
    }

    fn remove_gui_approvals(&self, session_id: &str) {
        let removed = {
            let mut approvals = self
                .gui_approvals
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let ids = approvals
                .iter()
                .filter(|(_, pending)| pending.session.id == session_id)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            for id in &ids {
                approvals.remove(id);
            }
            ids
        };
        for id in removed {
            self.emit(EngineEvent::InteractionResolved { id });
        }
    }
}

/// Read `thread/list` into the shared model.
///
/// `archived` asks the other half of the list. Codex keeps archived threads out
/// of the live list on purpose; the dataset backfill is the one caller that
/// wants them, so it asks directly instead of changing what the window shows.
async fn list_threads(server: &AppServer, archived: bool) -> Result<Vec<Session>, EngineError> {
    let page = server
        .call(
            "thread/list",
            json!({
                "limit": THREAD_PAGE,
                "sortKey": "updated_at",
                "sortDirection": "desc",
                "archived": archived,
            }),
        )
        .await
        .map_err(engine_error)?;

    Ok(page
        .get("data")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(projection::session_from_thread)
                .collect()
        })
        .unwrap_or_default())
}

/// Remember the turn id a CLI `turn/start` produced.
fn remember_cli_turn(engine: &CodexEngine, session: &SessionKey, turn: &str) {
    engine
        .cli_turns
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(session.id.clone(), turn.to_string());
}

/// Translate a transport failure into an engine failure.
fn engine_error(error: crate::rpc::RpcError) -> EngineError {
    use crate::rpc::RpcError;
    match error {
        RpcError::NotRunning => EngineError::NotConnected {
            engine: EngineId::Codex,
        },
        RpcError::Timeout(_) => EngineError::Timeout("codex request"),
        RpcError::Upstream { code, message } => EngineError::Upstream {
            code: if error_is_writer_held(&message) {
                "WRITER_HELD".to_string()
            } else {
                format!("codex/{code}")
            },
            message,
            details: None,
        },
        RpcError::Transport(message) => EngineError::upstream(message),
    }
}

fn error_is_writer_held(message: &str) -> bool {
    message.contains("locked") || message.contains("in use") || message.contains("already open")
}

/// Epoch milliseconds.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[async_trait]
impl Engine for CodexEngine {
    fn id(&self) -> EngineId {
        EngineId::Codex
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            attachments: true,
            file_preview: true,
            fork: true,
            archive: true,
            activity: true,
            balance: false,
            models: true,
            permissions: true,
        }
    }

    async fn reconnect(&self) -> Result<(), EngineError> {
        // Re-reading the session index is what re-probes the CLI and its files.
        self.refresh_sessions().await?;
        Ok(())
    }

    fn status(&self) -> (bool, Option<String>) {
        let state = self.state();
        (state.connected, state.detail.clone())
    }

    async fn sessions(&self) -> Result<Vec<Session>, EngineError> {
        self.refresh_sessions().await
    }

    /// The threads the live list hides, for the one-year dataset backfill.
    async fn archived_sessions(&self) -> Result<Vec<Session>, EngineError> {
        list_threads(&self.server, true).await
    }

    async fn records(
        &self,
        session: &SessionKey,
        limit: usize,
        before: Option<i64>,
    ) -> Result<Vec<Record>, EngineError> {
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
        }

        // A live GUI turn has not necessarily been written to the app-server's
        // turn list yet; the watcher snapshot is the freshest view.
        if self.is_desktop_session(session) {
            let cached = self
                .gui_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&session.id)
                .cloned()
                .unwrap_or_default();
            if !cached.is_empty() {
                return Ok(page_records(cached, limit, before));
            }
        }

        let page = self
            .server
            .call(
                "thread/turns/list",
                json!({
                    "threadId": session.raw(),
                    "limit": TURN_PAGE,
                    "sortDirection": "desc",
                    "itemsView": "full",
                }),
            )
            .await
            .map_err(engine_error)?;

        let mut turns: Vec<Value> = page
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // The list answers newest-first; a transcript reads oldest-first.
        turns.reverse();

        let mut records = projection::records_from_turns(&turns);
        if let Some(before) = before {
            records.retain(|record| record.seq < before);
        }
        let start = records.len().saturating_sub(limit);
        Ok(records.split_off(start))
    }

    /// Every day of activity in the local rollout tree.
    ///
    /// Codex's app-server answers about *loaded* threads only, and history is a
    /// page at a time, so a year of usage cannot be asked for over RPC. The
    /// rollouts under `~/.codex/sessions` are the durable record of what was
    /// billed, and they are read from disk instead.
    /// Groups this host's threads by the directory they work in.
    ///
    /// Codex has no workspace registry, but a thread always knows its cwd and
    /// that is the grouping the operator already organises by. The path is the
    /// id: it is stable across launches, unlike anything this side could mint.
    async fn workspaces(&self) -> Result<Vec<Workspace>, EngineError> {
        let sessions = self.sessions().await?;
        let mut groups: Vec<Workspace> = Vec::new();
        for session in sessions {
            let Some(cwd) = session.cwd.clone().filter(|cwd| !cwd.is_empty()) else {
                continue;
            };
            let name = std::path::Path::new(&cwd)
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .map(str::to_string);
            match groups.iter_mut().find(|group| group.path == cwd) {
                Some(group) => group
                    .session_ids
                    .get_or_insert_with(Vec::new)
                    .push(session.key.to_string()),
                None => groups.push(Workspace {
                    id: cwd.clone(),
                    path: cwd,
                    name,
                    session_ids: Some(vec![session.key.to_string()]),
                }),
            }
        }
        Ok(groups)
    }

    /// Files the model changed, from the patch updates Codex volunteers.
    ///
    /// Paths are made relative to the thread's working directory before they are
    /// handed out: the phone opens them through the session's file endpoints,
    /// which are fenced to that directory.
    async fn deliverables(&self, session: &SessionKey) -> Result<Vec<DeliveredFile>, EngineError> {
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        let files = self
            .deliverables
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session.raw())
            .cloned()
            .unwrap_or_default();
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let cwd = self.session_cwd(session).await.ok();
        Ok(files
            .into_iter()
            .map(|mut file| {
                if let Some(cwd) = &cwd {
                    if let Ok(relative) = std::path::Path::new(&file.path).strip_prefix(cwd) {
                        file.path = relative.to_string_lossy().to_string();
                    }
                }
                file
            })
            .collect())
    }

    /// What this thread has cost: turns, steps, time in the model and in tools,
    /// and the token total its rollout ends with.
    ///
    /// The durations come from the items the app-server reports; the tokens come
    /// from `thread.path`, the rollout Codex writes beside the thread, because
    /// the protocol only volunteers totals through notifications, and a session
    /// page opened a day later has no notifications to fold.
    async fn stats(
        &self,
        session: &SessionKey,
    ) -> Result<agi_protocol::domain::SessionStats, EngineError> {
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        let value = self
            .server
            .call(
                "thread/read",
                json!({ "threadId": session.raw(), "includeTurns": true }),
            )
            .await
            .map_err(engine_error)?;
        let thread = value.get("thread").cloned().unwrap_or(Value::Null);
        let turns = thread
            .get("turns")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut stats = agi_protocol::domain::SessionStats {
            turns: turns.len() as u32,
            ..Default::default()
        };
        for turn in &turns {
            for item in turn
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                let duration = item.get("durationMs").and_then(Value::as_i64).unwrap_or(0);
                match kind {
                    "userMessage" => continue,
                    "reasoning" | "agentMessage" => {
                        stats.steps = stats.steps.saturating_add(1);
                        stats.llm_ms += duration;
                    }
                    _ => {
                        stats.steps = stats.steps.saturating_add(1);
                        stats.tool_ms += duration;
                    }
                }
            }
        }
        if let Some(path) = thread.get("path").and_then(Value::as_str) {
            if let Some((input, output)) = crate::usage::thread_tokens(std::path::Path::new(path)) {
                stats.input_tokens = input;
                stats.output_tokens = output;
            }
        }
        Ok(stats)
    }

    /// What the running turn is doing, taken from the records it already
    /// produced.
    ///
    /// Derived rather than listened for: a second feed that can disagree with
    /// the transcript under it is worse than none, and the records already carry
    /// the tool names, the arguments and the output.
    async fn activity(
        &self,
        session: &SessionKey,
        limit: usize,
    ) -> Result<Vec<ActivityEntry>, EngineError> {
        let records = self.records(session, ACTIVITY_PAGE, None).await?;
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
                        if matches!(tool.name.as_str(), "bash" | "shell" | "exec" | "command") {
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
                    title: tool.name.clone(),
                    text,
                    turn: Some(latest),
                })
            })
            .collect();
        if entries.len() > limit {
            entries = entries.split_off(entries.len() - limit);
        }
        Ok(entries)
    }

    /// Every model this host can run.
    ///
    /// The list comes from the app-server, which means it is the same list the
    /// GUI shows: with a routed provider it is that provider's models, not
    /// OpenAI's. `supportedReasoningEfforts` is what the reasoning picker needs,
    /// and it is absent for models that have none.
    async fn models(&self) -> Result<Vec<ModelInfo>, EngineError> {
        let value = self
            .server
            .call(
                "model/list",
                json!({ "limit": MODEL_PAGE, "includeHidden": false }),
            )
            .await
            .map_err(engine_error)?;
        let mut models = Vec::new();
        for model in value
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let Some(id) = model.get("id").and_then(Value::as_str) else {
                continue;
            };
            models.push(ModelInfo {
                id: id.to_string(),
                label: model
                    .get("displayName")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
                provider: None,
                reasoning: model
                    .get("supportedReasoningEfforts")
                    .and_then(Value::as_array)
                    .map(|efforts| {
                        efforts
                            .iter()
                            .filter_map(|effort| {
                                effort.get("reasoningEffort").and_then(Value::as_str)
                            })
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
            });
        }
        Ok(models)
    }

    /// The permission profiles the app-server will accept.
    ///
    /// They arrive with a leading colon — that spelling is the id, and the same
    /// string is what `thread/settings/update` takes.
    async fn permissions(&self) -> Result<Vec<PermissionPreset>, EngineError> {
        let value = self
            .server
            .call("permissionProfile/list", json!({}))
            .await
            .map_err(engine_error)?;
        Ok(value
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|profile| {
                let id = profile.get("id").and_then(Value::as_str)?.to_string();
                Some(PermissionPreset {
                    label: id.trim_start_matches(':').replace('-', " "),
                    description: profile
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    id,
                })
            })
            .collect())
    }

    /// Start a thread, the way the GUI's "new thread" does.
    async fn create(&self, cwd: Option<&str>) -> Result<SessionKey, EngineError> {
        let mut params = json!({});
        if let Some(cwd) = cwd {
            params["cwd"] = json!(cwd);
        }
        let value = self
            .server
            .call("thread/start", params)
            .await
            .map_err(engine_error)?;
        let id = value
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| EngineError::upstream("thread/start answered without a thread"))?;
        // Insert the thread the server just returned, without waiting for
        // `thread/list`: that list is only eventually consistent, and a phone
        // creates a thread and prompts it immediately. Without the cache entry
        // the turn's `turn/started` / `thread/status/changed` notifications are
        // dropped by `set_running`, so the kernel never sees the running
        // true -> false transition that triggers the judge.
        if let Some(thread) = value.get("thread") {
            if let Some(session) = projection::session_from_thread(thread) {
                self.state()
                    .sessions
                    .insert(session.key.id.clone(), session.clone());
                self.emit(EngineEvent::SessionUpdated(session));
            }
        }
        Ok(SessionKey::new(EngineId::Codex, id))
    }

    async fn rename(&self, session: &SessionKey, title: &str) -> Result<(), EngineError> {
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        self.server
            .call(
                "thread/name/set",
                json!({ "threadId": session.raw(), "name": title }),
            )
            .await
            .map_err(engine_error)?;
        Ok(())
    }

    async fn compact(&self, session: &SessionKey) -> Result<(), EngineError> {
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        // Codex owns compaction as an app-server thread operation. It installs
        // a compaction item and drives the context rewrite itself; there is no
        // prompt to send and no transcript file this process may rewrite.
        self.server
            .call("thread/compact/start", json!({ "threadId": session.raw() }))
            .await
            .map_err(engine_error)?;
        Ok(())
    }

    async fn archive(&self, session: &SessionKey, _stop_activity: bool) -> Result<(), EngineError> {
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        match self
            .server
            .call("thread/archive", json!({ "threadId": session.raw() }))
            .await
        {
            Ok(_) => Ok(()),
            // A thread that has not run yet has no rollout to archive; Codex
            // refuses, but from the operator's side there is nothing left to
            // hide, so the intent is already satisfied.
            Err(crate::rpc::RpcError::Upstream { message, .. })
                if message.contains("no rollout found") =>
            {
                Ok(())
            }
            // The GUI owns the writer while a thread is open. The desktop's
            // own archive action is still reachable through the optional CDP
            // bridge; without it, return the original writer-held error.
            Err(crate::rpc::RpcError::Upstream { message, .. })
                if error_is_writer_held(&message) =>
            {
                match crate::cdp::archive(session.raw()).await {
                    Ok(()) => Ok(()),
                    Err(cdp) => Err(EngineError::Upstream {
                        code: "CODEX_ARCHIVE_CDP".to_string(),
                        message: format!(
                            "Codex Desktop owns this thread ({message}); CDP fallback: {cdp}"
                        ),
                        details: None,
                    }),
                }
            }
            Err(error) => Err(engine_error(error)),
        }
    }

    /// The directory a thread was started in, from the thread itself.
    ///
    /// Every file feature — preview, stat, slices — hangs off this, so it is the
    /// one piece that turns "the model says it wrote a file" into something the
    /// preview page can open.
    async fn session_cwd(&self, session: &SessionKey) -> Result<String, EngineError> {
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        let value = self
            .server
            .call(
                "thread/read",
                json!({ "threadId": session.raw(), "includeTurns": false }),
            )
            .await
            .map_err(engine_error)?;
        value
            .get("thread")
            .and_then(|thread| thread.get("cwd"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| EngineError::upstream("thread/read answered without a cwd"))
    }

    async fn usage_history(&self) -> Result<Vec<agi_protocol::api::UsageDay>, EngineError> {
        let mut cache = self
            .usage_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(crate::usage::days_with_cache(
            &crate::usage::sessions_dir(),
            &mut cache,
        ))
    }

    async fn prompt(&self, session: &SessionKey, text: &str) -> Result<(), EngineError> {
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
        }

        if self.is_desktop_session(session) {
            let turn = crate::gui_ipc::start_turn(session.raw(), text, &[])
                .await
                .map_err(Self::gui_error)?;
            self.gui_turns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(session.id.clone(), turn);
            self.mark_gui_running(session, true);
            self.spawn_gui_watch(session.clone());
            return Ok(());
        }

        // A thread has to be loaded before it can take a turn, and resuming first
        // is what turns "another window owns this" into a clear error instead of
        // an opaque refusal from `turn/start`.
        match self
            .server
            .call(
                "thread/resume",
                json!({ "threadId": session.raw(), "excludeTurns": true }),
            )
            .await
        {
            Ok(_) => {}
            Err(crate::rpc::RpcError::Upstream { message, .. })
                if error_is_writer_held(&message) =>
            {
                return Err(EngineError::Upstream {
                    code: "WRITER_HELD".to_string(),
                    message,
                    details: None,
                });
            }
            // A thread that does not exist yet cannot be resumed; `turn/start`
            // will say so with a better message.
            Err(_) => {}
        }

        let started = self
            .server
            .call(
                "turn/start",
                json!({
                    "threadId": session.raw(),
                    "input": [{ "type": "text", "text": text, "text_elements": [] }],
                    "clientUserMessageId": format!("agi-{}", now_ms()),
                }),
            )
            .await
            .map_err(engine_error)?;
        if let Some(turn) = started
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
        {
            remember_cli_turn(self, session, turn);
        }
        Ok(())
    }

    fn supports_attachments(&self) -> bool {
        true
    }

    async fn prompt_with(
        &self,
        session: &SessionKey,
        text: &str,
        attachments: &[agi_protocol::domain::PromptAttachment],
    ) -> Result<(), EngineError> {
        if attachments.is_empty() {
            return self.prompt(session, text).await;
        }
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        let values = attachment_values(attachments);
        if values.is_empty() {
            return Err(EngineError::upstream(
                "Codex attachments need a local path; the upload had none",
            ));
        }
        if self.is_desktop_session(session) {
            let turn = crate::gui_ipc::start_turn(session.raw(), text, &values)
                .await
                .map_err(Self::gui_error)?;
            self.gui_turns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(session.id.clone(), turn);
            self.mark_gui_running(session, true);
            self.spawn_gui_watch(session.clone());
            return Ok(());
        }

        // The CLI app-server path is the same as prompt, with extra input parts.
        match self
            .server
            .call(
                "thread/resume",
                json!({ "threadId": session.raw(), "excludeTurns": true }),
            )
            .await
        {
            Ok(_) => {}
            Err(crate::rpc::RpcError::Upstream { message, .. })
                if error_is_writer_held(&message) =>
            {
                return Err(EngineError::Upstream {
                    code: "WRITER_HELD".to_string(),
                    message,
                    details: None,
                });
            }
            Err(_) => {}
        }
        let mut input = vec![json!({
            "type": "text",
            "text": text,
            "text_elements": [],
        })];
        input.extend(values);
        let started = self
            .server
            .call(
                "turn/start",
                json!({
                    "threadId": session.raw(),
                    "input": input,
                    "clientUserMessageId": format!("agi-{}", now_ms()),
                }),
            )
            .await
            .map_err(engine_error)?;
        if let Some(turn) = started
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
        {
            remember_cli_turn(self, session, turn);
        }
        Ok(())
    }

    async fn steer(&self, session: &SessionKey, text: &str) -> Result<(), EngineError> {
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        if self.is_desktop_session(session) {
            let cwd = self
                .state()
                .sessions
                .get(&session.id)
                .and_then(|row| row.cwd.clone());
            crate::gui_ipc::steer(session.raw(), text, cwd.as_deref())
                .await
                .map_err(Self::gui_error)?;
            return Ok(());
        }
        let expected = self
            .cli_turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session.id)
            .cloned()
            .ok_or_else(|| EngineError::upstream("no running Codex CLI turn to steer"))?;
        self.server
            .call(
                "turn/steer",
                json!({
                    "threadId": session.raw(),
                    "expectedTurnId": expected,
                    "input": [{ "type": "text", "text": text, "text_elements": [] }],
                    "clientUserMessageId": format!("agi-steer-{}", now_ms()),
                }),
            )
            .await
            .map_err(engine_error)?;
        Ok(())
    }

    async fn fork(&self, session: &SessionKey, _at_seq: i64) -> Result<SessionKey, EngineError> {
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        let value = self
            .server
            .call("thread/fork", json!({ "threadId": session.raw() }))
            .await
            .map_err(engine_error)?;
        let thread = value.get("thread").cloned().unwrap_or(Value::Null);
        let forked = projection::session_from_thread(&thread)
            .ok_or_else(|| EngineError::upstream("thread/fork answered without a thread"))?;
        let key = forked.key.clone();
        self.state().sessions.insert(key.id.clone(), forked.clone());
        self.emit(EngineEvent::SessionUpdated(forked));
        Ok(key)
    }

    async fn cancel(&self, session: &SessionKey) -> Result<(), EngineError> {
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        if self.is_desktop_session(session) {
            let turn = self
                .gui_turns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&session.id)
                .cloned();
            let Some(turn) = turn else {
                return Err(Self::gui_error(
                    "this Codex desktop turn was not started by AGI Mizar",
                ));
            };
            crate::gui_ipc::interrupt(session.raw(), &turn)
                .await
                .map_err(Self::gui_error)?;
            self.gui_turns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&session.id);
            self.mark_gui_running(session, false);
            self.remove_gui_approvals(&session.id);
            return Ok(());
        }

        let page = self
            .server
            .call(
                "thread/turns/list",
                json!({
                    "threadId": session.raw(),
                    "limit": 5,
                    "sortDirection": "desc",
                    "itemsView": "none",
                }),
            )
            .await
            .map_err(engine_error)?;

        let turn_id = page
            .get("data")
            .and_then(Value::as_array)
            .and_then(|turns| {
                turns.iter().find_map(|turn| {
                    let status = turn.get("status").and_then(Value::as_str)?;
                    (status == "inProgress").then(|| turn.get("id").and_then(Value::as_str))?
                })
            })
            .map(str::to_string);

        // Nothing running is not a failure: the operator pressed stop on a turn
        // that had already finished.
        let Some(turn_id) = turn_id else {
            return Ok(());
        };

        self.server
            .call(
                "turn/interrupt",
                json!({ "threadId": session.raw(), "turnId": turn_id }),
            )
            .await
            .map_err(engine_error)?;
        Ok(())
    }

    async fn answer(&self, id: &str, answer: Answer) -> Result<(), EngineError> {
        if id.starts_with("gui-") {
            let pending = self
                .gui_approvals
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(id);
            let Some(pending) = pending else {
                return Err(EngineError::upstream(format!(
                    "no pending Codex desktop approval {id}"
                )));
            };
            if pending.method == "item/tool/requestUserInput" {
                let response = gui_user_input_response(
                    pending.questions.as_deref().unwrap_or_default(),
                    answer.answers.as_deref().unwrap_or_default(),
                );
                crate::gui_ipc::answer_user_input(
                    pending.session.raw(),
                    &pending.request_id,
                    response,
                )
                .await
                .map_err(Self::gui_error)?;
                self.emit(EngineEvent::InteractionResolved { id: id.to_string() });
                return Ok(());
            }
            if !matches!(
                pending.method.as_str(),
                "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
            ) {
                return Err(EngineError::Unsupported {
                    engine: EngineId::Codex,
                    feature: "answering this Codex desktop request",
                });
            }
            let decision = if answer.outcome.as_deref() == Some("allowed-once") {
                "accept"
            } else {
                "cancel"
            };
            crate::gui_ipc::answer(
                pending.session.raw(),
                &pending.request_id,
                &pending.method,
                decision,
            )
            .await
            .map_err(Self::gui_error)?;
            self.emit(EngineEvent::InteractionResolved { id: id.to_string() });
            return Ok(());
        }
        let Some(pending) = self.approvals().remove(id) else {
            return Err(EngineError::upstream(format!("no pending approval {id}")));
        };

        if let Some(ids) = &pending.questions {
            // `answers` is one entry per question, shaped `{id, answers: [...]}`.
            // A client that sends them in order without ids is still understood.
            let given = answer.answers.unwrap_or_default();
            let mut mapped = serde_json::Map::new();
            for (index, question_id) in ids.iter().enumerate() {
                let entry = given
                    .iter()
                    .find(|entry| {
                        entry
                            .get("id")
                            .or_else(|| entry.get("questionId"))
                            .and_then(Value::as_str)
                            == Some(question_id.as_str())
                    })
                    .or_else(|| given.get(index));
                let values = entry
                    .and_then(|entry| entry.get("answers"))
                    .and_then(Value::as_array)
                    .cloned()
                    .or_else(|| entry.and_then(Value::as_str).map(|text| vec![json!(text)]))
                    .unwrap_or_default();
                mapped.insert(question_id.clone(), json!({ "answers": values }));
            }
            let result = json!({ "answers": mapped });
            self.server
                .respond(pending.json_id, result)
                .await
                .map_err(engine_error)?;
            self.emit(EngineEvent::InteractionResolved { id: id.to_string() });
            return Ok(());
        }

        let allowed = answer.outcome.as_deref() == Some("allowed-once");
        let result = match pending.method.as_str() {
            "execCommandApproval" | "applyPatchApproval" => {
                if allowed {
                    json!({ "decision": "approved" })
                } else {
                    json!({ "decision": { "denied": { "rejection": "rejected in AGI Mizar" } } })
                }
            }
            "item/permissions/requestApproval" => json!({
                // Codex reads a refusal as "no additional permissions granted".
                // Echoing the request only on approval is what keeps a rejection
                // from widening access.
                "permissions": if allowed { pending.permissions } else { json!({}) },
                "scope": "turn",
            }),
            _ => json!({
                "decision": if allowed { "accept" } else { "decline" },
            }),
        };

        self.server
            .respond(pending.json_id, result)
            .await
            .map_err(engine_error)?;
        self.emit(EngineEvent::InteractionResolved { id: id.to_string() });
        Ok(())
    }

    async fn set_model(&self, session: &SessionKey, model: &str) -> Result<(), EngineError> {
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        if self.is_desktop_session(session) {
            crate::gui_ipc::set_model(session.raw(), model)
                .await
                .map_err(Self::gui_error)?;
            return Ok(());
        }
        self.server
            .call(
                "thread/settings/update",
                json!({ "threadId": session.raw(), "model": model }),
            )
            .await
            .map_err(engine_error)?;
        Ok(())
    }

    async fn set_permission(&self, session: &SessionKey, preset: &str) -> Result<(), EngineError> {
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        if self.is_desktop_session(session) {
            crate::gui_ipc::set_permission(session.raw(), preset)
                .await
                .map_err(Self::gui_error)?;
            return Ok(());
        }
        // Codex names a profile with a leading colon; accept both spellings so a
        // caller that already has the raw id is not punished for it.
        let id = if preset.starts_with(':') {
            preset.to_string()
        } else {
            format!(":{preset}")
        };
        self.server
            .call(
                "thread/settings/update",
                json!({ "threadId": session.raw(), "permissions": id }),
            )
            .await
            .map_err(engine_error)?;
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }
}

/// The first line of a tool's argument or output, cut to one readable line.
fn first_line(text: Option<&str>) -> String {
    let line = text
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or_default()
        .trim();
    line.chars().take(160).collect()
}

/// The suffix an activity id ends with, one per kind.
fn kind_slug(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Thinking => "thinking",
        ActivityKind::Command => "command",
        ActivityKind::Output => "output",
        ActivityKind::Tool => "tool",
    }
}

/// A short label for a desktop approval request.
fn gui_approval_title(method: &str) -> String {
    if method.contains("commandExecution") {
        "command".to_string()
    } else if method.contains("fileChange") {
        "file change".to_string()
    } else {
        "approval".to_string()
    }
}

/// A command or reason from a desktop approval request.
fn gui_approval_detail(params: Option<&Value>) -> Option<String> {
    let params = params?;
    params
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            params.get("command").map(|command| match command {
                Value::String(text) => text.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
                other => other.to_string(),
            })
        })
}

/// Parse the questions of a desktop `requestUserInput` request.
/// Parse the questions of a desktop `requestUserInput` request.
fn gui_questions(params: Option<&Value>) -> (Vec<Question>, Vec<String>) {
    let mut questions = Vec::new();
    let mut ids = Vec::new();
    for raw in params
        .and_then(|params| params.get("questions"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let Some(question_id) = raw.get("id").and_then(Value::as_str) else {
            continue;
        };
        ids.push(question_id.to_string());
        questions.push(Question {
            id: question_id.to_string(),
            text: raw
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            header: raw
                .get("header")
                .and_then(Value::as_str)
                .map(str::to_string),
            detail: None,
            options: raw
                .get("options")
                .and_then(Value::as_array)
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|option| {
                            option
                                .get("label")
                                .or_else(|| option.get("value"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default(),
            multi_select: false,
        });
    }
    (questions, ids)
}

/// Turn the shared `Answer` shape into Codex's `{answers: {id: {answers: []}}}`.
fn gui_user_input_response(question_ids: &[String], given: &[Value]) -> Value {
    let mut mapped = serde_json::Map::new();
    for (index, question_id) in question_ids.iter().enumerate() {
        let entry = given
            .iter()
            .find(|entry| {
                entry
                    .get("id")
                    .or_else(|| entry.get("questionId"))
                    .and_then(Value::as_str)
                    == Some(question_id.as_str())
            })
            .or_else(|| given.get(index));
        let values = entry
            .and_then(|entry| entry.get("answers"))
            .and_then(Value::as_array)
            .cloned()
            .or_else(|| entry.and_then(Value::as_str).map(|text| vec![json!(text)]))
            .unwrap_or_default();
        mapped.insert(question_id.clone(), json!({ "answers": values }));
    }
    json!({ "answers": mapped })
}

fn page_records(mut records: Vec<Record>, limit: usize, before: Option<i64>) -> Vec<Record> {
    if let Some(before) = before {
        records.retain(|record| record.seq < before);
    }
    let start = records.len().saturating_sub(limit);
    records.split_off(start)
}

fn attachment_values(attachments: &[agi_protocol::domain::PromptAttachment]) -> Vec<Value> {
    attachments
        .iter()
        .filter_map(|attachment| {
            let path = attachment.path.as_deref()?;
            match attachment.kind {
                agi_protocol::domain::AttachmentKind::Image => {
                    Some(json!({ "type": "localImage", "path": path }))
                }
                agi_protocol::domain::AttachmentKind::File => Some(json!({
                    "type": "mention",
                    "name": attachment.name,
                    "path": path,
                })),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Arc<CodexEngine> {
        CodexEngine::new(AppServerConfig {
            binary: "/bin/false".to_string(),
            args: Vec::new(),
            cwd: None,
        })
    }

    fn desktop_session(engine: &CodexEngine) -> SessionKey {
        let key = SessionKey::new(EngineId::Codex, "gui-1");
        let mut session = Session::bare(key.clone(), 1);
        session.source = Some("codex_desktop".to_string());
        session.running = true;
        engine.state().sessions.insert(key.id.clone(), session);
        key
    }

    #[tokio::test]
    async fn gui_snapshot_records_are_readable_mid_turn() {
        let engine = engine();
        let key = desktop_session(&engine);
        let snapshot = json!({
            "turns": [{
                "id": "t1",
                "status": "inProgress",
                "startedAtMs": 1_700_000_000_000_i64,
                "items": [{
                    "type": "userMessage",
                    "id": "m1",
                    "content": [{ "type": "text", "text": "hello" }]
                }]
            }],
            "requests": []
        });
        engine.apply_gui_snapshot(&key, &snapshot);
        let records = engine.records(&key, 100, None).await.unwrap();
        assert!(records.iter().any(|record| {
            record.kind == RecordKind::UserMessage && record.text.as_deref() == Some("hello")
        }));
    }

    #[test]
    fn a_gui_snapshot_becomes_a_question_with_ids() {
        let engine = engine();
        let key = desktop_session(&engine);
        let mut events = engine.events.subscribe();
        let snapshot = json!({
            "turns": [{ "status": "inProgress" }],
            "requests": [{
                "id": 1,
                "method": "item/tool/requestUserInput",
                "params": {
                    "questions": [{
                        "id": "q1",
                        "header": "Choice",
                        "question": "Which one?",
                        "options": [{ "label": "A" }, { "label": "B" }]
                    }]
                }
            }]
        });
        assert!(engine.apply_gui_snapshot(&key, &snapshot));
        let event = events.try_recv().expect("question event");
        match event {
            EngineEvent::InteractionRequested(interaction) => {
                assert_eq!(interaction.id, "gui-1");
                assert_eq!(interaction.kind, InteractionKind::Question);
                assert_eq!(interaction.questions[0].id, "q1");
                assert_eq!(interaction.questions[0].options, vec!["A", "B"]);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let response = gui_user_input_response(
            &["q1".to_string()],
            &[json!({ "id": "q1", "answers": ["A"] })],
        );
        assert_eq!(response["answers"]["q1"]["answers"][0], "A");
    }

    #[test]
    fn a_gui_snapshot_becomes_an_approval_once() {
        let engine = engine();
        let key = desktop_session(&engine);
        let mut events = engine.events.subscribe();
        let snapshot = json!({
            "turns": [{ "status": "inProgress" }],
            "requests": [{
                "id": 0,
                "method": "item/commandExecution/requestApproval",
                "params": { "command": "echo hi" }
            }]
        });
        assert!(engine.apply_gui_snapshot(&key, &snapshot));
        let event = events.try_recv().expect("approval event");
        match event {
            EngineEvent::InteractionRequested(interaction) => {
                assert_eq!(interaction.id, "gui-0");
                assert_eq!(interaction.kind, InteractionKind::Approval);
                assert_eq!(interaction.detail.as_deref(), Some("echo hi"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        // The same snapshot must not produce a second card. The first pass may
        // also have emitted snapshot-derived records; drain those.
        assert!(engine.apply_gui_snapshot(&key, &snapshot));
        let mut another_card = false;
        while let Ok(event) = events.try_recv() {
            if matches!(event, EngineEvent::InteractionRequested(_)) {
                another_card = true;
            }
        }
        assert!(!another_card);

        let finished = json!({ "turns": [{ "status": "completed" }], "requests": [] });
        assert!(!engine.apply_gui_snapshot(&key, &finished));
        let mut saw_resolved = false;
        while let Ok(event) = events.try_recv() {
            if let EngineEvent::InteractionResolved { id } = event {
                assert_eq!(id, "gui-0");
                saw_resolved = true;
            }
        }
        assert!(saw_resolved);
        assert!(engine.gui_approvals.lock().unwrap().is_empty());
    }
}
