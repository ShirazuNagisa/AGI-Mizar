//! Delegations from this desktop to same-account peers.
//!
//! The manager owns what the kernel can show and what survives a restart; the
//! wire lives in `agi-peer`, the token minting in `account`. Keeping the three
//! apart is what lets the persistence tests run without a relay and the wire
//! tests run without a kernel.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agi_peer::{Credentials, PeerClient, PeerError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

/// How many delegations may be running at once.
pub const MAX_ACTIVE: usize = 4;

/// The `peer` value a delegation to this computer's own DSH carries.
///
/// One namespace for both routes: a caller holds one delegation id and the
/// result/cancel/send commands do not care which side of the wire it ran on.
/// The value cannot collide with a device id — those are uuids or host-issued
/// ids, never the literal word.
pub const LOCAL_PEER: &str = "local";

/// The `route` a local delegation records, and what the window draws.
pub const ROUTE_LOCAL: &str = "local";

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
    /// `wan`, `lan`, or `local`; old records default to `wan`.
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
    /// Whether the settled session was archived, when auto-archive is on.
    ///
    /// A session that settled before the option existed reads `false`; the
    /// field is presentation, never a promise that the session still exists.
    #[serde(default)]
    pub archived: bool,
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

/// What one local turn produced.
#[derive(Debug, Clone)]
pub struct LocalTurn {
    /// `completed`, `error`, `aborted` — the vocabulary the record stores.
    pub reason: String,
    pub final_text: Option<String>,
}

/// This computer's own agent, cut down to what a delegation needs.
///
/// The kernel implements this over its engine registry. Keeping it a trait does
/// two things: `remote.rs` holds one copy of the record/cancel/audit logic for
/// both routes, and the local route is testable with a fake agent instead of a
/// live DSH.
#[async_trait::async_trait]
pub trait LocalAgent: Send + Sync {
    /// Whether the agent can take work right now, and why not when it cannot.
    fn status(&self) -> (bool, Option<String>);

    /// The agent's own name for a new session, in this workspace.
    async fn create_session(&self, cwd: Option<&str>) -> Result<String, String>;

