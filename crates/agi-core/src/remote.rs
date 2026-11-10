//! Delegations from this desktop to same-account peers.
//!
//! The manager owns what the kernel can show and what survives a restart; the
//! wire lives in `agi-peer`, the token minting in `account`. Keeping the three
//! apart is what lets the persistence tests run without a relay and the wire
//! tests run without a kernel.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agi_peer::{Credentials, PeerClient, PeerError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

/// How many delegations may be running at once.
pub const MAX_ACTIVE: usize = 4;

/// How long one remote turn may take before the manager gives up waiting.
///
/// The remote session is not cancelled on this timeout; the caller decides.
pub const TURN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How much of a final answer is kept in a result/notice.
pub const MAX_FINAL_BYTES: usize = 32 * 1024;

/// How many finished delegations are kept in memory and on disk.
const MAX_HISTORY: usize = 50;

/// What a delegation is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteState {
    Running,
    Done,
    Failed,
    Cancelled,
    Interrupted,
}

impl RemoteState {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running)
    }
}

/// The choices that shape one new remote session.
#[derive(Debug, Clone, Default)]
pub struct RemoteLaunch {
    pub cwd: Option<String>,
    pub permission: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

/// One delegation the window/tool can read.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Delegation {
    pub id: String,
    pub peer: String,
    pub peer_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Permission preset applied to the remote session, when one was requested.
    #[serde(default)]
    pub permission: Option<String>,
    /// Model route selected for the remote session, when one was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// LAN address used for this delegation, when the route is `lan`.
    #[serde(default)]
    pub address: Option<std::net::SocketAddr>,
    pub state: RemoteState,
    /// `wan` or `lan`; old records default to `wan`.
    #[serde(default = "default_route")]
    pub route: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The first line of the prompt, capped. The full prompt is never persisted.
    pub prompt_preview: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The pending approval holding this turn, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<PendingApproval>,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
}

fn default_route() -> String {
    "wan".to_string()
}

/// One approval waiting on the parent model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingApproval {
    pub request_id: String,
    pub risk: String,
    #[serde(default)]
    pub reason: String,
    pub requested_at: i64,
}

/// One audit line. No prompt and no final text: those are content, not events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEvent {
    pub at_ms: i64,
    pub delegation_id: String,
    pub peer: String,
    pub route: String,
    /// `started`, `approval`, `answered`, `settled`.
    pub event: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A peer target, already checked against the account's desktop list.
#[derive(Debug, Clone)]
pub struct PeerRef {
    pub device_id: String,
    pub name: String,
}

/// Where a settled delegation should be announced (the DSH link, when there is
/// one). Called outside the record lock.
pub type NoticeSink = Arc<dyn Fn(&Delegation, &str) + Send + Sync>;

pub struct RemoteManager {
    path: Mutex<Option<PathBuf>>,
    records: Mutex<BTreeMap<String, Delegation>>,
    cancels: Mutex<HashMap<String, watch::Sender<bool>>>,
    notices: Mutex<Option<NoticeSink>>,
    audit_path: Mutex<Option<PathBuf>>,
    audit: Mutex<std::collections::VecDeque<AuditEvent>>,
}

