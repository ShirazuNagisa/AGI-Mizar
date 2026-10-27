//! The Claude Code adapter.
//!
//! Claude Code has no inbound service and no driver protocol. Everything here is
//! built from the three seams it does offer:
//!
//! - **Transcripts** (`~/.claude/projects/<slug>/<session>.jsonl`) are the record.
//!   Reading is a file read; there is nothing to ask.
//! - **Hooks** are the only way to observe a running session, and `PreToolUse` is
//!   the only one that can answer for the operator — it blocks until we reply.
//! - **`claude -p`** starts a turn. One process per prompt: keeping one alive
//!   across turns means speaking a streaming-input protocol, which is a second
//!   contract to keep working across CLI versions.
//!
//! ### What "connected" means here
//!
//! There is no socket to hold, so the engine is always available. What matters is
//! whether a **UI is watching**: a hook that blocks for an approval nobody can
//! answer is worse than no hook at all, so `PreToolUse` only blocks while
//! [`ClaudeEngine::set_attended`] is true.
//!
//! ### Cancelling
//!
//! A `claude` process spawns children of its own. Killing the process group is
//! what actually stops the work; killing the direct child leaves a tool running.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use agi_engine::{Answer, Engine, EngineError, EngineEvent};
use agi_protocol::domain::{Interaction, InteractionKind, Record, Session, SessionKey};
use agi_protocol::engine::{Capabilities, EngineId};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot};

use crate::paths::{self, TranscriptFile};
use crate::transcript;

/// How long a hook may block waiting for the operator.
///
/// Claude Code gives a hook 60 s by default; staying under it means the answer
/// arrives before Claude Code gives up on us.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(55);

/// How much of a transcript is read to guess its title.
const PEEK_BYTES: u64 = 64 * 1024;

/// How far back the title search walks when the head has no prompt in it.
///
/// Measured on a real home: transcripts run from 24 MB to 50 MB, and the tail of
/// one is entirely tool output for hundreds of kilobytes. A single fixed window
/// therefore finds nothing; walking back in chunks stops as soon as it finds the
/// most recent prompt, which is the title.
const PEEK_TAIL_BUDGET: u64 = 2 * 1024 * 1024;

/// One backward step of that walk.
const PEEK_TAIL_CHUNK: u64 = 128 * 1024;

/// How many sessions the list will read metadata for.
const LIST_LIMIT: usize = 300;

/// What the operator decided, as the hook layer sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Allowed,
    Rejected,
}

/// One approval a hook is blocked on.
struct Pending {
    settle: oneshot::Sender<Outcome>,
}

/// What the UI chose for a session, applied on the next prompt.
#[derive(Debug, Clone, Default)]
struct Overrides {
    model: Option<String>,
    permission_mode: Option<String>,
}

struct State {
    sessions: HashMap<String, Session>,
    /// Running `claude -p` children, by session id.
    children: HashMap<String, tokio::process::Child>,
    /// Sessions a hook says are mid-turn.
    ///
    /// Hooks are the only signal that covers a turn started in the operator's own
    /// terminal: the transcript cannot say "still running", it only stops being
    /// appended to. `UserPromptSubmit` opens this and `Stop` closes it.
    active: HashSet<String>,
    overrides: HashMap<String, Overrides>,
}

impl State {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            children: HashMap::new(),
            active: HashSet::new(),
            overrides: HashMap::new(),
        }
    }
}

/// The Claude Code engine.
pub struct ClaudeEngine {
    events: broadcast::Sender<EngineEvent>,
    state: Mutex<State>,
    approvals: Mutex<HashMap<String, Pending>>,
    next_local: AtomicU64,
    /// Whether a UI is watching, which is what makes blocking on a hook safe.
    attended: AtomicBool,
    projects: PathBuf,
    binary: String,
    /// Parsed transcripts, per file, so a fold reads only what changed.
    usage_cache: Mutex<agi_engine::usage::ScanCache>,
    /// A weak handle to self, so a method taking `&self` can start a task that
    /// owns the engine. `Arc::new_cyclic` fills it in after construction.
    me: Weak<ClaudeEngine>,
}