    /// Choose the model (and reasoning level) for a session.
    async fn set_model(
        &self,
        session: &str,
        provider: &str,
        model: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<(), String>;

    /// Choose the permission preset for a session.
    async fn set_permission(&self, session: &str, preset: &str) -> Result<(), String>;

    /// Send one prompt and wait for its turn to settle.
    ///
    /// `cancel` is the manager's signal: the implementation stops the turn and
    /// answers `Err("cancelled")`, which is what turns the record into a
    /// cancelled delegation rather than a failed one.
    async fn run_turn(
        &self,
        session: &str,
        prompt: &str,
        timeout: Duration,
        cancel: watch::Receiver<bool>,
    ) -> Result<LocalTurn, String>;

    /// Hide the session from the agent's list.
    async fn archive(&self, session: &str) -> Result<(), String>;
}

pub struct RemoteManager {
    path: Mutex<Option<PathBuf>>,
    records: Mutex<BTreeMap<String, Delegation>>,
    cancels: Mutex<HashMap<String, watch::Sender<bool>>>,
    notices: Mutex<Option<NoticeSink>>,
    audit_path: Mutex<Option<PathBuf>>,
    audit: Mutex<std::collections::VecDeque<AuditEvent>>,
    /// This computer's own DSH, once the kernel has attached it.
    local: Mutex<Option<Arc<dyn LocalAgent>>>,
    /// Whether a settled delegation's session is archived.
    ///
    /// Read at settlement, not at start: a person who turns the switch off while
    /// a task runs does not want the session they are watching to disappear.
    auto_archive: AtomicBool,
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
            local: Mutex::new(None),
            // On unless somebody says otherwise: a build that predates the
            // switch must behave like the one that ships with it.
            auto_archive: AtomicBool::new(true),
        })
    }

    /// Attach this computer's own DSH, so `start_local` can drive it.
    pub fn set_local_agent(&self, agent: Arc<dyn LocalAgent>) {
        *self
            .local
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(agent);
    }

    /// The attached local agent, when this build has one.
    fn local_agent(&self) -> Option<Arc<dyn LocalAgent>> {
        self.local
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Whether finished sessions are archived automatically.
    pub fn auto_archive(&self) -> bool {
        self.auto_archive.load(Ordering::Acquire)
    }

    /// Turn the auto-archive switch.
    pub fn set_auto_archive(&self, enabled: bool) {
        self.auto_archive.store(enabled, Ordering::Release);
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

    /// Insert a record without a network round trip.
    ///
    /// Tests need "a delegation that is still running"; [`load`] deliberately
    /// turns a persisted `running` into `interrupted`, because a new process
    /// has no task for it.
    #[cfg(test)]
    pub(crate) fn seed(&self, record: Delegation) {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(record.id.clone(), record);
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
        self.admit(&peer.device_id)?;
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
            archived: false,
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
        self.admit(&peer.device_id)?;
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
            archived: false,
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

    /// Start one delegation on this computer's own DSH.
    ///
    /// The local twin of [`Self::start_with_options`]: same records, same
    /// cancel and audit trail, one process instead of two. `session` continues
    /// an earlier subagent session, which is what `local_send` uses.
    pub fn start_local(
        self: &Arc<Self>,
        launch: RemoteLaunch,
        prompt: String,
        session: Option<String>,
    ) -> Result<Delegation, String> {
        let Some(agent) = self.local_agent() else {
            return Err("this build has no local DSH attached".to_string());
        };
        let (ready, why) = agent.status();
        if !ready {
            return Err(why.unwrap_or_else(|| "the local DSH is not connected".to_string()));
        }
        self.admit(LOCAL_PEER)?;
        if prompt.trim().is_empty() {
            return Err("the subagent task is empty".to_string());
        }

        let id = uuid::Uuid::new_v4().to_string();
        let record = Delegation {
            id: id.clone(),
            peer: LOCAL_PEER.to_string(),
            peer_name: "本机 DSH".to_string(),
            session_id: session.clone(),
            permission: launch.permission.clone(),
            provider: launch.provider.clone(),
            model: launch.model.clone(),
            reasoning_effort: launch.reasoning_effort.clone(),
            address: None,
            state: RemoteState::Running,
            route: ROUTE_LOCAL.to_string(),
            cwd: launch.cwd.clone(),
            prompt_preview: preview(&prompt),
            final_text: None,
            error: None,
            reason: None,
            archived: false,
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
        tokio::spawn(async move {
            let outcome =
                run_local_one(&manager, &task_id, agent, launch, &prompt, session, cancel_rx).await;
            manager.finish(&task_id, outcome);
        });
        Ok(record)
    }

    /// How many delegations may run at once, for one target.
    ///
    /// Shared by all three routes so the local one cannot quietly become the
    /// only way to start five tasks on one machine.
    fn admit(&self, peer: &str) -> Result<(), String> {
        let records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let running = records.values().filter(|record| record.state.is_active()).count();
        if running >= MAX_ACTIVE {
            return Err(format!(
                "too many remote delegations are already running ({MAX_ACTIVE})"
            ));
        }
        let per_peer = records
            .values()
            .filter(|record| record.state.is_active() && record.peer == peer)
            .count();
        if per_peer >= 2 {
            return Err("that computer already has two running delegations".to_string());
        }
        Ok(())
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
                record.archived = turn.archived;
                record.ended_at = Some(now_ms());
            }
            Err(failure) => {
                record.state = failure.state;
                record.error = Some(failure.message);
                record.reason = failure.reason;
                record.archived = failure.archived;
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
    /// Whether the session was archived after it settled.
    archived: bool,
}

struct RunFailure {
    state: RemoteState,
    message: String,
    reason: Option<String>,
    archived: bool,
}

impl RunFailure {
    /// A failure that happened before a session existed, so nothing to archive.
    fn early(state: RemoteState, message: impl Into<String>, reason: &str) -> Self {
        Self {
            state,
            message: message.into(),
            reason: Some(reason.to_string()),
            archived: false,
        }
    }
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
        .map_err(|error| RunFailure::early(RemoteState::Failed, error.to_string(), "connect"))?;
    let session = match existing_session {
        Some(session) => session,
        None => client
            .create_session(launch.cwd.as_deref())
            .await
            .map_err(|error| {
                RunFailure::early(RemoteState::Failed, error.to_string(), "session.create")
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
            .map_err(|error| {
                RunFailure::early(RemoteState::Failed, error.to_string(), "session.model.set")
            })?;
    }
    if let Some(preset) = launch.permission.as_deref() {
        client
            .set_permission(&session, preset)
            .await
            .map_err(|error| {
                RunFailure::early(
                    RemoteState::Failed,
                    error.to_string(),
                    "session.permission.set",
                )
            })?;
    }
    let turn = client
        .run_turn_with(&session, prompt, TURN_TIMEOUT, cancel, |approval| {
            manager.observe_approval(id, approval);
        })
        .await;
    // A cancelled session is the operator's to look at, not ours to hide; a
    // finished one (whether it succeeded or failed) is what the tidy-up is for.
    let tidy = !matches!(&turn, Err(PeerError::Other(message)) if message == "cancelled");
    let archived = tidy && manager.auto_archive() && client.archive(&session).await.is_ok();
    match turn {
        Ok(turn) => Ok(TurnOutcome {
            reason: turn.reason,
            final_text: turn.final_text.map(cap),
            archived,
        }),
        Err(PeerError::Other(message)) if message == "cancelled" => Err(RunFailure {
            state: RemoteState::Cancelled,
            message: "the delegation was cancelled".to_string(),
            reason: Some("cancelled".to_string()),
            archived,
        }),
        Err(error) => Err(RunFailure {
            state: RemoteState::Failed,
            message: error.to_string(),
            reason: Some("turn".to_string()),
            archived,
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
        .map_err(|error| RunFailure::early(RemoteState::Failed, error.to_string(), "lan.connect"))?;
    let session = match existing_session {
        Some(session) => session,
        None => client
            .create_session(launch.cwd.as_deref())
            .await
            .map_err(|error| {
                RunFailure::early(RemoteState::Failed, error.to_string(), "lan.session.create")
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
            .map_err(|error| {
                RunFailure::early(
                    RemoteState::Failed,
                    error.to_string(),
                    "lan.session.model.set",
                )
            })?;
    }
    if let Some(preset) = launch.permission.as_deref() {
        client
            .set_permission(&session, preset)
            .await
            .map_err(|error| {
                RunFailure::early(
                    RemoteState::Failed,
                    error.to_string(),
                    "session.permission.set",
                )
            })?;
    }
    let turn = client
        .run_turn(&session, prompt, TURN_TIMEOUT, cancel, |approval| {
            manager.observe_approval(id, approval);
        })
        .await;
    let tidy = !matches!(&turn, Err(agi_lan::LanError::Remote(message)) if message == "cancelled");
    let archived = tidy && manager.auto_archive() && client.archive(&session).await.is_ok();
    match turn {
        Ok(turn) => Ok(TurnOutcome {
            reason: turn.reason,
            final_text: turn.final_text.map(cap),
            archived,
        }),
        Err(agi_lan::LanError::Remote(message)) if message == "cancelled" => Err(RunFailure {
            state: RemoteState::Cancelled,
            message: "the delegation was cancelled".to_string(),
            reason: Some("cancelled".to_string()),
            archived,
        }),
        Err(error) => Err(RunFailure {
            state: RemoteState::Failed,
            message: error.to_string(),
            reason: Some("lan.turn".to_string()),
            archived,
        }),
    }
}

/// One local turn, start to settlement, on the agent attached at boot.
#[allow(clippy::too_many_arguments)]
async fn run_local_one(
    manager: &RemoteManager,
    id: &str,
    agent: Arc<dyn LocalAgent>,
    launch: RemoteLaunch,
    prompt: &str,
    existing_session: Option<String>,
    cancel: watch::Receiver<bool>,
) -> Result<TurnOutcome, RunFailure> {
    let session = match existing_session {
        Some(session) => session,
        None => agent
            .create_session(launch.cwd.as_deref())
            .await
            .map_err(|message| {
                RunFailure::early(RemoteState::Failed, message, "local.session.create")
            })?,
    };
    manager.set_session(id, session.clone());
    if let (Some(provider), Some(model)) = (launch.provider.as_deref(), launch.model.as_deref()) {
        agent
            .set_model(&session, provider, model, launch.reasoning_effort.as_deref())
            .await
            .map_err(|message| {
                RunFailure::early(RemoteState::Failed, message, "local.session.model.set")
            })?;
    }
    if let Some(preset) = launch.permission.as_deref() {
        agent
            .set_permission(&session, preset)
            .await
            .map_err(|message| {
                RunFailure::early(
                    RemoteState::Failed,
                    message,
                    "local.session.permission.set",
                )
            })?;
    }
    let turn = agent.run_turn(&session, prompt, TURN_TIMEOUT, cancel).await;
    let tidy = !matches!(&turn, Err(message) if message == "cancelled");
    let archived = tidy && manager.auto_archive() && agent.archive(&session).await.is_ok();
    match turn {
        Ok(turn) => Ok(TurnOutcome {
            reason: turn.reason,
            final_text: turn.final_text.map(cap),
            archived,
        }),
        Err(message) if message == "cancelled" => Err(RunFailure {
            state: RemoteState::Cancelled,
            message: "the delegation was cancelled".to_string(),
            reason: Some("cancelled".to_string()),
            archived,
        }),
        Err(message) => Err(RunFailure {
            state: RemoteState::Failed,
            message,
            reason: Some("local.turn".to_string()),
            archived,
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
            archived: false,
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
                archived: false,
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

    /// A local agent that records what it was asked and answers at once.
    struct FakeLocal {
        session: String,
        /// What `archive` was called with, in order.
        archived: Mutex<Vec<String>>,
        /// The cwd `create_session` was handed; `None` when it was never called.
        created: Mutex<Option<Option<String>>>,
    }

    impl FakeLocal {
        fn new(session: &str) -> Arc<Self> {
            Arc::new(Self {
                session: session.to_string(),
                archived: Mutex::new(Vec::new()),
                created: Mutex::new(None),
            })
        }
    }

    #[async_trait::async_trait]
    impl LocalAgent for FakeLocal {
        fn status(&self) -> (bool, Option<String>) {
            (true, None)
        }

        async fn create_session(&self, cwd: Option<&str>) -> Result<String, String> {
            *self.created.lock().unwrap() = Some(cwd.map(str::to_string));
            Ok(self.session.clone())
        }

        async fn set_model(
            &self,
            _session: &str,
            _provider: &str,
            _model: &str,
            _effort: Option<&str>,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn set_permission(&self, _session: &str, _preset: &str) -> Result<(), String> {
            Ok(())
        }

        async fn run_turn(
            &self,
            _session: &str,
            _prompt: &str,
            _timeout: Duration,
            _cancel: watch::Receiver<bool>,
        ) -> Result<LocalTurn, String> {
            Ok(LocalTurn {
                reason: "completed".to_string(),
                final_text: Some("local answer".to_string()),
            })
        }

        async fn archive(&self, session: &str) -> Result<(), String> {
            self.archived.lock().unwrap().push(session.to_string());
            Ok(())
        }
    }

    /// The whole local route: a session is created, the turn is recorded as
    /// done, and the session is archived once because the switch is on.
    #[tokio::test]
    async fn a_local_delegation_runs_and_archives_its_session() {
        let agent = FakeLocal::new("dsh:local-1");
        let manager = RemoteManager::new();
        manager.set_local_agent(agent.clone());
        let record = manager
            .start_local(
                RemoteLaunch {
                    cwd: Some("C:\\work".to_string()),
                    ..RemoteLaunch::default()
                },
                "summarise the repo".to_string(),
                None,
            )
            .expect("the local delegation starts");
        assert_eq!(record.route, ROUTE_LOCAL);
        assert_eq!(record.peer, LOCAL_PEER);

        for _ in 0..50 {
            if !manager.get(&record.id).unwrap().state.is_active() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let settled = manager.get(&record.id).expect("the record survives");
        assert_eq!(settled.state, RemoteState::Done, "{settled:?}");
        assert_eq!(settled.final_text.as_deref(), Some("local answer"));
        assert_eq!(settled.session_id.as_deref(), Some("dsh:local-1"));
        assert!(settled.archived, "auto-archive is on by default");
        assert_eq!(
            *agent.archived.lock().unwrap(),
            vec!["dsh:local-1".to_string()]
        );
        assert_eq!(
            *agent.created.lock().unwrap(),
            Some(Some("C:\\work".to_string()))
        );
    }

    /// Turning the switch off leaves the session where the operator can read it.
    #[tokio::test]
    async fn auto_archive_off_keeps_the_session() {
        let agent = FakeLocal::new("dsh:local-2");
        let manager = RemoteManager::new();
        manager.set_local_agent(agent.clone());
        manager.set_auto_archive(false);
        let record = manager
            .start_local(RemoteLaunch::default(), "do it".to_string(), None)
            .expect("the local delegation starts");
        for _ in 0..50 {
            if !manager.get(&record.id).unwrap().state.is_active() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let settled = manager.get(&record.id).expect("the record survives");
        assert_eq!(settled.state, RemoteState::Done);
        assert!(!settled.archived);
        assert!(agent.archived.lock().unwrap().is_empty());
    }

    /// A second task on an existing session continues it and does not create
    /// another one.
    #[tokio::test]
    async fn a_local_follow_up_reuses_the_session() {
        let agent = FakeLocal::new("dsh:local-3");
        let manager = RemoteManager::new();
        manager.set_local_agent(agent.clone());
        manager.set_auto_archive(false);
        let record = manager
            .start_local(
                RemoteLaunch::default(),
                "step one".to_string(),
                Some("dsh:local-3".to_string()),
            )
            .expect("the follow-up starts");
        for _ in 0..50 {
            if !manager.get(&record.id).unwrap().state.is_active() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(manager.get(&record.id).unwrap().state, RemoteState::Done);
        assert_eq!(*agent.created.lock().unwrap(), None, "no new session");
    }

    /// Without an attached local agent the command answers instead of pretending
    /// a task started.
    #[test]
    fn a_local_delegation_needs_an_attached_agent() {
        let manager = RemoteManager::new();
        let error = manager
            .start_local(RemoteLaunch::default(), "do it".to_string(), None)
            .expect_err("nothing is attached");
        assert!(error.contains("no local DSH"), "{error}");
    }
}