impl RemoteManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            path: Mutex::new(None),
            records: Mutex::new(BTreeMap::new()),
            cancels: Mutex::new(HashMap::new()),
            notices: Mutex::new(None),
            audit_path: Mutex::new(None),
            audit: Mutex::new(std::collections::VecDeque::new()),
        })
    }

    /// Install (or clear) the settlement announcement sink.
    pub fn set_notice_sink(&self, sink: NoticeSink) {
        *self
            .notices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sink);
    }

    fn announce(&self, record: &Delegation, kind: &str) {
        let sink = self
            .notices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(sink) = sink {
            sink(record, kind);
        }
    }

    /// Record one approval the remote turn is waiting on, once.
    pub fn observe_approval(&self, id: &str, approval: &Value) {
        let Some(request_id) = approval.get("requestId").and_then(Value::as_str) else {
            return;
        };
        let risk = approval
            .get("risk")
            .and_then(Value::as_str)
            .unwrap_or("high")
            .to_string();
        let reason = approval
            .get("riskReason")
            .and_then(Value::as_str)
            .or_else(|| approval.get("reason").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let requested_at = approval
            .get("requestedAt")
            .and_then(Value::as_i64)
            .unwrap_or_else(now_ms);
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(record) = records.get_mut(id) else {
            return;
        };
        if !record.state.is_active() {
            return;
        }
        let next = PendingApproval {
            request_id: request_id.to_string(),
            risk,
            reason,
            requested_at,
        };
        if record.approval.as_ref() == Some(&next) {
            return;
        }
        record.approval = Some(next);
        let notice = record.clone();
        drop(records);
        self.save();
        self.record_audit(
            id,
            &notice.peer,
            &notice.route,
            "approval",
            notice.state,
            notice
                .approval
                .as_ref()
                .map(|approval| approval.risk.clone()),
            notice
                .approval
                .as_ref()
                .map(|approval| approval.reason.clone()),
        );
        self.announce(&notice, "approval");
    }

    /// Drop the pending approval after the model answered it, recording which
    /// outcome was sent.
    pub fn clear_approval(&self, id: &str, outcome: &str) {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(record) = records.get_mut(id) else {
            return;
        };
        let risk = record
            .approval
            .as_ref()
            .map(|approval| approval.risk.clone());
        let peer = record.peer.clone();
        let route = record.route.clone();
        let state = record.state;
        record.approval = None;
        drop(records);
        self.record_audit(
            id,
            &peer,
            &route,
            "answered",
            state,
            risk,
            Some(outcome.to_string()),
        );
    }

    /// One record, cloned for a command that must not hold the lock.
    pub fn get(&self, id: &str) -> Option<Delegation> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
    }

    /// Point the audit at a file. Boot calls this once; tests may skip it.
    pub fn set_audit_path(&self, path: PathBuf) {
        *self
            .audit_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
    }

    /// Where the audit lines are being written, once boot pointed us there.
    pub fn audit_path(&self) -> Option<PathBuf> {
        self.audit_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The most recent audit lines, newest last.
    pub fn audit(&self, limit: usize) -> Vec<AuditEvent> {
        let audit = self
            .audit
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let skip = audit.len().saturating_sub(limit.min(500));
        audit.iter().skip(skip).cloned().collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn record_audit(
        &self,
        id: &str,
        peer: &str,
        route: &str,
        event: &str,
        state: RemoteState,
        risk: Option<String>,
        note: Option<String>,
    ) {
        let line = AuditEvent {
            at_ms: now_ms(),
            delegation_id: id.to_string(),
            peer: peer.to_string(),
            route: route.to_string(),
            event: event.to_string(),
            state: format!("{state:?}").to_lowercase(),
            risk,
            note,
        };
        {
            let mut audit = self
                .audit
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            audit.push_back(line.clone());
            while audit.len() > 500 {
                audit.pop_front();
            }
        }
        let path = self
            .audit_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(path) = path else {
            return;
        };
        let Ok(mut json) = serde_json::to_string(&line) else {
            return;
        };
        json.push('\n');
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Keep one previous file; an audit log is for reading, not for growing
        // without bound. Five MiB is months of events at this rate.
        if std::fs::metadata(&path).is_ok_and(|meta| meta.len() >= 5 * 1024 * 1024) {
            let _ = std::fs::rename(&path, path.with_extension("jsonl.1"));
        }
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        else {
            return;
        };
        use std::io::Write;
        let _ = file.write_all(json.as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }

    /// Read the persisted records, marking anything that was running as
    /// interrupted: this process has no live task for them any more.
    pub fn load(&self, path: PathBuf) {
        let records = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Vec<Delegation>>(&text).ok())
            .unwrap_or_default();
        let mut map = BTreeMap::new();
        for mut record in records {
            if record.state == RemoteState::Running {
                record.state = RemoteState::Interrupted;
                record.ended_at.get_or_insert_with(now_ms);
            }
            map.insert(record.id.clone(), record);
        }
        *self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = map;
        *self
            .path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
        self.save();
    }

    /// Start one delegation and return its record immediately.
    ///
    /// The remote work continues in a task. A rejected start leaves no record.
    pub fn start(
        self: &Arc<Self>,
        peer: PeerRef,
        credentials: Credentials,
        cwd: Option<String>,
        prompt: String,
    ) -> Result<Delegation, String> {
        self.start_with_options(
            peer,
            credentials,
            RemoteLaunch {
                cwd,
                ..RemoteLaunch::default()
            },
            prompt,
        )
    }

    /// Start a WAN delegation with explicit workspace, permission and model.
    pub fn start_with_options(
        self: &Arc<Self>,
        peer: PeerRef,
        credentials: Credentials,
        launch: RemoteLaunch,
        prompt: String,
    ) -> Result<Delegation, String> {
        self.start_wan(peer, credentials, launch, prompt, None)
    }

    /// Continue an existing peer session with another prompt.
    pub fn start_with_session(
        self: &Arc<Self>,
        peer: PeerRef,
        credentials: Credentials,
        cwd: Option<String>,
        prompt: String,
        session: String,
    ) -> Result<Delegation, String> {
        self.start_wan(
            peer,
            credentials,
            RemoteLaunch {
                cwd,
                ..RemoteLaunch::default()
            },
            prompt,
            Some(session),
        )
    }

    fn start_wan(
        self: &Arc<Self>,
        peer: PeerRef,
        credentials: Credentials,
        launch: RemoteLaunch,
        prompt: String,
        session: Option<String>,
    ) -> Result<Delegation, String> {
        let running = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter(|record| record.state.is_active())
            .count();
        if running >= MAX_ACTIVE {
            return Err(format!(
                "too many remote delegations are already running ({MAX_ACTIVE})"
            ));
        }
        let per_peer = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter(|record| record.state.is_active() && record.peer == peer.device_id)
            .count();
        if per_peer >= 2 {
            return Err("that computer already has two running delegations".to_string());
        }
        if prompt.trim().is_empty() {
            return Err("the remote task is empty".to_string());
        }

        let id = uuid::Uuid::new_v4().to_string();
        let record = Delegation {
            id: id.clone(),
            peer: peer.device_id.clone(),
            peer_name: peer.name.clone(),
            session_id: session.clone(),
            permission: launch.permission.clone(),
            provider: launch.provider.clone(),
            model: launch.model.clone(),
            reasoning_effort: launch.reasoning_effort.clone(),
            address: None,
            state: RemoteState::Running,
            route: "wan".to_string(),
            cwd: launch.cwd.clone(),
            prompt_preview: preview(&prompt),
            final_text: None,
            error: None,
            reason: None,
            approval: None,
            started_at: now_ms(),
            ended_at: None,
        };
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), record.clone());
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.cancels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), cancel_tx);
        self.prune();
        self.save();
        self.record_audit(
            &id,
            &record.peer,
            &record.route,
            "started",
            RemoteState::Running,
            None,
            record.cwd.clone(),
        );

        let manager = Arc::clone(self);
        let task_id = id.clone();
        let task_prompt = prompt;
        tokio::spawn(async move {
            let outcome = run_one(
                &manager,
                &task_id,
                credentials,
                launch,
                &task_prompt,
                session,
                cancel_rx,
            )
            .await;
            manager.finish(&task_id, outcome);
        });
        Ok(record)
    }

    /// Every record, newest first.
    pub fn list(&self) -> Vec<Delegation> {
        let mut records: Vec<Delegation> = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        records.sort_by_key(|left| std::cmp::Reverse(left.started_at));
        records
    }

    /// Start one delegation over the LAN route.
    pub fn start_lan(
        self: &Arc<Self>,
        peer: PeerRef,
        address: std::net::SocketAddr,
        credentials: agi_lan::LanCredentials,
        cwd: Option<String>,
        prompt: String,
    ) -> Result<Delegation, String> {
        self.start_lan_with_options(
            peer,
            address,
            credentials,
            RemoteLaunch {
                cwd,
                ..RemoteLaunch::default()
            },
            prompt,
        )
    }

    /// Start a LAN delegation with explicit workspace, permission and model.
    pub fn start_lan_with_options(
        self: &Arc<Self>,
        peer: PeerRef,
        address: std::net::SocketAddr,
        credentials: agi_lan::LanCredentials,
        launch: RemoteLaunch,
        prompt: String,
    ) -> Result<Delegation, String> {
        self.start_lan_inner(peer, address, credentials, launch, prompt, None)
    }

    /// Continue an existing peer session over LAN.
    pub fn start_lan_with_session(
        self: &Arc<Self>,
        peer: PeerRef,
        address: std::net::SocketAddr,
        credentials: agi_lan::LanCredentials,
        cwd: Option<String>,
        prompt: String,
        session: String,
    ) -> Result<Delegation, String> {
        self.start_lan_inner(
            peer,
            address,
            credentials,
            RemoteLaunch {
                cwd,
                ..RemoteLaunch::default()
            },
            prompt,
            Some(session),
        )
    }

    fn start_lan_inner(
        self: &Arc<Self>,
        peer: PeerRef,
        address: std::net::SocketAddr,
        credentials: agi_lan::LanCredentials,
        launch: RemoteLaunch,
        prompt: String,
        session: Option<String>,
    ) -> Result<Delegation, String> {
        let running = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter(|record| record.state.is_active())
            .count();
        if running >= MAX_ACTIVE {
            return Err(format!(
                "too many remote delegations are already running ({MAX_ACTIVE})"
            ));
        }
        let per_peer = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter(|record| record.state.is_active() && record.peer == peer.device_id)
            .count();
        if per_peer >= 2 {
            return Err("that computer already has two running delegations".to_string());
        }
        if prompt.trim().is_empty() {
            return Err("the remote task is empty".to_string());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let record = Delegation {
            id: id.clone(),
            peer: peer.device_id.clone(),
            peer_name: peer.name,
            session_id: session.clone(),
            permission: launch.permission.clone(),
            provider: launch.provider.clone(),
            model: launch.model.clone(),
            reasoning_effort: launch.reasoning_effort.clone(),
            address: Some(address),
            state: RemoteState::Running,
            route: "lan".to_string(),
            cwd: launch.cwd.clone(),
            prompt_preview: preview(&prompt),
            final_text: None,
            error: None,
            reason: None,
            approval: None,
            started_at: now_ms(),
            ended_at: None,
        };
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), record.clone());
        self.save();
        self.record_audit(
            &id,
            &record.peer,
            &record.route,
            "started",
            RemoteState::Running,
            None,
            record.cwd.clone(),
        );
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.cancels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), cancel_tx);
        let manager = Arc::clone(self);
        let task_id = id.clone();
        tokio::spawn(async move {
            let outcome = run_lan_one(
                &manager,
                &task_id,
                address,
                credentials,
                launch,
                &prompt,
                session,
                cancel_rx,
            )
            .await;
            manager.finish(&task_id, outcome);
        });
        Ok(record)
    }

    /// Ask the remote turn to stop. Returns whether a live task was signalled.
    pub fn cancel(&self, id: &str) -> bool {
        let sender = self
            .cancels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
        let signalled = sender.is_some();
        if let Some(sender) = sender {
            let _ = sender.send(true);
        }
        let notice = {
            let mut records = self
                .records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(record) = records.get_mut(id) {
                if record.state.is_active() {
                    record.state = RemoteState::Cancelled;
                    record.reason = Some("cancelled".to_string());
                    record.ended_at = Some(now_ms());
                    Some(record.clone())
                } else {
                    None
                }
            } else {
                None
            }
        };
        self.save();
        if let Some(notice) = notice {
            self.record_audit(
                id,
                &notice.peer,
                &notice.route,
                "settled",
                RemoteState::Cancelled,
                None,
                Some("cancelled".to_string()),
            );
            self.announce(&notice, "settled");
        }
        signalled
    }

    fn finish(&self, id: &str, outcome: Result<TurnOutcome, RunFailure>) {
        self.cancels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(record) = records.get_mut(id) else {
            return;
        };
        // A cancel that landed while the task was unwinding wins.
        if !record.state.is_active() {
            return;
        }
        match outcome {
            Ok(turn) => {
                record.state = RemoteState::Done;
                record.reason = Some(turn.reason);
                record.final_text = turn.final_text.map(cap);
                record.ended_at = Some(now_ms());
            }
            Err(failure) => {
                record.state = failure.state;
                record.error = Some(failure.message);
                record.reason = failure.reason;
                record.ended_at = Some(now_ms());
            }
        }
        record.approval = None;
        let session = record.session_id.clone();
        let notice = record.clone();
        drop(records);
        if let Some(session) = session {
            tracing::debug!("remote delegation {id} settled for {session}");
        }
        self.save();
        self.record_audit(
            id,
            &notice.peer,
            &notice.route,
            "settled",
            notice.state,
            None,
            notice.error.clone(),
        );
        self.announce(&notice, "settled");
    }

    fn set_session(&self, id: &str, session: String) {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = records.get_mut(id) {
            record.session_id = Some(session);
        }
        drop(records);
        self.save();
    }

    fn prune(&self) {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let finished: Vec<String> = {
            let mut finished: Vec<&Delegation> = records
                .values()
                .filter(|record| !record.state.is_active())
                .collect();
            finished.sort_by_key(|record| record.started_at);
            finished
                .into_iter()
                .rev()
                .skip(MAX_HISTORY)
                .map(|record| record.id.clone())
                .collect()
        };
        for id in finished {
            records.remove(&id);
        }
    }

    fn save(&self) {
        let Some(path) = self
            .path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return;
        };
        let records: Vec<Delegation> = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        let Ok(bytes) = serde_json::to_vec_pretty(&records) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let temp = path.with_extension("json.tmp");
        if std::fs::write(&temp, &bytes).is_err() {
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600));
        }
        let _ = std::fs::rename(&temp, &path);
    }
}