impl ClaudeEngine {
    /// Build the engine over a projects directory and a `claude` executable.
    pub fn new(projects: impl Into<PathBuf>, binary: impl Into<String>) -> Arc<Self> {
        let (events, _) = broadcast::channel(512);
        Arc::new_cyclic(|me| Self {
            events,
            state: Mutex::new(State::new()),
            approvals: Mutex::new(HashMap::new()),
            next_local: AtomicU64::new(1),
            attended: AtomicBool::new(false),
            projects: projects.into(),
            binary: binary.into(),
            usage_cache: Mutex::new(agi_engine::usage::ScanCache::new()),
            me: me.clone(),
        })
    }

    /// Build over the real `~/.claude/projects` and a `claude` found on `PATH`.
    pub fn from_environment() -> Arc<Self> {
        let binary = std::env::var("AGI_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
        Self::new(paths::projects_dir(), binary)
    }

    /// Tell the engine whether a UI is watching.
    pub fn set_attended(&self, attended: bool) {
        self.attended.store(attended, Ordering::Relaxed);
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn approvals(&self) -> MutexGuard<'_, HashMap<String, Pending>> {
        self.approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emit(&self, event: EngineEvent) {
        let _ = self.events.send(event);
    }

    /// The transcript file for a session, when it exists.
    fn transcript_of(&self, session_id: &str) -> Option<TranscriptFile> {
        paths::list_transcripts(&self.projects)
            .into_iter()
            .find(|file| file.session_id == session_id)
    }

    /// Re-read the session list.
    fn refresh_sessions(&self) -> Vec<Session> {
        let running: Vec<String> = {
            let state = self.state();
            state
                .children
                .keys()
                .chain(state.active.iter())
                .cloned()
                .collect()
        };
        let mut sessions = Vec::new();

        for file in paths::list_transcripts(&self.projects)
            .into_iter()
            .take(LIST_LIMIT)
        {
            let meta = peek(&file.path);
            let mut session = Session::bare(
                SessionKey::new(EngineId::Claude, &file.session_id),
                file.mtime,
            );
            session.cwd = meta.cwd;
            session.title = meta.custom_title.or(meta.title);
            session.model = meta.model;
            session.running = running.contains(&file.session_id);
            session.turns = meta.turns;
            sessions.push(session);
        }

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
                }
            }
        }

