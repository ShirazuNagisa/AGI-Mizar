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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use agi_engine::{Answer, Engine, EngineError, EngineEvent};
use agi_protocol::domain::{Interaction, InteractionKind, Record, Session, SessionKey};
use agi_protocol::engine::{Capabilities, EngineId};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::projection;
use crate::rpc::{AppServer, AppServerConfig, RpcEvent};

/// How many thread rows one list request asks for.
const THREAD_PAGE: u64 = 100;

/// How many turns one history read asks for.
const TURN_PAGE: u64 = 100;

/// First restart delay, and the ceiling.
const RESTART_BASE: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(30);

/// One approval Codex is waiting on.
struct PendingApproval {
    /// The JSON-RPC id to answer.
    json_id: Value,
    /// The method, which decides the shape of the answer.
    method: String,
    /// The request's `permissions`, echoed back when a permissions ask is granted.
    permissions: Value,
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
}

impl CodexEngine {
    /// Build the engine. Nothing is spawned until [`CodexEngine::spawn`].
    pub fn new(config: AppServerConfig) -> Arc<Self> {
        let (events, _) = broadcast::channel(512);
        Arc::new(Self {
            server: AppServer::new(config),
            events,
            state: Mutex::new(State::new()),
            approvals: Mutex::new(HashMap::new()),
            next_local: AtomicU64::new(1),
            usage_cache: Mutex::new(agi_engine::usage::ScanCache::new()),
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

            // Serve until the child goes away.
            loop {
                match events.recv().await {
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
            "thread/name/updated"
            | "thread/status/changed"
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
    fn set_running(&self, thread_id: &str, running: bool) {
        let session = {
            let mut state = self.state();
            let Some(session) = state.sessions.get_mut(thread_id) else {
                return;
            };
            session.running = running;
            if running {
                session.turns = session.turns.saturating_add(1);
            }
            session.updated_at = now_ms();
            session.clone()
        };
        self.emit(EngineEvent::SessionUpdated(session));
    }

    /// An approval, or something this adapter does not answer.
    fn on_server_request(self: &Arc<Self>, id: Value, method: String, params: Value) {
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
            },
        );
        self.emit(EngineEvent::InteractionRequested(interaction));
    }

    /// Re-read the thread list into the cache and announce the difference.
    async fn refresh_sessions(&self) -> Result<Vec<Session>, EngineError> {
        let sessions = list_threads(&self.server).await?;
        let mut removed = Vec::new();
        {
            let mut state = self.state();
            let known: Vec<String> = state.sessions.keys().cloned().collect();
            for session in &sessions {
                state
                    .sessions
                    .insert(session.key.id.clone(), session.clone());
            }
            for id in known {
                if !sessions.iter().any(|session| session.key.id == id) {
                    state.sessions.remove(&id);
                    removed.push(SessionKey::new(EngineId::Codex, id));
                }
            }
        }
        for session in &sessions {
            self.emit(EngineEvent::SessionUpdated(session.clone()));
        }
        for key in removed {
            self.emit(EngineEvent::SessionRemoved(key));
        }
        Ok(sessions)
    }
}

/// Read `thread/list` into the shared model.
async fn list_threads(server: &AppServer) -> Result<Vec<Session>, EngineError> {
    let page = server
        .call(
            "thread/list",
            json!({
                "limit": THREAD_PAGE,
                "sortKey": "updated_at",
                "sortDirection": "desc",
                "archived": false,
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

    async fn records(
        &self,
        session: &SessionKey,
        limit: usize,
        before: Option<i64>,
    ) -> Result<Vec<Record>, EngineError> {
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
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

        self.server
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
        Ok(())
    }

    async fn cancel(&self, session: &SessionKey) -> Result<(), EngineError> {
        if session.engine != EngineId::Codex {
            return Err(EngineError::UnknownSession(session.clone()));
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
        let Some(pending) = self.approvals().remove(id) else {
            return Err(EngineError::upstream(format!("no pending approval {id}")));
        };

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