struct TurnOutcome {
    reason: String,
    final_text: Option<String>,
}

struct RunFailure {
    state: RemoteState,
    message: String,
    reason: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn run_one(
    manager: &RemoteManager,
    id: &str,
    credentials: Credentials,
    launch: RemoteLaunch,
    prompt: &str,
    existing_session: Option<String>,
    cancel: watch::Receiver<bool>,
) -> Result<TurnOutcome, RunFailure> {
    let client = PeerClient::connect(credentials)
        .await
        .map_err(|error| RunFailure {
            state: RemoteState::Failed,
            message: error.to_string(),
            reason: Some("connect".to_string()),
        })?;
    let session = match existing_session {
        Some(session) => session,
        None => client
            .create_session(launch.cwd.as_deref())
            .await
            .map_err(|error| RunFailure {
                state: RemoteState::Failed,
                message: error.to_string(),
                reason: Some("session.create".to_string()),
            })?,
    };
    manager.set_session(id, session.clone());
    if let (Some(provider), Some(model)) = (launch.provider.as_deref(), launch.model.as_deref()) {
        client
            .set_model(
                &session,
                provider,
                model,
                launch.reasoning_effort.as_deref(),
            )
            .await
            .map_err(|error| RunFailure {
                state: RemoteState::Failed,
                message: error.to_string(),
                reason: Some("session.model.set".to_string()),
            })?;
    }
    if let Some(preset) = launch.permission.as_deref() {
        client
            .set_permission(&session, preset)
            .await
            .map_err(|error| RunFailure {
                state: RemoteState::Failed,
                message: error.to_string(),
                reason: Some("session.permission.set".to_string()),
            })?;
    }
    let turn = client
        .run_turn_with(&session, prompt, TURN_TIMEOUT, cancel, |approval| {
            manager.observe_approval(id, approval);
        })
        .await;
    match turn {
        Ok(turn) => Ok(TurnOutcome {
            reason: turn.reason,
            final_text: turn.final_text.map(cap),
        }),
        Err(PeerError::Other(message)) if message == "cancelled" => Err(RunFailure {
            state: RemoteState::Cancelled,
            message: "the delegation was cancelled".to_string(),
            reason: Some("cancelled".to_string()),
        }),
        Err(error) => Err(RunFailure {
            state: RemoteState::Failed,
            message: error.to_string(),
            reason: Some("turn".to_string()),
        }),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_lan_one(
    manager: &RemoteManager,
    id: &str,
    address: std::net::SocketAddr,
    credentials: agi_lan::LanCredentials,
    launch: RemoteLaunch,
    prompt: &str,
    existing_session: Option<String>,
    cancel: watch::Receiver<bool>,
) -> Result<TurnOutcome, RunFailure> {
    let client = agi_lan::LanMizarClient::connect(address, &credentials)
        .await
        .map_err(|error| RunFailure {
            state: RemoteState::Failed,
            message: error.to_string(),
            reason: Some("lan.connect".to_string()),
        })?;
    let session = match existing_session {
        Some(session) => session,
        None => client
            .create_session(launch.cwd.as_deref())
            .await
            .map_err(|error| RunFailure {
                state: RemoteState::Failed,
                message: error.to_string(),
                reason: Some("lan.session.create".to_string()),
            })?,
    };
    manager.set_session(id, session.clone());
    if let (Some(provider), Some(model)) = (launch.provider.as_deref(), launch.model.as_deref()) {
        client
            .set_model(
                &session,
                provider,
                model,
                launch.reasoning_effort.as_deref(),
            )
            .await
            .map_err(|error| RunFailure {
                state: RemoteState::Failed,
                message: error.to_string(),
                reason: Some("lan.session.model.set".to_string()),
            })?;
    }
    if let Some(preset) = launch.permission.as_deref() {
        client
            .set_permission(&session, preset)
            .await
            .map_err(|error| RunFailure {
                state: RemoteState::Failed,
                message: error.to_string(),
                reason: Some("session.permission.set".to_string()),
            })?;
    }
    let turn = client
        .run_turn(&session, prompt, TURN_TIMEOUT, cancel, |approval| {
            manager.observe_approval(id, approval);
        })
        .await;
    match turn {
        Ok(turn) => Ok(TurnOutcome {
            reason: turn.reason,
            final_text: turn.final_text.map(cap),
        }),
        Err(agi_lan::LanError::Remote(message)) if message == "cancelled" => Err(RunFailure {
            state: RemoteState::Cancelled,
            message: "the delegation was cancelled".to_string(),
            reason: Some("cancelled".to_string()),
        }),
        Err(error) => Err(RunFailure {
            state: RemoteState::Failed,
            message: error.to_string(),
            reason: Some("lan.turn".to_string()),
        }),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn preview(prompt: &str) -> String {
    let first = prompt
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let mut out: String = first.chars().take(200).collect();
    if first.chars().count() > 200 {
        out.push('…');
    }
    out
}

fn cap(mut text: String) -> String {
    if text.len() > MAX_FINAL_BYTES {
        let boundary = floor_char_boundary(&text, MAX_FINAL_BYTES);
        text.truncate(boundary);
        text.push_str("\n\n[truncated]");
    }
    text
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// The audit file, beside the delegation records.
pub fn default_audit_path() -> PathBuf {
    agi_platform::data_dir().join("delegations-audit.jsonl")
}

/// The default record path, beside the other per-install state.
pub fn default_path() -> PathBuf {
    agi_platform::data_dir().join("delegations.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Delegation {
        Delegation {
            id: "d1".into(),
            peer: "peer".into(),
            peer_name: "B".into(),
            session_id: Some("dsh:s".into()),
            permission: None,
            provider: None,
            model: None,
            reasoning_effort: None,
            address: None,
            state: RemoteState::Running,
            route: "wan".into(),
            cwd: None,
            prompt_preview: "do it".into(),
            final_text: None,
            error: None,
            reason: None,
            approval: None,
            started_at: 1,
            ended_at: None,
        }
    }

    #[test]
    fn a_running_record_becomes_interrupted_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delegations.json");
        std::fs::write(&path, serde_json::to_vec(&vec![sample()]).unwrap()).unwrap();
        let manager = RemoteManager::new();
        manager.load(path);
        let record = manager.get("d1").expect("the record survived");
        assert_eq!(record.state, RemoteState::Interrupted);
        assert!(record.ended_at.is_some());
    }

    #[test]
    fn the_preview_keeps_the_first_non_empty_line() {
        assert_eq!(preview("\n\n  first line\nsecond"), "  first line");
        let long = "x".repeat(300);
        assert!(preview(&long).chars().count() <= 201);
    }

    #[test]
    fn final_text_is_capped_on_a_char_boundary() {
        let text = "好".repeat(MAX_FINAL_BYTES);
        let capped = cap(text);
        assert!(capped.len() <= MAX_FINAL_BYTES + 20);
        assert!(capped.ends_with("[truncated]"));
    }

    #[test]
    fn cancel_writes_a_settled_audit_and_announces() {
        let manager = RemoteManager::new();
        manager
            .records
            .lock()
            .unwrap()
            .insert("d1".to_string(), sample());
        let (cancel, _) = watch::channel(false);
        manager
            .cancels
            .lock()
            .unwrap()
            .insert("d1".to_string(), cancel);
        let seen = Arc::new(Mutex::new(Vec::new()));
        manager.set_notice_sink({
            let seen = Arc::clone(&seen);
            Arc::new(move |delegation: &Delegation, kind: &str| {
                seen.lock()
                    .unwrap()
                    .push((delegation.id.clone(), kind.to_string()));
            })
        });
        assert!(manager.cancel("d1"));
        let record = manager.get("d1").unwrap();
        assert_eq!(record.state, RemoteState::Cancelled);
        let events = manager.audit(10);
        assert_eq!(events.last().unwrap().state, "cancelled");
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("d1".to_string(), "settled".to_string())]
        );
    }

    #[test]
    fn a_settled_delegation_reaches_the_notice_sink() {
        let manager = RemoteManager::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        manager.set_notice_sink(Arc::new(move |record, _kind| {
            sink.lock().unwrap().push(record.id.clone());
        }));
        manager.announce(
            &Delegation {
                id: "d1".to_string(),
                peer: "p".to_string(),
                peer_name: "Peer".to_string(),
                session_id: None,
                permission: None,
                provider: None,
                model: None,
                reasoning_effort: None,
                address: None,
                state: RemoteState::Done,
                route: "wan".to_string(),
                cwd: None,
                prompt_preview: "x".to_string(),
                final_text: Some("done".to_string()),
                error: None,
                reason: Some("completed".to_string()),
                approval: None,
                started_at: 1,
                ended_at: Some(2),
            },
            "settled",
        );
        assert_eq!(*seen.lock().unwrap(), vec!["d1"]);
    }

    #[test]
    fn an_approval_is_recorded_and_cleared() {
        let manager = RemoteManager::new();
        manager
            .records
            .lock()
            .unwrap()
            .insert("d1".to_string(), sample());
        manager.observe_approval(
            "d1",
            &serde_json::json!({
                "requestId": "a1", "risk": "low", "riskReason": "cargo test"
            }),
        );
        let record = manager.get("d1").unwrap();
        let approval = record.approval.expect("the approval is recorded");
        assert_eq!(approval.request_id, "a1");
        assert_eq!(approval.risk, "low");
        manager.clear_approval("d1", "allowed-once");
        assert!(manager.get("d1").unwrap().approval.is_none());
    }

    #[test]
    fn audit_lines_cover_start_approval_and_answer_without_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let manager = RemoteManager::new();
        manager.set_audit_path(path.clone());
        manager
            .records
            .lock()
            .unwrap()
            .insert("d1".to_string(), sample());
        manager.record_audit(
            "d1",
            "peer",
            "wan",
            "started",
            RemoteState::Running,
            None,
            None,
        );
        manager.observe_approval(
            "d1",
            &serde_json::json!({
                "requestId": "a1", "risk": "low", "riskReason": "cargo test"
            }),
        );
        manager.clear_approval("d1", "allowed-once");
        let events = manager.audit(10);
        assert_eq!(events.len(), 3, "{events:?}");
        assert_eq!(events[0].event, "started");
        assert_eq!(events[1].event, "approval");
        assert_eq!(events[1].risk.as_deref(), Some("low"));
        assert_eq!(events[2].event, "answered");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(
            !text.contains("do it"),
            "prompt content must not be audited"
        );
    }

    #[test]
    fn the_default_path_is_beside_the_other_state() {
        assert!(default_path().ends_with("delegations.json"));
        assert_eq!(
            default_path().parent(),
            Some(agi_platform::data_dir().as_path())
        );
    }
}