        sessions
    }

    /// Tell the UI that one session's row changed.
    fn touch(&self, session_id: &str) {
        let session = {
            let mut state = self.state();
            let Some(session) = state.sessions.get_mut(session_id) else {
                return;
            };
            session.updated_at = now_ms();
            session.clone()
        };
        self.emit(EngineEvent::SessionUpdated(session));
    }

    /// Handle one hook invocation.
    ///
    /// Returns the JSON Claude Code should read from the hook's stdout, or `None`
    /// when the hook has nothing to say. A hook that returns nothing is a hook
    /// that let Claude Code decide — which is what every failure here does.
    pub async fn handle_hook(&self, event: &str, payload: &Value) -> Option<Value> {
        let session = payload
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string);

        match event {
            "PreToolUse" => self.pre_tool_use(payload, session).await,
            // Everything else is a hint that a row changed. The transcript is
            // the truth; this only decides when to look at it again — and, for
            // the two lifecycle hooks, whether the turn is still open.
            "UserPromptSubmit" | "SessionStart" => {
                if let Some(id) = &session {
                    self.state().active.insert(id.clone());
                }
                if let Some(id) = session {
                    self.refresh_sessions();
                    self.touch(&id);
                }
                None
            }
            "Stop" | "SessionEnd" => {
                if let Some(id) = &session {
                    self.state().active.remove(id);
                }
                if let Some(id) = session {
                    self.refresh_sessions();
                    self.touch(&id);
                }
                None
            }
            "SubagentStop" | "Notification" | "PreCompact" => {
                if let Some(id) = session {
                    self.refresh_sessions();
                    self.touch(&id);
                }
                None
            }
            _ => None,
        }
    }

    /// The one hook that can answer for the operator.
    async fn pre_tool_use(&self, payload: &Value, session: Option<String>) -> Option<Value> {
        // Nobody is watching: blocking for a decision nobody can make would stall
        // the operator's own terminal.
        if !self.attended.load(Ordering::Relaxed) {
            return None;
        }

        // Only when Claude Code was going to ask anyway. In `acceptEdits` the
        // operator already approved edits, in `plan` nothing runs, and in
        // `bypassPermissions` there is no question to relay — answering one there
        // would be us granting what they deliberately left ungated.
        let mode = payload
            .get("permission_mode")
            .and_then(Value::as_str)
            .unwrap_or("default");
        if mode != "default" {
            return None;
        }

        let tool = payload.get("tool_name").and_then(Value::as_str)?;
        if tool.is_empty() {
            return None;
        }

        let local = format!("cl-{}", self.next_local.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.approvals()
            .insert(local.clone(), Pending { settle: tx });

        let interaction = Interaction {
            id: local.clone(),
            session: session.map(|id| SessionKey::new(EngineId::Claude, id)),
            kind: InteractionKind::Approval,
            title: tool.to_string(),
            detail: payload.get("tool_input").and_then(describe_tool_input),
            requested_at: now_ms(),
            questions: Vec::new(),
        };
        self.emit(EngineEvent::InteractionRequested(interaction));

        let outcome = tokio::time::timeout(APPROVAL_TIMEOUT, rx).await;
        // Whatever happened, the card must go: an answered one was removed by
        // `answer`, and an unanswered one is a decision that can no longer be
        // made.
        self.approvals().remove(&local);

        match outcome {
            Ok(Ok(Outcome::Allowed)) => Some(decision("allow", "Approved in AGI Mizar.")),
            Ok(Ok(Outcome::Rejected)) => Some(decision("deny", "Rejected in AGI Mizar.")),
            // A timeout or a dropped sender claims nothing: Claude Code asks its
            // own question, which is the behaviour the operator had before.
            _ => None,
        }
    }

    /// Start a turn for one session.
    async fn run_prompt(
        self: &Arc<Self>,
        session: &SessionKey,
        text: &str,
    ) -> Result<(), EngineError> {
        let existing = self.transcript_of(session.raw());
        let overrides = self
            .state()
            .overrides
            .get(&session.id)
            .cloned()
            .unwrap_or_default();

        let mut command = tokio::process::Command::new(&self.binary);
        command
            .arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .args(if existing.is_some() {
                ["--resume", session.raw()]
            } else {
                ["--session-id", session.raw()]
            })
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        if let Some(model) = overrides.model {
            command.args(["--model", &model]);
        }
        if let Some(mode) = overrides.permission_mode {
            command.args(["--permission-mode", &mode]);
        }
        if let Some(cwd) = self
            .state()
            .sessions
            .get(&session.id)
            .and_then(|session| session.cwd.clone())
        {
            command.current_dir(cwd);
        }

        let mut child = command.spawn().map_err(|error| {
            EngineError::upstream(format!("could not start {}: {error}", self.binary))
        })?;

        // The prompt goes in on stdin, not as an argument: a prompt carrying a
        // file list can exceed a platform's argument limit, and a failed spawn is
        // a message the operator loses.
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let text = text.to_string();
            tokio::spawn(async move {
                let _ = stdin.write_all(text.as_bytes()).await;
                let _ = stdin.shutdown().await;
            });
        }

        // Drain the stream so the child cannot block on a full pipe. The
        // transcript is the record; this output is only useful for logging.
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(frame) = serde_json::from_str::<Value>(&line) {
                        if frame.get("type").and_then(Value::as_str) == Some("result") {
                            tracing::debug!("claude: run finished");
                        }
                    }
                }
            });
        }

        self.state().children.insert(session.id.clone(), child);

        // Watch for the end, then refresh: the transcript is what changed.
        let engine = Arc::clone(self);
        let id = session.id.clone();
        tokio::spawn(async move {
            loop {
                let exited = {
                    let mut state = engine.state();
                    match state.children.get_mut(&id) {
                        Some(child) => match child.try_wait() {
                            Ok(Some(_)) => {
                                state.children.remove(&id);
                                true
                            }
                            Ok(None) => false,
                            Err(_) => {
                                state.children.remove(&id);
                                true
                            }
                        },
                        None => true,
                    }
                };
                if exited {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            engine.refresh_sessions();
            engine.touch(&id);
        });

        Ok(())
    }
}

/// The JSON a `PreToolUse` hook prints to answer.
fn decision(decision: &str, reason: &str) -> Value {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    })
}

/// A one-line description of a tool call, for the approval card.
fn describe_tool_input(input: &Value) -> Option<String> {
    let text = match input {
        Value::String(text) => text.clone(),
        Value::Object(map) => {
            for key in ["command", "file_path", "path", "pattern", "url"] {
                if let Some(value) = map.get(key).and_then(Value::as_str) {
                    return Some(truncate(value));
                }
            }
            truncate(&input.to_string())
        }
        other => truncate(&other.to_string()),
    };
    Some(text)
}

/// Cut a description to something a card can show.
fn truncate(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= 160 {
        line.to_string()
    } else {
        let mut shortened: String = line.chars().take(157).collect();
        shortened.push('…');
        shortened
    }
}

/// Metadata a session list needs, without parsing the whole file.
#[derive(Debug, Default)]
struct Meta {
    title: Option<String>,
    custom_title: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    turns: u32,
}

/// Read the head of a transcript for its title, cwd and model.
///
/// Deliberately not a full parse: a list of three hundred sessions must not read
/// three hundred megabytes. A `custom-title` written later in the file is missed
/// here and picked up when the session is opened, which is the right trade.
fn peek(path: &Path) -> Meta {
    use std::io::Read;

    let Ok(mut file) = std::fs::File::open(path) else {
        return Meta::default();
    };
    let mut buffer = vec![0u8; PEEK_BYTES as usize];
    let Ok(read) = file.read(&mut buffer) else {
        return Meta::default();
    };
    buffer.truncate(read);
    let text = String::from_utf8_lossy(&buffer);

    let mut meta = Meta::default();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if meta.cwd.is_none() {
            meta.cwd = value.get("cwd").and_then(Value::as_str).map(str::to_string);
        }
        if value.get("type").and_then(Value::as_str) == Some("custom-title") {
            meta.custom_title = value
                .get("customTitle")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if value.get("type").and_then(Value::as_str) == Some("assistant") && meta.model.is_none() {
            meta.model = value
                .get("message")
                .and_then(|message| message.get("model"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if meta.title.is_none() && transcript::is_human_prompt(&value) {
            let text = value
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            if !text.trim().is_empty() {
                meta.turns += 1;
                if meta.title.is_none() {
                    meta.title = transcript::title_of(&text);
                }
            }
        }
    }
    if meta.title.is_none() {
        meta.title = last_prompt_in_tail(path);
    }
    meta
}

/// The most recent human prompt, walking back from the end of the file.
///
/// Returns as soon as a chunk yields one: a chunk closer to the end contains a
/// later prompt, so the first chunk that has any has the newest.
fn last_prompt_in_tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let mut end = size;
    let mut budget = PEEK_TAIL_BUDGET;

    while end > 0 && budget > 0 {
        let start = end.saturating_sub(PEEK_TAIL_CHUNK);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut buffer = vec![0u8; (end - start) as usize];
        let read = file.read(&mut buffer).ok()?;
        buffer.truncate(read);
        let text = String::from_utf8_lossy(&buffer);

        let mut found = None;
        // A chunk that does not begin at the file start begins mid-line.
        for line in text.lines().skip(if start == 0 { 0 } else { 1 }) {
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if !transcript::is_human_prompt(&value) {
                continue;
            }
            if let Some(first) = first_line(&value) {
                found = Some(first);
            }
        }
        if found.is_some() {
            return found;
        }

        budget = budget.saturating_sub(end - start);
        end = start;
    }
    None
}

/// The first line of a prompt, shortened for a title.
fn first_line(value: &Value) -> Option<String> {
    let content = value
        .get("message")
        .and_then(|message| message.get("content"))?;
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    if transcript::looks_injected(&text) {
        return None;
    }
    let first = text.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return None;
    }
    Some(if first.chars().count() <= 60 {
        first.to_string()
    } else {
        first.chars().take(57).collect::<String>() + "…"
    })
}

/// Epoch milliseconds.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[async_trait]
impl Engine for ClaudeEngine {
    fn id(&self) -> EngineId {
        EngineId::Claude
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
        // There is no link to drop: the transcripts are read straight off disk,
        // so the next read is the reconnection.
        Ok(())
    }

    fn status(&self) -> (bool, Option<String>) {
        // There is nothing to connect to: the engine is usable exactly when a
        // projects directory exists, and a missing one is the normal state on a
        // machine that has never run Claude Code.
        (
            true,
            Some(if self.projects.is_dir() {
                format!("transcripts at {}", self.projects.display())
            } else {
                format!("no transcripts yet at {}", self.projects.display())
            }),
        )
    }

    async fn sessions(&self) -> Result<Vec<Session>, EngineError> {
        let sessions = self.refresh_sessions();
        for session in &sessions {
            self.emit(EngineEvent::SessionUpdated(session.clone()));
        }
        Ok(sessions)
    }

    async fn records(
        &self,
        session: &SessionKey,
        limit: usize,
        before: Option<i64>,
    ) -> Result<Vec<Record>, EngineError> {
        if session.engine != EngineId::Claude {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        let Some(file) = self.transcript_of(session.raw()) else {
            return Err(EngineError::UnknownSession(session.clone()));
        };
        let text = std::fs::read_to_string(&file.path).map_err(|error| {
            EngineError::upstream(format!("could not read the transcript: {error}"))
        })?;
        let running = {
            let state = self.state();
            state.children.contains_key(&session.id) || state.active.contains(&session.id)
        };
        let parsed = transcript::parse(&text, running);

        // A full read is the authoritative source for a title. The list guesses
        // from a window; a session whose first prompt sits behind megabytes of
        // tool output only learns its name when somebody opens it.
        if let Some(title) = &parsed.title {
            let changed = {
                let mut state = self.state();
                match state.sessions.get_mut(&session.id) {
                    Some(cached) if cached.title.as_deref() != Some(title.as_str()) => {
                        cached.title = Some(title.clone());
                        Some(cached.clone())
                    }
                    _ => None,
                }
            };
            if let Some(session) = changed {
                self.emit(EngineEvent::SessionUpdated(session));
            }
        }

        let mut records = parsed.records;
        if let Some(before) = before {
            records.retain(|record| record.seq < before);
        }
        let start = records.len().saturating_sub(limit);
        Ok(records.split_off(start))
    }

    /// Every day of activity in every transcript.
    ///
    /// A directory read rather than a call: Claude Code writes the usage the
    /// provider billed into each `assistant` line, so there is nothing to ask.
    async fn usage_history(&self) -> Result<Vec<agi_protocol::api::UsageDay>, EngineError> {
        let mut cache = self
            .usage_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(crate::usage::days_with_cache(&self.projects, &mut cache))
    }

    async fn prompt(&self, session: &SessionKey, text: &str) -> Result<(), EngineError> {
        if session.engine != EngineId::Claude {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        // One process per prompt, so a session that is already running has no
        // room for a second. Saying so is better than a queue nobody can see.
        if self.state().children.contains_key(&session.id) {
            return Err(EngineError::Upstream {
                code: "WRITER_HELD".to_string(),
                message: "this session is already running a turn".to_string(),
                details: None,
            });
        }
        let Some(engine) = self.me.upgrade() else {
            return Err(EngineError::upstream("the engine is shutting down"));
        };
        engine.run_prompt(session, text).await
    }

    async fn cancel(&self, session: &SessionKey) -> Result<(), EngineError> {
        if session.engine != EngineId::Claude {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        let mut state = self.state();
        let Some(mut child) = state.children.remove(&session.id) else {
            // Nothing running is not a failure.
            return Ok(());
        };
        // Killing the direct child leaves the tools it spawned running; the
        // process group is the unit that actually stops the work.
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &format!("-{pid}")])
                .status();
        }
        let _ = child.start_kill();
        Ok(())
    }

    async fn answer(&self, id: &str, answer: Answer) -> Result<(), EngineError> {
        let Some(pending) = self.approvals().remove(id) else {
            return Err(EngineError::upstream(format!("no pending approval {id}")));
        };
        let outcome = if answer.outcome.as_deref() == Some("allowed-once") {
            Outcome::Allowed
        } else {
            Outcome::Rejected
        };
        let _ = pending.settle.send(outcome);
        self.emit(EngineEvent::InteractionResolved { id: id.to_string() });
        Ok(())
    }

    async fn set_model(&self, session: &SessionKey, model: &str) -> Result<(), EngineError> {
        if session.engine != EngineId::Claude {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        // Claude Code has no live model switch on a running turn: the choice is
        // an argument to the next process. Storing it is honest; pretending it
        // applied now would not be.
        self.state()
            .overrides
            .entry(session.id.clone())
            .or_default()
            .model = Some(model.to_string());
        Ok(())
    }

    async fn set_permission(&self, session: &SessionKey, preset: &str) -> Result<(), EngineError> {
        if session.engine != EngineId::Claude {
            return Err(EngineError::UnknownSession(session.clone()));
        }
        self.state()
            .overrides
            .entry(session.id.clone())
            .or_default()
            .permission_mode = Some(preset.to_string());
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }
}
