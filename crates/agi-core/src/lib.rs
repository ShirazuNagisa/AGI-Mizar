//! The kernel.
//!
//! Holds one of each engine, keeps the state every client reads, and answers the
//! local API. It owns no transport and no window: the HTTP server lives in
//! [`server`], the DSH link socket is handed to the adapter, and a Tauri shell
//! would embed this same [`Core`] behind a window.
//!
//! ### Why state is aggregated here
//!
//! Three engines update at three different rates through three mechanisms. A
//! client that had to ask each one separately would need three code paths, three
//! failure modes and three notions of "latest". The kernel subscribes to all of
//! them once, keeps one ordered view, and pushes one event stream — so the UI has
//! exactly one source of truth and one place to be wrong.
//!
//! ### What is deliberately not here
//!
//! No session persistence. The kernel is a live view of what the engines report;
//! the session logs are the engines' own durable record, and a second copy would
//! be a second truth. The one exception is the window's own preferences — a
//! theme name — which belongs to this app rather than to any engine.

// Tests sit next to the code they exercise, which occasionally puts a helper
// below a `mod tests`; clippy's default order is not this file's order.
#![allow(clippy::items_after_test_module)]

pub mod account;
pub mod dataset;
pub mod dependencies;
pub mod endpoint;
pub mod models;
pub mod plugin;
pub mod remote;
pub mod remote_health;
pub mod server;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use agi_engine::{Engine, EngineError, EngineEvent};
use agi_layout::model::{Backend, LabelMap, ModelManifest};
use agi_layout::{facts_from_turn, Item, JournalEntry, Layout, LayoutService, Live, StateMachine};
use agi_protocol::api::{
    ApiEvent, Command, CommandResponse, DshPluginView, EngineStatus, OnboardingView, StateSnapshot,
    UsageOverview, DSH_PLUGIN_MIN_VERSION,
};
use agi_protocol::domain::{Interaction, Record, RecordKind, Session, SessionKey};
use agi_protocol::engine::EngineId;
use agi_protocol::state_frame::HEARTBEAT_MS;
use agi_protocol::{SessionState, StateFrame};
use models::{ModelEvent, ModelService};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;

/// How many events a slow subscriber may fall behind before it is told it missed
/// some. A window that lags is better than a window that blocks the kernel.
const EVENT_BUFFER: usize = 1_024;

/// Records kept per session in the aggregated view.
const MAX_RECORDS: usize = 2_000;

/// How often the background task asks npm for a new `dsh-mizar`.
///
/// Six hours: a plugin release is not a security patch, and every launch already
/// checks once. The number is small enough that a laptop left open for a day
/// notices, large enough that a window polled every five seconds never does.
const DSH_PLUGIN_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Epoch milliseconds, for the plugin view's "last checked".
fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// The state every client reads.
#[derive(Default)]
struct Store {
    sessions: BTreeMap<SessionKey, Session>,
    records: HashMap<SessionKey, VecDeque<Record>>,
    interactions: BTreeMap<String, Interaction>,
    /// Which engine asked for each interaction, so an answer goes to the right
    /// adapter. An interaction carries its session only when the engine had one.
    interaction_engine: HashMap<String, EngineId>,
}

/// The kernel.
/// What the window remembers between launches.
///
/// The page's origin changes with the kernel's port, so the browser's own
/// storage cannot hold this; it lives beside the device roster instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preferences {
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Whether the first-run guide has been completed.
    #[serde(default)]
    pub onboarding: Onboarding,
    /// Which agents the operator said yes to.
    #[serde(default)]
    pub engines: EnginePrefs,
    /// Whether this install shares its turns to improve Mizarllm.
    #[serde(default)]
    pub dataset: DatasetPrefs,
    /// Whether this computer accepts remote peers, and where they may work.
    #[serde(default)]
    pub remote: RemotePrefs,
}

/// One LAN peer seen in the last few seconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanPeer {
    pub device_id: String,
    pub name: String,
    pub address: String,
    pub port: u16,
    pub certificate_fingerprint: String,
    pub last_seen_ms: i64,
}

/// How long a beacon keeps a LAN peer "online".
const LAN_PEER_TTL_MS: i64 = 20_000;

/// What this computer allows a same-account peer to do.
///
/// Off by default for every install that never opted in: a machine that
/// upgrades into this build must not start accepting remote work because a
/// field defaulted the wrong way.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemotePrefs {
    /// The master switch for accepting peer work.
    #[serde(default)]
    pub enabled: bool,
    /// Whether a peer on the same LAN may connect (free).
    #[serde(default)]
    pub allow_lan: bool,
    /// Whether a peer through the relay may connect (paid).
    #[serde(default)]
    pub allow_wan: bool,
    /// Whether every DSH workspace is open to a peer, ignoring `workspaces`.
    #[serde(default)]
    pub allow_all_workspaces: bool,
    /// Workspace paths a peer may start a session in.
    #[serde(default)]
    pub workspaces: Vec<String>,
    /// Devices explicitly refused, even when their token is valid.
    #[serde(default)]
    pub blocked: Vec<String>,
}

/// What the operator chose about anonymous session sharing.
///
/// Off by default for every install that never saw the guide step: an existing
/// machine upgrading into this build must not start uploading because a field
/// defaulted the wrong way.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatasetPrefs {
    /// The operator's answer. The guide writes `true` when the (pre-ticked)
    /// switch is left on, and the settings page can turn it off later.
    #[serde(default)]
    pub enabled: bool,
    /// Which version of the consent copy they saw; `0` means never asked.
    #[serde(default)]
    pub policy_version: u32,
    /// When the answer last changed, epoch milliseconds.
    #[serde(default)]
    pub since: Option<i64>,
    /// Random per-install id, never derived from the account. It keys the
    /// pseudonym of every session and is the only way two uploaded turns can be
    /// known to belong to the same session.
    #[serde(default)]
    pub install_id: String,
    /// Project names (the last path component) never shared.
    #[serde(default)]
    pub excluded_projects: Vec<String>,
    /// Engine-scoped session ids whose history has already been walked.
    #[serde(default)]
    pub backfilled: Vec<String>,
}

/// The guide's own state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Onboarding {
    #[serde(default)]
    pub done: bool,
}

/// Which agents were configured, by the guide or from an agent page.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnginePrefs {
    #[serde(default)]
    pub dsh: bool,
    #[serde(default)]
    pub claude: bool,
    #[serde(default)]
    pub codex: bool,
}

impl EnginePrefs {
    fn get(&self, engine: EngineId) -> bool {
        match engine {
            EngineId::Dsh => self.dsh,
            EngineId::Claude => self.claude,
            EngineId::Codex => self.codex,
        }
    }

    fn set(&mut self, engine: EngineId, enabled: bool) {
        match engine {
            EngineId::Dsh => self.dsh = enabled,
            EngineId::Claude => self.claude = enabled,
            EngineId::Codex => self.codex = enabled,
        }
    }
}

fn default_theme() -> String {
    "system".to_string()
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            onboarding: Onboarding::default(),
            engines: EnginePrefs::default(),
            dataset: DatasetPrefs::default(),
            remote: RemotePrefs::default(),
        }
    }
}

/// Normalise a workspace path for the remote-policy comparison.
///
/// A delegating peer spells the same directory the way its own platform does:
/// Windows accepts both separators and is case-insensitive, macOS may hand back
/// a path with or without the trailing slash. Comparing the raw strings means a
/// legitimate delegation is refused as "not in the allowlist", so separators
/// are folded, trailing separators dropped, and (on Windows) the case folded.
fn normalize_workspace(path: &str) -> String {
    let mut normalised = path.replace('\\', "/");
    while normalised.len() > 1 && normalised.ends_with('/') {
        normalised.pop();
    }
    if cfg!(windows) {
        normalised = normalised.to_lowercase();
    }
    normalised
}

/// Run the `agi-remote` installer next to this executable, without a shell.
pub(crate) fn run_remote_installer(
    uninstall: bool,
    profile: Option<&str>,
    dry_run: bool,
) -> CommandResponse {
    let path = std::env::var("AGI_REMOTE_BIN")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| find_remote_installer(&exe))
        })
        .filter(|path| path.is_file());
    let Some(path) = path else {
        return CommandResponse::error(
            "REMOTE_INSTALLER_MISSING",
            "agi-remote is not beside this app; install it first or set AGI_REMOTE_BIN",
        );
    };
    let mut command = agi_platform::std_command(path);
    // Pin the DSH home the kernel reads onto the installer. A GUI launch on
    // Windows has no HOME, so without this the two could resolve different
    // profiles; `agi_platform::dsh_home()` is the same resolution the kernel's
    // plugin reader uses.
    command.env("DSH_HOME", agi_platform::dsh_home());
    command.arg(if uninstall { "uninstall" } else { "install" });
    if let Some(profile) = profile {
        command.arg("--profile").arg(profile);
    }
    if dry_run {
        command.arg("--dry-run");
    }
    match command.output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            if output.status.success() {
                CommandResponse::ok(json!({ "stdout": stdout, "stderr": stderr }))
            } else {
                CommandResponse::error(
                    "REMOTE_INSTALLER_FAILED",
                    format!("{}{}", stdout, stderr).trim().to_string(),
                )
            }
        }
        Err(error) => CommandResponse::error("REMOTE_INSTALLER_FAILED", error.to_string()),
    }
}

/// Find the `agi-remote` sidecar beside a packaged app.
///
/// The sidecar is staged in three layouts, all of which are checked:
/// - next to the executable (a dev `cargo build` puts it beside `agi-mizar.exe`);
/// - under `bin/` (the Windows bundler maps `resources/bin` there);
/// - under `../Resources/bin` (the macOS `.app` bundle layout).
pub(crate) fn find_remote_installer(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    let parent = exe.parent()?;
    [
        parent.join("agi-remote"),
        parent.join("agi-remote.exe"),
        parent.join("bin/agi-remote"),
        parent.join("bin/agi-remote.exe"),
        parent.join("../Resources/bin/agi-remote"),
        parent.join("../Resources/bin/agi-remote.exe"),
        parent.join("../Resources/agi-remote"),
        parent.join("../Resources/agi-remote.exe"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    // `agi_platform::canonicalize` is `dunce` on Windows: it resolves the same
    // way but leaves out the `\\?\` verbatim prefix, which would otherwise be
    // shown to the operator in the dependency report and written into the
    // `cordis.patch.yml` that the same value is compared against.
    .and_then(|path| agi_platform::canonicalize(&path).ok().or(Some(path)))
}

/// Whether a theme name is one this window can paint.
pub fn valid_theme(theme: &str) -> bool {
    // `oled` is a window theme, not a system one: the kernel only stores it so
    // the next launch paints the same window.
    matches!(theme, "system" | "light" | "dark" | "oled")
}

/// Installs the Claude Code hooks, from whatever knows where the shim is.
type HookInstaller = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Looks for agents again, given the ones already hosted.
type RescanFn =
    Arc<dyn Fn(&[EngineId]) -> (Vec<Arc<dyn Engine>>, HashMap<EngineId, String>) + Send + Sync>;

/// One turn's streamed intervals, as a phone that missed them can read them back.
///
/// The live state channel is "what now"; this is "what happened", so a phone that
/// was disconnected while the turn ran can rebuild the turn instead of finding an
/// empty interval and a hole in the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamReplay {
    /// The turn these intervals belong to, if one has started.
    pub turn: Option<u32>,
    /// The state on the wire now.
    pub state: SessionState,
    /// The interval on the wire now.
    pub interval: u32,
    /// Bytes the current interval has emitted.
    pub text_seq: u32,
    /// Every interval of the turn, oldest first.
    pub entries: Vec<JournalEntry>,
    /// Whether the oldest intervals were dropped to stay within budget.
    pub truncated: bool,
}

pub struct Core {
    engines: Vec<Arc<dyn Engine>>,
    store: RwLock<Store>,
    events: broadcast::Sender<ApiEvent>,
    version: String,
    /// The phone server, when this build has one. Attached after construction
    /// because it needs the kernel it serves.
    pairing: RwLock<Option<Arc<dyn PairingSource>>>,
    /// The phone roster, attached the same way and for the same reason.
    devices: RwLock<Option<Arc<dyn DeviceStatusSource>>>,
    preferences: RwLock<Preferences>,
    /// Where preferences are written, once a launch tells us.
    preferences_path: RwLock<Option<PathBuf>>,
    /// The relay account this window is signed in to.
    account: account::AccountService,
    /// Delegations to same-account peers.
    remote: Arc<remote::RemoteManager>,
    /// This machine's LAN keypair, created lazily and kept for the process.
    lan_identity: RwLock<Option<Arc<agi_lan::DeviceIdentity>>>,
    /// The account CA certificate for this LAN identity, after a fetch.
    lan_certificate: RwLock<Option<account::LanCertificateAnswer>>,
    /// Fires when the remote-peer policy changes, so live peer sockets can
    /// notice a block without waiting for their next frame.
    remote_prefs_changed: broadcast::Sender<()>,
    /// LAN peers seen by the beacon listener, by device id.
    lan_peers: RwLock<HashMap<String, LanPeer>>,
    /// Where the LAN certificate is cached, once boot points us at it.
    lan_certificate_path: RwLock<Option<std::path::PathBuf>>,
    /// The tunnel's own state, when this build dials a relay.
    relay_status: RwLock<Option<Arc<dyn RelayStatusSource>>>,
    /// Turn-completion judge, when this build has a model sidecar.
    ///
    /// Shadow mode by default: results are recorded and logged but nothing is
    /// published to clients until the UI asks for them.
    layout: RwLock<Option<Arc<LayoutService>>>,
    /// Judgements by `(session, turn)`, so a late result can be read back.
    layouts: Arc<Mutex<HashMap<(SessionKey, u32), Layout>>>,
    /// The model directory, the downloader and the sidecar, as one thing.
    model_service: Arc<ModelService>,
    /// The active model, when one is.
    manifest: RwLock<Option<ModelManifest>>,
    /// What each session is doing, and the interval it is on.
    states: Arc<Mutex<HashMap<SessionKey, StateMachine>>>,
    /// The state channel: encoded frames, one per state change or heartbeat.
    frames: broadcast::Sender<(SessionKey, Vec<u8>)>,
    /// Engines this installation knows about but could not host, and why.
    ///
    /// An agent that was never installed and one that is installed but silent
    /// are different problems; only the second is worth a reconnect.
    unavailable: RwLock<HashMap<EngineId, String>>,
    /// Engines discovered after launch, because the operator installed one
    /// while the window was open.
    late: RwLock<HashMap<EngineId, Arc<dyn Engine>>>,
    /// Installs the Claude Code hooks. The boot sequence owns this because the
    /// shim's path is a packaging fact, not a kernel one.
    claude_hooks: RwLock<Option<HookInstaller>>,
    /// Re-runs discovery. Installed by the boot sequence, which is the only
    /// thing that knows where an agent's binary might be.
    rescan: RwLock<Option<RescanFn>>,
    /// Whether the heartbeat task has been started.
    ///
    /// Starting it inside `new()` is not reliable: the kernel is constructed on
    /// the main thread, before any runtime context exists, so the ticker silently
    /// never spawns. The first emitted frame comes from the engine's async loop,
    /// which is a runtime, and that is where it starts.
    heartbeat_started: Arc<std::sync::atomic::AtomicBool>,
    /// How many phone sockets are open right now.
    ///
    /// Not "how many phones are paired": a phone in a pocket has a token and no
    /// socket, and the judge is bought with sockets. The count is what the lease
    /// in [`Core::phone_attached`] is made of.
    phone_sockets: Arc<std::sync::atomic::AtomicUsize>,
    /// When the phone count last moved.
    ///
    /// The clock the idle release measures from. Read only while the count is
    /// zero, so a phone that never arrives costs one comparison.
    phone_last_seen: Mutex<Instant>,
    /// Whether the task that watches for an idle desktop is already running.
    lease_guard: std::sync::atomic::AtomicBool,
    /// Anonymous dataset sharing: the spool, the ledger, and the wake-up signal.
    dataset: dataset::Dataset,
    /// The DSH helper plugin's last known version, and npm's answer.
    ///
    /// Kept in the kernel rather than read on every snapshot: a window polls the
    /// snapshot every five seconds, and a registry request on that cadence would
    /// be both slow and rude. The watcher and the explicit check write it.
    dsh_plugin: RwLock<DshPluginView>,
    /// One npm check or install at a time.
    ///
    /// A background check landing during a one-click update would otherwise
    /// overwrite the view with the version that is half installed.
    plugin_lock: tokio::sync::Mutex<()>,
}

/// How long a desktop may sit with no phone attached before the judge is let go.
///
/// Five minutes: long enough that a phone switching networks or a laptop lid
/// closing and opening does not throw the model away and reload several hundred
/// megabytes, short enough that a machine somebody walked away from is not
/// holding a sidecar all night.
pub const PHONE_IDLE_RELEASE: Duration = Duration::from_secs(300);

impl Core {
    /// Build a kernel over the given engines.
    pub fn new(engines: Vec<Arc<dyn Engine>>) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let (frame_tx, _) = broadcast::channel(EVENT_BUFFER);
        let (remote_prefs_changed, _) = broadcast::channel(8);
        let model_service = ModelService::open(models_dir());
        // The judge is **not** built here. A launch with nobody listening has no
        // reason to hold a model, and the sidecar it starts is the largest thing
        // this process owns. `phone_attached` installs it, `release_judge_if_idle`
        // lets it go.
        let manifest = layout_from(&model_service);
        Arc::new(Self {
            engines,
            store: RwLock::new(Store::default()),
            events,
            version: env!("CARGO_PKG_VERSION").to_string(),
            pairing: RwLock::new(None),
            devices: RwLock::new(None),
            preferences: RwLock::new(Preferences::default()),
            preferences_path: RwLock::new(None),
            account: account::AccountService::new(account_base()),
            remote: remote::RemoteManager::new(),
            lan_identity: RwLock::new(None),
            lan_certificate: RwLock::new(None),
            remote_prefs_changed: remote_prefs_changed.clone(),
            lan_peers: RwLock::new(HashMap::new()),
            lan_certificate_path: RwLock::new(None),
            relay_status: RwLock::new(None),
            layout: RwLock::new(None),
            layouts: Arc::new(Mutex::new(HashMap::new())),
            model_service: Arc::clone(&model_service),
            manifest: RwLock::new(manifest),
            states: Arc::new(Mutex::new(HashMap::new())),
            frames: frame_tx.clone(),
            unavailable: RwLock::new(HashMap::new()),
            late: RwLock::new(HashMap::new()),
            claude_hooks: RwLock::new(None),
            rescan: RwLock::new(None),
            heartbeat_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            phone_sockets: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            phone_last_seen: Mutex::new(Instant::now()),
            lease_guard: std::sync::atomic::AtomicBool::new(false),
            dataset: dataset::Dataset::default(),
            dsh_plugin: RwLock::new(plugin::initial_view()),
            plugin_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// A phone opened a socket: install the judge and hold it.
    ///
    /// The only moment the judge is built. Called from the phone server's socket
    /// lifecycle, which is the one place that knows a device is actually
    /// listening rather than merely paired.
    pub fn phone_attached(self: &Arc<Self>) {
        self.phone_sockets
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        *self
            .phone_last_seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
        self.install_judge();
        self.start_lease_guard();
    }

    /// A phone's socket closed. The judge stays until the lease runs out.
    pub fn phone_detached(&self) {
        let mut count = self
            .phone_sockets
            .load(std::sync::atomic::Ordering::Acquire);
        while count > 0 {
            match self.phone_sockets.compare_exchange(
                count,
                count - 1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => count = actual,
            }
        }
        *self
            .phone_last_seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
    }

    /// How many phone sockets are open, for diagnostics and tests.
    pub fn phones_attached(&self) -> usize {
        self.phone_sockets
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Whether any session is running right now.
    ///
    /// Read from the store rather than from the state machines: `running` is the
    /// engine's own summary and it is what the activity rows and the phone's
    /// session list are both drawn from, so the two cannot disagree.
    pub fn any_running(&self) -> bool {
        self.store_read()
            .sessions
            .values()
            .any(|session| session.running)
    }

    /// Whether a judge is installed right now.
    pub fn has_judge(&self) -> bool {
        self.layout
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    /// Install the judge if a model is installed and none is up.
    ///
    /// Idempotent, and deliberately does not start the sidecar: that happens on
    /// the first verdict, so a phone that connects and says nothing costs an
    /// `Arc` rather than a process.
    fn install_judge(&self) -> bool {
        if self.has_judge() {
            return false;
        }
        // An explicit URL still wins over an installed model, exactly as it did
        // when this was built at launch.
        if let Some(manifest) = env_manifest() {
            let judge = manifest.judge();
            *self
                .layout
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(Arc::new(LayoutService::new(judge)));
            tracing::info!(model = %manifest.id, "layout: started from MIZAR_LAYOUT_URL");
            return true;
        }
        let Some(manifest) = self.model_service.manifest() else {
            return false;
        };
        let judge = Arc::new(models::SidecarJudge::new(Arc::clone(&self.model_service)));
        *self
            .layout
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Arc::new(LayoutService::new(judge)));
        tracing::info!(model = %manifest.id, "layout: started, a phone is attached");
        true
    }

    /// Let the judge go when no phone has been attached for `idle_for`.
    ///
    /// Returns whether this call released anything, so a caller (or a test) can
    /// tell "nothing to do" from "the model was dropped". Stopping the sidecar
    /// only when something was actually released is what keeps a race between
    /// two callers from killing a sidecar the other one just started.
    pub async fn release_judge_if_idle(&self, idle_for: Duration) -> bool {
        if self.phones_attached() != 0 {
            return false;
        }
        // A turn that is still running is a reason to keep the judge even with
        // nobody on a socket: the verdict is what ends the row on a phone's Live
        // Activity, and a phone in a pocket is exactly the case the island exists
        // for. The lease is about "nobody is listening *and* nothing is
        // happening", not about sockets alone.
        if self.any_running() {
            return false;
        }
        let idle = self
            .phone_last_seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .elapsed();
        if idle < idle_for {
            return false;
        }
        let released = self
            .layout
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .is_some();
        if released {
            self.model_service.stop_sidecar().await;
            tracing::info!(
                idle_seconds = idle.as_secs(),
                "layout: released, no phone has been attached"
            );
        }
        released
    }

    /// Watch for an idle desktop, once per process.
    ///
    /// Skipped when there is no runtime to spawn on, and the "already started"
    /// flag is set only after a task exists: a kernel constructed on the Tauri
    /// main thread (or in a unit test) has no reactor, and a flag set there
    /// would stop the guard from ever starting on the async side.
    fn start_lease_guard(self: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self
            .lease_guard
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let core = Arc::clone(self);
        handle.spawn(async move {
            loop {
                // A quarter of the leash, so the release lands between five and
                // five and a quarter minutes rather than up to ten.
                tokio::time::sleep(PHONE_IDLE_RELEASE / 4).await;
                core.release_judge_if_idle(PHONE_IDLE_RELEASE).await;
            }
        });
    }

    /// Install a judge that needs no model, for the lease tests.
    #[cfg(test)]
    fn install_test_judge(&self) {
        let judge = Arc::new(agi_layout::RuleJudge);
        *self
            .layout
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Arc::new(LayoutService::new(judge)));
    }

    /// Attach the tunnel's state, so the window can show whether it is up.
    pub fn attach_relay_status(&self, source: Arc<dyn RelayStatusSource>) {
        *self
            .relay_status
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(source);
    }

    /// Attach the phone server, so the window can show a pairing code.
    pub fn attach_devices(&self, source: Arc<dyn DeviceStatusSource>) {
        *self
            .devices
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(source);
    }

    /// The phones, or none when nothing composed them (tests, the daemon).
    pub fn device_views(&self) -> Vec<agi_protocol::api::DeviceView> {
        self.devices
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|source| source.device_views())
            .unwrap_or_default()
    }

    pub fn attach_pairing(&self, source: Arc<dyn PairingSource>) {
        *self
            .pairing
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(source);
    }

    /// Read the stored preferences, then remember where to write them.
    pub fn load_preferences(&self, path: PathBuf) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(stored) = serde_json::from_str::<Preferences>(&text) {
                if valid_theme(&stored.theme) {
                    *self
                        .preferences
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = stored;
                }
            }
        }
        *self
            .preferences_path
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
    }

    /// Read the account file and point the service at it.
    pub fn load_account(&self, path: PathBuf) {
        self.account.load(path);
    }

    /// Read the delegation records from disk.
    pub fn load_remote(&self, path: PathBuf) {
        self.remote.load(path);
    }

    /// Point the delegation audit at its JSONL file.
    pub fn set_remote_audit_path(&self, path: PathBuf) {
        self.remote.set_audit_path(path);
    }

    /// Install the sink that wakes DSH when a remote delegation settles.
    pub fn set_remote_notice_sink(&self, sink: remote::NoticeSink) {
        self.remote.set_notice_sink(sink);
    }

    /// Refresh the account profile from the relay, for tests and explicit UI refreshes.
    pub async fn refresh_account(
        &self,
    ) -> Result<Option<agi_protocol::account::AccountUser>, String> {
        self.account
            .refresh()
            .await
            .map_err(|error| format!("{}: {}", error.code, error.message))
    }

    /// The account this window is signed in to, when it is.
    pub fn account_user(&self) -> Option<agi_protocol::account::AccountUser> {
        self.account.user()
    }

    /// The session and device id the tunnel presents, when signed in.
    pub fn relay_account(&self) -> Option<(String, String)> {
        self.account.session()
    }

    /// Whether the tunnel is up, for the remote check and anything else that
    /// needs to know before it promises the paid route.
    pub fn relay_connected(&self) -> bool {
        self.relay_status
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .is_some_and(|source| source.relay_status().connected)
    }

    /// Follow sign-ins and sign-outs.
    pub fn subscribe_account(&self) -> tokio::sync::broadcast::Receiver<account::AccountEvent> {
        self.account.subscribe()
    }

    /// Refresh the account in the background, at launch.
    pub fn refresh_account_in_background(self: &Arc<Self>) {
        let core = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = core.account.refresh().await {
                tracing::debug!("account: could not refresh at launch — {error:?}");
            }
        });
    }

    /// The appearance the window should start with.
    pub fn theme(&self) -> String {
        self.preferences
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .theme
            .clone()
    }

    /// Remember the appearance for the next launch.
    pub fn set_theme(&self, theme: &str) {
        if !valid_theme(theme) {
            return;
        }
        let path = {
            let mut preferences = self
                .preferences
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if preferences.theme == theme {
                return;
            }
            preferences.theme = theme.to_string();
            self.preferences_path
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        };
        let Some(path) = path else {
            return;
        };
        let _ = path;
        self.save_preferences();
    }

    /// Whether the first-run guide has been completed on this install.
    pub fn onboarding_done(&self) -> bool {
        self.preferences
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .onboarding
            .done
    }

    /// Remember that the guide finished, or re-arm it from the settings page.
    pub fn set_onboarding_done(&self, done: bool) -> bool {
        {
            let mut preferences = self
                .preferences
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if preferences.onboarding.done == done {
                return preferences.onboarding.done;
            }
            preferences.onboarding.done = done;
        }
        self.save_preferences();
        done
    }

    /// Whether an agent was configured, by the guide or from its page.
    pub fn engine_configured(&self, engine: EngineId) -> bool {
        self.preferences
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .engines
            .get(engine)
    }

    /// Remember an agent's configured flag.
    pub fn set_engine_configured(&self, engine: EngineId, enabled: bool) {
        {
            let mut preferences = self
                .preferences
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if preferences.engines.get(engine) == enabled {
                return;
            }
            preferences.engines.set(engine, enabled);
        }
        self.save_preferences();
    }

    /// Remember one LAN beacon, if it belongs to this account and LAN is on.
    pub fn record_lan_beacon(&self, beacon: &agi_lan::LanBeacon, from: std::net::SocketAddr) {
        self.record_lan_beacon_ip(beacon, from.ip());
    }

    /// Same, for mDNS, which reports an address without a socket.
    pub fn record_lan_beacon_ip(&self, beacon: &agi_lan::LanBeacon, ip: std::net::IpAddr) {
        let prefs = self.remote_prefs();
        if !prefs.enabled || !prefs.allow_lan {
            return;
        }
        let Some(account) = self.account.user_id() else {
            return;
        };
        if !beacon.matches_account(&account)
            || beacon.device_id == self.account.device_id().unwrap_or_default()
        {
            return;
        }
        let peer = LanPeer {
            device_id: beacon.device_id.clone(),
            name: beacon.name.clone(),
            address: ip.to_string(),
            port: beacon.port,
            certificate_fingerprint: beacon.certificate_fingerprint.clone(),
            last_seen_ms: agi_lan::now_ms(),
        };
        self.lan_peers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(peer.device_id.clone(), peer);
    }

    /// LAN peers whose beacon is still fresh.
    pub fn lan_peers(&self) -> Vec<LanPeer> {
        let now = agi_lan::now_ms();
        self.lan_peers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, peer| now - peer.last_seen_ms <= LAN_PEER_TTL_MS);
        self.lan_peers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Load a cached certificate, verify it against this identity, and keep it.
    pub fn load_lan_certificate(&self, path: std::path::PathBuf) {
        *self
            .lan_certificate_path
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path.clone());
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(answer) = serde_json::from_str::<account::LanCertificateAnswer>(&text) else {
            return;
        };
        let Ok(identity) = self.lan_identity() else {
            return;
        };
        if self.verify_lan_certificate(&answer, &identity).is_err() {
            return;
        }
        *self
            .lan_certificate
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(answer);
    }

    fn verify_lan_certificate(
        &self,
        answer: &account::LanCertificateAnswer,
        identity: &agi_lan::DeviceIdentity,
    ) -> Result<(), String> {
        let public = identity.public();
        if answer
            .body
            .ed25519_key()
            .map_err(|error| error.to_string())?
            != public.ed25519
            || answer
                .body
                .x25519_key()
                .map_err(|error| error.to_string())?
                != public.x25519
        {
            return Err("the certificate is for different device keys".to_string());
        }
        let ca = agi_lan::decode_public_key(&answer.ca_public_key)
            .map_err(|error| format!("the relay's CA key is malformed: {error}"))?;
        let certificate = agi_lan::DeviceCertificate {
            body: answer.body.clone(),
            signature: answer.signature.clone(),
        };
        certificate
            .verify(
                &ca,
                &answer.body.account_id,
                &answer.body.device_id,
                agi_lan::now_ms(),
            )
            .map_err(|error| format!("the relay's certificate is not valid: {error}"))
    }

    fn save_lan_certificate(&self) {
        let path = self
            .lan_certificate_path
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(path) = path else {
            return;
        };
        let answer = self
            .lan_certificate
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(answer) = answer else {
            return;
        };
        let Ok(bytes) = serde_json::to_vec_pretty(&answer) else {
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

    fn lan_identity(&self) -> Result<Arc<agi_lan::DeviceIdentity>, String> {
        if let Some(identity) = self
            .lan_identity
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            return Ok(identity);
        }
        let identity = Arc::new(
            agi_lan::DeviceIdentity::load_or_create(agi_lan::default_identity_path())
                .map_err(|error| error.to_string())?,
        );
        *self
            .lan_identity
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&identity));
        Ok(identity)
    }

    /// The public LAN identity and certificate status, for a settings page.
    pub fn lan_status(&self) -> Result<Value, String> {
        let identity = self.lan_identity()?;
        let public = identity.public();
        let certificate = self
            .lan_certificate
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Ok(json!({
            "deviceId": self.account.device_id(),
            "ed25519Public": agi_lan::public_key_b64(public.ed25519),
            "x25519Public": agi_lan::public_key_b64(public.x25519),
            "certificate": certificate.as_ref().map(|answer| &answer.body),
            "caPublicKey": certificate.as_ref().map(|answer| &answer.ca_public_key),
            "path": agi_lan::default_identity_path(),
        }))
    }

    /// Fetch or renew the account CA certificate and verify it locally.
    pub async fn refresh_lan_certificate(&self) -> Result<Value, String> {
        let identity = self.lan_identity()?;
        let public = identity.public();
        let answer = self
            .account
            .lan_certificate(
                &agi_lan::public_key_b64(public.ed25519),
                &agi_lan::public_key_b64(public.x25519),
            )
            .await
            .map_err(|error| format!("{}: {}", error.code, error.message))?;
        self.verify_lan_certificate(&answer, &identity)?;
        *self
            .lan_certificate
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(answer);
        self.save_lan_certificate();
        self.lan_status()
    }

    /// The credentials a LAN listener/client needs, fetched on demand.
    pub async fn lan_credentials(&self) -> Result<agi_lan::LanCredentials, String> {
        let prefs = self.remote_prefs();
        if !prefs.enabled || !prefs.allow_lan {
            return Err("LAN peers are disabled on this computer".to_string());
        }
        let account_id = self
            .account
            .user_id()
            .ok_or_else(|| "sign in to the account first".to_string())?;
        let identity = self.lan_identity()?;
        let needs_certificate = {
            let certificate = self
                .lan_certificate
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            certificate
                .as_ref()
                .is_none_or(|answer| answer.body.expires_at <= agi_lan::now_ms())
        };
        if needs_certificate {
            self.refresh_lan_certificate().await?;
        }
        let answer = self
            .lan_certificate
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| "the account did not issue a LAN certificate".to_string())?;
        let ca_public =
            agi_lan::decode_public_key(&answer.ca_public_key).map_err(|error| error.to_string())?;
        Ok(agi_lan::LanCredentials {
            identity,
            certificate: agi_lan::DeviceCertificate {
                body: answer.body,
                signature: answer.signature,
            },
            trust: agi_lan::LanTrust {
                ca_public,
                account_id,
            },
        })
    }

    /// The account CA public key, when a certificate has been fetched.
    pub fn lan_ca_public(&self) -> Option<[u8; 32]> {
        self.lan_certificate
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .and_then(|answer| agi_lan::decode_public_key(&answer.ca_public_key).ok())
    }

    /// Whether the operator refused this device id.
    pub fn peer_is_blocked(&self, device_id: &str) -> bool {
        self.remote_prefs()
            .blocked
            .iter()
            .any(|blocked| blocked == device_id)
    }

    /// What this computer allows a same-account peer to do.
    pub fn remote_prefs(&self) -> RemotePrefs {
        self.preferences
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remote
            .clone()
    }

    /// Replace the remote-peer policy; no-op when nothing changed.
    pub fn set_remote_prefs(&self, next: RemotePrefs) {
        {
            let mut preferences = self
                .preferences
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if preferences.remote == next {
                return;
            }
            preferences.remote = next;
        }
        self.save_preferences();
        let _ = self.remote_prefs_changed.send(());
    }

    /// Watch remote-policy changes; used by live peer sockets to close promptly.
    pub fn subscribe_remote_prefs(&self) -> broadcast::Receiver<()> {
        self.remote_prefs_changed.subscribe()
    }

    /// Whether the remote policy would accept this exact workspace path.
    pub fn remote_workspace_allowed(&self, path: &str) -> bool {
        let prefs = self.remote_prefs();
        if !prefs.enabled {
            return false;
        }
        if prefs.allow_all_workspaces {
            return true;
        }
        let requested = normalize_workspace(path);
        prefs
            .workspaces
            .iter()
            .any(|candidate| normalize_workspace(candidate) == requested)
    }

    /// Write the preference file the way the endpoint file is written: private,
    /// and atomically, so a crash mid-write cannot leave half a JSON file where
    /// the next launch expects its settings.
    fn save_preferences(&self) {
        let path = self
            .preferences_path
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(path) = path else {
            return;
        };
        let stored = self
            .preferences
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let bytes = serde_json::to_vec_pretty(&stored).unwrap_or_default();
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

    /// The version reported in every snapshot.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Subscribe to the API event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<ApiEvent> {
        self.events.subscribe()
    }

    /// The engine with this id, if it is hosted.
    pub fn engine(&self, id: EngineId) -> Option<Arc<dyn Engine>> {
        if let Some(engine) = self.engines.iter().find(|engine| engine.id() == id) {
            return Some(Arc::clone(engine));
        }
        self.late
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&id)
            .cloned()
    }

    /// Which agents this machine does not have, and why.
    pub fn set_unavailable(&self, entries: Vec<(EngineId, String)>) {
        let mut slot = self
            .unavailable
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slot.clear();
        for (id, reason) in entries {
            slot.insert(id, reason);
        }
    }

    /// Hand the kernel the hook installer, so the guide can run it.
    pub fn set_claude_hooks(&self, install: HookInstaller) {
        *self
            .claude_hooks
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(install);
    }

    /// Run the hook installer, if this build has one.
    fn install_claude_hooks(&self) -> Result<(), String> {
        let install = self
            .claude_hooks
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        match install {
            Some(install) => install(),
            None => Err("this build cannot install the Claude Code hooks".to_string()),
        }
    }

    /// The DSH helper plugin's last known state.
    pub fn dsh_plugin_view(&self) -> DshPluginView {
        self.dsh_plugin
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Re-read the installed plugin and, when asked, npm's newest release.
    ///
    /// The installed half is a file read and always happens, so a repair that
    /// just finished is visible immediately. The network half is optional: the
    /// background watcher and the explicit check ask for it, while a snapshot
    /// rebuild does not.
    pub async fn refresh_dsh_plugin(&self, force: bool) -> DshPluginView {
        let _one_at_a_time = self.plugin_lock.lock().await;
        let home = plugin::dsh_home();
        let installed =
            plugin::installed_version(&home.join("profiles").join(plugin::profile_name(&home)));
        let current = self.dsh_plugin_view();
        let (latest, error, last_checked) = if force {
            match plugin::latest_version().await {
                Ok(latest) => (Some(latest), None, Some(epoch_ms())),
                Err(reason) => (None, Some(reason), current.last_checked),
            }
        } else {
            (current.latest, current.error, current.last_checked)
        };
        let view = plugin::view(installed, latest, error, last_checked);
        *self
            .dsh_plugin
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = view.clone();
        self.publish(ApiEvent::DshPluginChanged {
            plugin: view.clone(),
        });
        view
    }

    /// Install or update `dsh-mizar`, optionally pinning a version.
    ///
    /// `None` means "whatever npm says is newest", which is what the update
    /// popup asks for. A repair passes the version it already decided on, so a
    /// second registry answer cannot change what it is about to install. A
    /// target older than the installed one is replaced by the installed one:
    /// this button repairs and updates, it never downgrades.
    pub async fn install_dsh_plugin(
        &self,
        version: Option<String>,
    ) -> Result<DshPluginView, String> {
        let _one_at_a_time = self.plugin_lock.lock().await;
        let current = self.dsh_plugin_view();
        let mut fetched: Option<String> = None;
        let requested = match version {
            Some(version) => version,
            None => match current.latest.clone() {
                Some(latest) => latest,
                None => {
                    let latest = plugin::latest_version().await?;
                    fetched = Some(latest.clone());
                    latest
                }
            },
        };
        let target = match current.installed.as_deref() {
            Some(installed) if plugin::release_is_newer(installed, &requested) => {
                installed.to_string()
            }
            _ => requested,
        };
        if plugin::release_is_newer(DSH_PLUGIN_MIN_VERSION, &target) {
            return Err(format!(
                "npm 上的 {target} 低于本应用要求的 {}，暂时不能自动安装",
                DSH_PLUGIN_MIN_VERSION
            ));
        }

        plugin::install(&target).await?;
        let home = plugin::dsh_home();
        let installed =
            plugin::installed_version(&home.join("profiles").join(plugin::profile_name(&home)));
        let latest = fetched.or(current.latest);
        let view = plugin::view(installed, latest, None, Some(epoch_ms()));
        *self
            .dsh_plugin
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = view.clone();
        self.publish(ApiEvent::DshPluginChanged {
            plugin: view.clone(),
        });
        Ok(view)
    }

    /// Start the once-at-launch and every-few-hours plugin check.
    ///
    /// Lives here but is started by the boot sequence rather than by
    /// [`Core::start`]: a test kernel has no registry to ask and no window to
    /// pop a dialog in, and `cargo test` must not depend on npm being reachable.
    pub fn start_dsh_plugin_watch(self: &Arc<Self>) {
        let core = Arc::clone(self);
        tokio::spawn(async move {
            // Let the window draw before the first request competes with the
            // boot traffic; four seconds is under a person's patience for a
            // popup and over the burst of session listings.
            tokio::time::sleep(Duration::from_secs(4)).await;
            loop {
                if core
                    .status_of(EngineId::Dsh)
                    .is_some_and(|status| status.available)
                {
                    core.refresh_dsh_plugin(true).await;
                }
                tokio::time::sleep(DSH_PLUGIN_CHECK_INTERVAL).await;
            }
        });
    }

    /// Hand the kernel a way to look for agents again.
    pub fn set_rescan(&self, rescan: RescanFn) {
        *self
            .rescan
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(rescan);
    }

    /// Look for every agent again, host the ones that appeared, and say what is
    /// still missing.
    pub fn rescan_engines(&self) -> Vec<EngineStatus> {
        let rescan = self
            .rescan
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(rescan) = rescan {
            let mut already: Vec<EngineId> =
                self.engines.iter().map(|engine| engine.id()).collect();
            already.extend(
                self.late
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .keys()
                    .copied(),
            );
            let (hosted, missing) = rescan(&already);
            {
                let mut slot = self
                    .late
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                for engine in hosted {
                    slot.insert(engine.id(), engine);
                }
            }
            self.set_unavailable(missing.into_iter().collect());
        }
        EngineId::all()
            .into_iter()
            .filter_map(|id| self.status_of(id))
            .collect()
    }

    /// Start listening to every engine and seed the store from what they know.
    ///
    /// Called once, after construction, because it needs the async runtime. A
    /// failure to list one engine is logged and does not stop the others: a
    /// kernel where Codex is unavailable should still show DSH.
    pub async fn start(self: &Arc<Self>) {
        // Model changes are the kernel's to publish: the downloader and the
        // sidecar only know their own state.
        {
            let mut models = self.model_service.subscribe();
            let core = Arc::clone(self);
            tokio::spawn(async move {
                while let Ok(event) = models.recv().await {
                    match event {
                        ModelEvent::Progress(download) => {
                            core.publish(ApiEvent::ModelProgress { download })
                        }
                        ModelEvent::Changed(model) => {
                            // A finished download is a new judge: without this the
                            // kernel would keep the rule fallback until the next
                            // launch.
                            core.sync_layout();
                            core.publish(ApiEvent::ModelChanged { model: *model })
                        }
                    }
                }
            });
        }

        for engine in &self.engines {
            let id = engine.id();

            match engine.sessions().await {
                Ok(sessions) => {
                    let mut store = self.store_write();
                    for session in sessions {
                        store.sessions.insert(session.key.clone(), session);
                    }
                }
                Err(error) => {
                    tracing::debug!("{id}: no session list yet — {error}");
                }
            }

            let mut events = engine.subscribe();
            let core = Arc::clone(self);
            tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(event) => core.on_engine_event(id, event),
                        // Lagging means events were dropped; the store is still
                        // correct for anything a client can re-read, so keep
                        // going rather than tearing the subscription down.
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!("{id}: fell {skipped} events behind");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        // The dataset uploader and the one-year backfill. It is a single task,
        // asleep most of the time: consent is off by default, and a machine
        // that never opted in should pay one timer, not a thread.
        dataset::spawn(Arc::downgrade(self));
    }

    fn store_read(&self) -> std::sync::RwLockReadGuard<'_, Store> {
        self.store
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Read back the judgement for one turn, if the shadow judge has produced one.
    pub fn layout_of(&self, session: &SessionKey, turn: u32) -> Option<Layout> {
        self.layouts
            .lock()
            .ok()?
            .get(&(session.clone(), turn))
            .cloned()
    }

    /// Feed one record into a session's state machine and emit what it says.
    fn advance(&self, session: &SessionKey, record: &Record) {
        let mut frames: Vec<StateFrame> = Vec::new();
        {
            let Ok(mut map) = self.states.lock() else {
                return;
            };
            let machine = map.entry(session.clone()).or_insert_with(StateMachine::new);

            if record.kind == RecordKind::TurnStart {
                // A turn start is not a live signal: it also resets what the
                // session has streamed this turn, which is what the journal
                // replays to a phone that was away.
                match record.turn {
                    Some(turn) => frames.extend(machine.on_turn_start(turn)),
                    // An engine that does not number its turns gives nothing to
                    // key the reset on; the old live-signal behaviour stands.
                    None => frames.extend(machine.on_live(Live::Thinking)),
                }
            } else if record.kind == RecordKind::AssistantMessage {
                if let Some(text) = record.text.as_deref() {
                    frames.extend(machine.on_text(text));
                }
            } else if let Some(live) = live_of(record) {
                if let Some(frame) = machine.on_live(live) {
                    frames.push(frame);
                }
            }
        }
        self.emit(session, frames);
    }

    /// Announce a finished turn's resulting state.
    fn settle(
        &self,
        session: &SessionKey,
        facts: &agi_layout::Facts,
        verdict: Option<agi_layout::State>,
    ) {
        let frames = {
            let Ok(mut map) = self.states.lock() else {
                return;
            };
            let machine = map.entry(session.clone()).or_insert_with(StateMachine::new);
            machine.on_turn_end(facts, verdict)
        };
        self.emit(session, frames);
    }

    /// Send frames to whoever is listening, and to the log.
    fn emit(&self, session: &SessionKey, frames: Vec<StateFrame>) {
        if !frames.is_empty()
            && !self
                .heartbeat_started
                .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            start_heartbeat(self.states.clone(), self.frames.clone());
        }
        for frame in frames {
            tracing::debug!(
                session = %session.id,
                state = frame.state.text(),
                interval = frame.interval,
                bytes = agi_protocol::state_frame::HEADER_LEN + frame.text.len(),
                "state frame"
            );
            let _ = self.frames.send((session.clone(), frame.encode()));
        }
    }

    /// Subscribe to encoded state frames, one per state change or heartbeat.
    ///
    /// The transport is not wired yet: this is the seam the sync channel will
    /// read from, and the reason the kernel never touches the wire itself.
    pub fn subscribe_states(&self) -> broadcast::Receiver<(SessionKey, Vec<u8>)> {
        self.frames.subscribe()
    }

    /// The state a session is in, for the desktop UI.
    pub fn session_state(&self, session: &SessionKey) -> Option<(SessionState, u32)> {
        let map = self.states.lock().ok()?;
        let machine = map.get(session)?;
        Some((machine.state(), machine.interval()))
    }

    /// What this session has streamed in the current turn, for a phone that was
    /// away while it happened.
    ///
    /// The live channel only carries what is happening now, so a phone that
    /// disconnected mid-turn comes back to an empty interval and no way to learn
    /// what it missed. This is the turn's journal: every interval it opened, in
    /// order, with the text each one carried.
    ///
    /// `None` when the session never ran a turn here (an old session, or one
    /// that only exists on the engine side) -- which is "nothing to replay", not
    /// an error.
    pub fn stream_of(&self, session: &SessionKey) -> Option<StreamReplay> {
        let map = self.states.lock().ok()?;
        let machine = map.get(session)?;
        Some(StreamReplay {
            turn: machine.turn(),
            state: machine.state(),
            interval: machine.interval(),
            text_seq: machine.text_seq(),
            entries: machine.journal().to_vec(),
            truncated: machine.truncated(),
        })
    }

    /// The model service, for the dispatch layer and the snapshot.
    pub fn models(&self) -> &Arc<ModelService> {
        &self.model_service
    }

    /// Switch models while the app runs.
    ///
    /// A swapped model is a file plus one call: no restart, no rebuild, no
    /// phone-side change. The judge is rebuilt because it holds the service, not
    /// the manifest.
    pub fn activate_model(&self, id: &str) -> Result<ModelManifest, String> {
        let manifest = self.model_service.activate(id)?;
        self.rebuild_layout(&manifest);
        Ok(manifest)
    }

    /// Re-read the model the service holds and install (or remove) the judge.
    ///
    /// Called when a download finishes and when the window switches models: the
    /// judge is built from the service, so the port is not frozen at launch.
    fn sync_layout(&self) {
        match self.model_service.manifest() {
            Some(manifest) => {
                let judge = Arc::new(models::SidecarJudge::new(Arc::clone(&self.model_service)));
                if let Ok(mut slot) = self.layout.write() {
                    *slot = Some(Arc::new(LayoutService::new(judge)));
                }
                if let Ok(mut slot) = self.manifest.write() {
                    *slot = Some(manifest);
                }
            }
            None => {
                if let Ok(mut slot) = self.layout.write() {
                    *slot = None;
                }
            }
        }
    }

    fn rebuild_layout(&self, manifest: &ModelManifest) {
        let judge = Arc::new(models::SidecarJudge::new(Arc::clone(&self.model_service)));
        if let Ok(mut slot) = self.layout.write() {
            *slot = Some(Arc::new(LayoutService::new(judge)));
        }
        if let Ok(mut slot) = self.manifest.write() {
            *slot = Some(manifest.clone());
        }
        tracing::info!(model = %manifest.id, version = %manifest.version, "layout: model switched");
    }

    /// The model currently answering, if any.
    pub fn active_model(&self) -> Option<ModelManifest> {
        self.manifest.read().ok().and_then(|m| m.clone())
    }

    /// Judge a finished turn without blocking the event loop.
    ///
    /// Shadow mode: the answer lands in `layouts` and in the log, and nothing is
    /// pushed to clients. Promotion to the wire is a deliberate, separate change.
    fn judge_turn(self: &Arc<Self>, session: SessionKey, turn: Option<u32>, records: Vec<Record>) {
        // The judge's *presence* is the lease: `phone_attached` installs it, and
        // `release_judge_if_idle` takes it away after five minutes with nobody
        // listening and nothing running. Re-checking the socket count here would
        // stop the verdict a sleeping phone's Live Activity is waiting for --
        // the one moment the island is the only thing the operator can see.
        let Some(service) = self.layout.read().ok().and_then(|l| l.clone()) else {
            return;
        };
        let Some(turn) = turn else {
            return;
        };
        // One verdict per (session, turn): the DSH path can call here twice
        // (a live `turn/end` record and the fallback for engines that do not
        // emit records), and a second judgement would only re-announce a state
        // the phone already has.
        if self
            .layouts
            .lock()
            .map(|map| map.contains_key(&(session.clone(), turn)))
            .unwrap_or(false)
        {
            return;
        }
        let facts = facts_for(&session, turn, &records);
        let store = self.layouts.clone();
        let core = Arc::clone(self);
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let layout = service.judge_turn(Some(turn), &facts).await;
            // The turn's verdict becomes the session's state; the frames go out
            // through the same channel as every live signal.
            core.settle(&session, &facts, Some(layout.state));
            tracing::debug!(
                session = %session.id,
                turn,
                state = layout.state.as_str(),
                source = ?layout.source,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "layout judged"
            );
            if let Ok(mut map) = store.lock() {
                map.insert((session, turn), layout);
            }
        });
    }

    /// Judge a turn that just finished on an engine that does not emit records.
    ///
    /// The state channel is built on [`EngineEvent::RecordAppended`], and only the
    /// DSH adapter produces that: its plugin forwards every projected event.
    /// Claude Code and Codex are read back from their own logs or app-server, so
    /// no record ever arrives and the judge used to be reachable only on DSH.
    /// This turns the session's `running: true -> false` transition into the same
    /// question, by asking the engine for its newest records. DSH keeps the
    /// event-driven path (and is skipped here) so the Mac behaviour does not
    /// change.
    fn judge_finished_turn(self: &Arc<Self>, session: SessionKey) {
        let core = Arc::clone(self);
        tokio::spawn(async move {
            let Some(engine) = core.engine(session.engine) else {
                return;
            };
            let Ok(records) = engine.records(&session, 200, None).await else {
                return;
            };
            let Some((turn, slice)) = last_finished_turn(&records) else {
                return;
            };
            // The dataset capture happens before the judge, and independently of
            // it: a machine with no model still has turns worth sharing.
            let facts = facts_for(&session, turn, &slice);
            core.capture_dataset_turn(&session, &facts, &slice);
            core.judge_turn(session, Some(turn), slice);
        });
    }

    fn store_write(&self) -> std::sync::RwLockWriteGuard<'_, Store> {
        self.store
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn publish(&self, event: ApiEvent) {
        // No subscribers is the normal state before a window opens.
        let _ = self.events.send(event);
    }

    /// Fold one engine event into the store and announce it.
    fn on_engine_event(self: &Arc<Self>, engine: EngineId, event: EngineEvent) {
        match event {
            EngineEvent::SessionUpdated(session) => {
                // A turn that just stopped is the end-of-turn signal for the
                // engines that do not stream records (Claude Code, Codex).
                let was_running = self
                    .store
                    .read()
                    .ok()
                    .and_then(|store| store.sessions.get(&session.key).map(|old| old.running))
                    .unwrap_or(false);
                let finished = was_running && !session.running;
                let now_running = session.running;
                let key = session.key.clone();
                {
                    let mut store = self.store_write();
                    store.sessions.insert(session.key.clone(), session.clone());
                }
                self.publish(ApiEvent::SessionUpdated { session });
                // The lamp is derived from "is a turn running", so a start or a
                // stop has to move it before the next snapshot poll would.
                if was_running != now_running {
                    if let Some(status) = self.status_of(engine) {
                        self.publish(ApiEvent::EngineStatus { status });
                    }
                }
                // DSH reports the same finish through `RecordAppended`, which
                // already runs the judge; asking twice would judge one turn twice.
                if finished && key.engine != EngineId::Dsh {
                    self.judge_finished_turn(key);
                }
            }
            EngineEvent::SessionRemoved(key) => {
                {
                    let mut store = self.store_write();
                    store.sessions.remove(&key);
                    store.records.remove(&key);
                }
                self.publish(ApiEvent::SessionRemoved { session: key });
                // An archived session may have been the only running one.
                if let Some(status) = self.status_of(engine) {
                    self.publish(ApiEvent::EngineStatus { status });
                }
            }
            EngineEvent::RecordAppended { session, record } => {
                let turn_slice = {
                    let mut store = self.store_write();
                    let bucket = store.records.entry(session.clone()).or_default();
                    bucket.push_back(record.clone());
                    while bucket.len() > MAX_RECORDS {
                        bucket.pop_front();
                    }
                    if record.kind == RecordKind::TurnEnd {
                        Some((turn_records(bucket, record.turn), record.turn))
                    } else {
                        None
                    }
                };
                self.advance(&session, &record);
                if let Some((slice, turn)) = turn_slice {
                    // Structure settles the turn immediately, so a session never
                    // stays stuck in "thinking" when no model is configured. A
                    // model verdict can correct it a moment later; an agreeing
                    // verdict sends nothing.
                    let facts = facts_for(&session, turn.unwrap_or(0), &slice);
                    self.capture_dataset_turn(&session, &facts, &slice);
                    self.settle(&session, &facts, None);
                    self.judge_turn(session.clone(), turn, slice);
                }
                self.publish(ApiEvent::RecordAppended { session, record });
            }
            EngineEvent::InteractionRequested(interaction) => {
                {
                    let mut store = self.store_write();
                    store
                        .interaction_engine
                        .insert(interaction.id.clone(), engine);
                    store
                        .interactions
                        .insert(interaction.id.clone(), interaction.clone());
                }
                self.publish(ApiEvent::InteractionRequested { interaction });
            }
            EngineEvent::InteractionResolved { id } => {
                {
                    let mut store = self.store_write();
                    store.interactions.remove(&id);
                    store.interaction_engine.remove(&id);
                }
                self.publish(ApiEvent::InteractionResolved { id });
            }
            EngineEvent::StatusChanged { .. } => {
                // The status is read from the engine itself rather than carried
                // in the event, so a reconnect cannot publish a stale detail.
                if let Some(status) = self.status_of(engine) {
                    self.publish(ApiEvent::EngineStatus { status });
                }
            }
            EngineEvent::Notice { level, message } => {
                self.publish(ApiEvent::Notice { level, message });
            }
        }
    }

    /// One engine's status, as the engine switch draws it.
    fn status_of(&self, id: EngineId) -> Option<EngineStatus> {
        let configured = self.engine_configured(id);
        let missing = self
            .unavailable
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&id)
            .cloned();
        let (sessions, running) = {
            let store = self.store_read();
            let mut sessions = 0usize;
            let mut running = false;
            for (key, session) in &store.sessions {
                if key.engine == id {
                    sessions += 1;
                    running |= session.running;
                }
            }
            (sessions, running)
        };

        match self.engine(id) {
            Some(engine) => {
                let (connected, detail) = engine.status();
                Some(EngineStatus {
                    engine: id,
                    connected,
                    // Hosted is not the same as installed: Claude's engine runs
                    // against transcripts even when its CLI is nowhere.
                    available: missing.is_none(),
                    configured,
                    // Claude and Codex have no standing socket: their engine
                    // status is "the adapter can take a prompt", not "something
                    // is attached". A lamp that stayed on for a machine that was
                    // never opened is why the running turn is the evidence.
                    engaged: configured && connected && (id == EngineId::Dsh || running),
                    capabilities: engine.capabilities(),
                    detail,
                    sessions,
                })
            }
            // Known but not hosted: say so instead of dropping the row. The
            // window has to be able to tell "not installed" from "installed and
            // quiet", and only this row carries that answer.
            None => {
                let reason = missing?;
                Some(EngineStatus {
                    engine: id,
                    connected: false,
                    available: false,
                    configured,
                    engaged: false,
                    capabilities: agi_protocol::engine::Capabilities::default(),
                    detail: Some(reason),
                    sessions,
                })
            }
        }
    }

    /// Everything a freshly opened window needs.
    pub fn snapshot(&self) -> StateSnapshot {
        // `status_of` takes the store read lock itself. Taking it here first
        // makes that a recursive read; once a writer is queued, the second read
        // waits behind it while the first is still held, and every caller of
        // snapshot blocks. Compute the engine rows before taking the guard.
        let engines = EngineId::all()
            .into_iter()
            .filter_map(|id| self.status_of(id))
            .collect();
        let store = self.store_read();

        let mut sessions: Vec<Session> = store.sessions.values().cloned().collect();
        // Newest first, which is the only order a session list is ever read in.
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));

        StateSnapshot {
            version: self.version.clone(),
            // The spelling the window shows and the relay records. Every client
            // asks the same seam rather than guessing from `cfg!(...)`.
            platform: agi_platform::platform().to_string(),
            theme: self.theme(),
            engines,
            sessions,
            interactions: store.interactions.values().cloned().collect(),
            onboarding: OnboardingView {
                done: self.onboarding_done(),
            },
            model: self.model_service.state(),
            devices: self.device_views(),
            dataset: dataset::view(self),
            dsh_plugin: self.dsh_plugin_view(),
        }
    }

    /// Every day of work, across every engine, as the home page charts it.
    ///
    /// Asked of all three adapters at once: each one folds its own store (two
    /// file scans and a link fold), and doing them one after another makes the
    /// window wait for the sum of the three.
    ///
    /// An engine that cannot answer — a machine that has never run Codex, an
    /// adapter that has no usage records — is skipped rather than failing the
    /// command. A year of Claude Code history is worth showing on a host where
    /// Codex is not installed, and the alternative is an error panel instead of
    /// two working charts.
    async fn usage_overview(&self) -> CommandResponse {
        let answers = futures_util::future::join_all(
            self.engines
                .iter()
                .map(|engine| async move { (engine.id(), engine.usage_history().await) }),
        )
        .await;

        // One accumulator for the three, so a date that two engines worked on is
        // one row rather than two rows the window has to add up itself.
        let mut days = agi_engine::UsageDays::new();
        for (id, answer) in answers {
            match answer {
                Ok(found) => days.merge(found),
                Err(error) => tracing::debug!("{id}: no usage history — {error}"),
            }
        }

        CommandResponse::ok(UsageOverview {
            sessions: self.counted_sessions(),
            days: days.finish(),
        })
    }

    /// Top-level sessions across every engine.
    ///
    /// Archived sessions are not in the store at all: archiving is answered by
    /// the engine withdrawing the session (`EngineEvent::SessionRemoved`), so
    /// being in the snapshot *is* being unarchived. A delegated child — a
    /// subagent, a fork's source — is not a session the operator started, and
    /// counting it would inflate the number they read as "how much have I been
    /// working".
    fn counted_sessions(&self) -> u64 {
        self.snapshot()
            .sessions
            .iter()
            // A delegated child and a codex fork are both conversations the
            // operator did not start; the dashboard counts the ones they did.
            .filter(|session| !matches!(session.origin.as_deref(), Some("subagent") | Some("fork")))
            .count() as u64
    }

    /// The rows a peer may see, without the target's session ids.
    ///
    /// The target already strips these for a real peer, but a relay that still
    /// issues a phone token would not; dropping them here keeps the options
    /// response about paths, not about unrelated sessions.
    fn remote_workspace_rows(workspaces: &Value) -> Vec<Value> {
        workspaces
            .get("items")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .cloned()
                    .map(|mut row| {
                        if let Some(object) = row.as_object_mut() {
                            object.remove("sessionIds");
                        }
                        row
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Ask a peer what it permits before the conversation chooses a workspace
    /// and a permission preset.
    async fn remote_options(&self, peer: String) -> CommandResponse {
        if self.remote_prefs().allow_lan {
            if let Some(lan) = self
                .lan_peers()
                .into_iter()
                .find(|candidate| candidate.device_id == peer)
            {
                let credentials = self.lan_credentials().await.ok();
                let address = format!("{}:{}", lan.address, lan.port)
                    .parse::<std::net::SocketAddr>()
                    .ok();
                if let (Some(credentials), Some(address)) = (credentials, address) {
                    if let Ok(client) =
                        agi_lan::LanMizarClient::connect(address, &credentials).await
                    {
                        let workspaces = client.workspaces().await;
                        let permissions = client.permissions().await;
                        let models = client.models().await;
                        if let (Ok(workspaces), Ok(permissions), Ok(models)) =
                            (workspaces, permissions, models)
                        {
                            return CommandResponse::ok(json!({
                                "peer": peer,
                                "route": "lan",
                                "workspaces": Self::remote_workspace_rows(&workspaces),
                                "permissions": permissions,
                                "models": models,
                            }));
                        }
                    }
                }
            }
        }

        let desktops = match self.account.desktops().await {
            Ok(desktops) => desktops,
            Err(error) => return CommandResponse::error(error.code, error.message),
        };
        let Some(target) = desktops
            .into_iter()
            .find(|desktop| desktop.device_id == peer)
        else {
            return CommandResponse::error(
                "REMOTE_PEER_UNKNOWN",
                "that computer is not on this account",
            );
        };
        if !target.online {
            return CommandResponse::error(
                "REMOTE_PEER_OFFLINE",
                "that computer is not connected to the relay",
            );
        }
        let connection = match self.account.connect(&peer, "dsh").await {
            Ok(connection) => connection,
            Err(error) => return CommandResponse::error(error.code, error.message),
        };
        let credentials = agi_peer::Credentials {
            base: connection.base,
            room: connection.room,
            account_session: connection.account_session,
            device_id: connection.device_id,
            account_id: connection.account_id,
            peer_token: connection.token,
            app_version: self.version.clone(),
        };
        let client = match agi_peer::PeerClient::connect(credentials).await {
            Ok(client) => client,
            Err(error) => {
                return CommandResponse::error("REMOTE_OPTIONS_FAILED", error.to_string())
            }
        };
        let workspaces = match client.workspaces().await {
            Ok(workspaces) => workspaces,
            Err(error) => {
                return CommandResponse::error("REMOTE_OPTIONS_FAILED", error.to_string())
            }
        };
        let permissions = match client.permissions().await {
            Ok(permissions) => permissions,
            Err(error) => {
                return CommandResponse::error("REMOTE_OPTIONS_FAILED", error.to_string())
            }
        };
        let models = match client.models().await {
            Ok(models) => models,
            Err(error) => {
                return CommandResponse::error("REMOTE_OPTIONS_FAILED", error.to_string())
            }
        };
        CommandResponse::ok(json!({
            "peer": peer,
            "route": "wan",
            "workspaces": Self::remote_workspace_rows(&workspaces),
            "permissions": permissions,
            "models": models,
        }))
    }

    /// Run one command.
    pub async fn dispatch(&self, command: Command) -> CommandResponse {
        match command {
            Command::State => CommandResponse::ok(self.snapshot()),
            Command::RelayStatus => {
                let status = self
                    .relay_status
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_ref()
                    .map(|source| source.relay_status())
                    .unwrap_or_default();
                CommandResponse::ok(json!({
                    "connected": status.connected,
                    "room": status.room,
                }))
            }
            Command::Account => {
                // A reading from the relay is better than a cached one, but a
                // launch with no network must still draw the last known account.
                match self.account.refresh().await {
                    Ok(Some(user)) => CommandResponse::ok(json!({ "user": user })),
                    Ok(None) => CommandResponse::ok(json!({ "user": Value::Null })),
                    Err(error) if self.account.signed_in() => {
                        let cached = self.account.user();
                        if cached.is_some() {
                            tracing::debug!("account: serving the last reading — {error:?}");
                            CommandResponse::ok(json!({ "user": cached }))
                        } else {
                            CommandResponse::error(error.code, error.message)
                        }
                    }
                    Err(error) => CommandResponse::error(error.code, error.message),
                }
            }
            Command::AccountLogin {
                identifier,
                password,
            } => match self.account.login(&identifier, &password).await {
                Ok(user) => CommandResponse::ok(json!({ "user": user })),
                Err(error) => CommandResponse::error(error.code, error.message),
            },
            Command::AccountGithubStart => match self.account.github_start().await {
                Ok((url, state)) => {
                    CommandResponse::ok(json!({ "authorizeUrl": url, "state": state }))
                }
                Err(error) => CommandResponse::error(error.code, error.message),
            },
            Command::AccountGithubExchange { handoff } => {
                match self.account.github_exchange(&handoff).await {
                    Ok(user) => CommandResponse::ok(json!({ "user": user })),
                    Err(error) => CommandResponse::error(error.code, error.message),
                }
            }
            Command::AccountAppleStart => match self.account.apple_start().await {
                Ok((url, state)) => {
                    CommandResponse::ok(json!({ "authorizeUrl": url, "state": state }))
                }
                Err(error) => CommandResponse::error(error.code, error.message),
            },
            Command::AccountAppleExchange { handoff } => {
                match self.account.apple_exchange(&handoff).await {
                    Ok(user) => CommandResponse::ok(json!({ "user": user })),
                    Err(error) => CommandResponse::error(error.code, error.message),
                }
            }
            Command::AccountAppleAwait { state } => match self.account.apple_await(&state).await {
                Ok(user) => CommandResponse::ok(json!({ "user": user })),
                Err(error) => CommandResponse::error(error.code, error.message),
            },
            Command::AccountGithubAwait { state } => {
                match self.account.github_await(&state).await {
                    Ok(user) => CommandResponse::ok(json!({ "user": user })),
                    Err(error) => CommandResponse::error(error.code, error.message),
                }
            }
            Command::OpenExternal { url } => {
                if !url.starts_with("https://") {
                    CommandResponse::error("MALFORMED_FRAME", "only https URLs are opened")
                } else {
                    // The opener is a platform fact — `open`, `start`, or
                    // `xdg-open` — and everything above this line stays
                    // platform-free. Until this was a seam, every non-macOS
                    // build answered "no opener" and the sign-in buttons died.
                    match agi_platform::open_url(&url) {
                        Ok(()) => CommandResponse::ok(json!({ "opened": true })),
                        Err(error) => {
                            CommandResponse::error("OPEN_FAILED", format!("the browser: {error}"))
                        }
                    }
                }
            }
            Command::AccountDesktops => match self.account.desktops().await {
                Ok(desktops) => CommandResponse::ok(json!({ "desktops": desktops })),
                Err(error) => CommandResponse::error(error.code, error.message),
            },
            Command::LanIdentity => match self.lan_status() {
                Ok(value) => CommandResponse::ok(value),
                Err(message) => CommandResponse::error("LAN_IDENTITY_FAILED", message),
            },
            Command::LanCertificate => match self.refresh_lan_certificate().await {
                Ok(value) => CommandResponse::ok(value),
                Err(message) => CommandResponse::error("LAN_CERTIFICATE_FAILED", message),
            },
            Command::RemoteTargetSettings => CommandResponse::ok(self.remote_prefs()),
            Command::SetRemoteTargetSettings {
                enabled,
                allow_lan,
                allow_wan,
                allow_all_workspaces,
                workspaces,
                blocked,
            } => {
                self.set_remote_prefs(RemotePrefs {
                    enabled,
                    allow_lan,
                    allow_wan,
                    allow_all_workspaces,
                    workspaces,
                    blocked,
                });
                CommandResponse::ok(self.remote_prefs())
            }
            Command::RemotePeers => match self.account.desktops().await {
                Ok(desktops) => {
                    let mut peers = Vec::with_capacity(desktops.len());
                    for desktop in desktops
                        .into_iter()
                        .filter(|desktop| !desktop.device_id.is_empty())
                    {
                        let hello = match (&desktop.room, desktop.online) {
                            (Some(room), true) => self.account.probe(room).await.ok(),
                            _ => None,
                        };
                        let remote = hello
                            .as_ref()
                            .and_then(|value| value.get("remote"))
                            .cloned()
                            .unwrap_or_else(|| json!({ "enabled": false }));
                        peers.push(json!({
                            "deviceId": desktop.device_id,
                            "name": desktop.name,
                            "model": desktop.model,
                            "platform": desktop.platform,
                            "online": desktop.online,
                            "room": desktop.room,
                            "routes": if desktop.online { vec!["wan"] } else { Vec::<&str>::new() },
                            "remote": remote,
                        }));
                    }
                    for lan in self.lan_peers() {
                        let address = format!("{}:{}", lan.address, lan.port);
                        if let Some(existing) = peers.iter_mut().find(|peer| {
                            peer.get("deviceId").and_then(Value::as_str)
                                == Some(lan.device_id.as_str())
                        }) {
                            existing["routes"] = json!(["wan", "lan"]);
                            existing["lanAddress"] = json!(address);
                            continue;
                        }
                        peers.push(json!({
                            "deviceId": lan.device_id,
                            "name": lan.name,
                            "model": "",
                            "platform": "",
                            "online": true,
                            "room": Value::Null,
                            "lanAddress": address,
                            "routes": ["lan"],
                            "remote": {
                                "enabled": true,
                                "allowLan": true,
                                "allowWan": false,
                                "workspaces": [],
                            },
                        }));
                    }
                    CommandResponse::ok(json!({
                        "thisDeviceId": self.account.device_id(),
                        "peers": peers,
                    }))
                }
                Err(error) => CommandResponse::error(error.code, error.message),
            },
            Command::RemoteOptions { peer } => self.remote_options(peer).await,
            Command::RemoteDelegate {
                peer,
                cwd,
                permission,
                provider,
                model,
                reasoning_effort,
                prompt,
            } => {
                // A beacon seen recently is the free LAN route; use it before
                // asking the relay for a token.
                if self.remote_prefs().allow_lan {
                    if let Some(lan) = self
                        .lan_peers()
                        .into_iter()
                        .find(|candidate| candidate.device_id == peer)
                    {
                        // A fresh beacon is the free path, but a bad certificate
                        // or address must not strand the delegation: fall
                        // through to the relay below instead of returning here.
                        let lan_credentials = self.lan_credentials().await.ok();
                        let lan_address = format!("{}:{}", lan.address, lan.port)
                            .parse::<std::net::SocketAddr>()
                            .ok();
                        if let (Some(credentials), Some(address)) = (lan_credentials, lan_address) {
                            // A handshake costs one round trip and turns a stale
                            // beacon into a WAN fallback instead of a delegation
                            // that fails after it is recorded.
                            if agi_lan::LanMizarClient::connect(address, &credentials)
                                .await
                                .is_ok()
                            {
                                let reference = remote::PeerRef {
                                    device_id: lan.device_id,
                                    name: lan.name,
                                };
                                let launch = remote::RemoteLaunch {
                                    cwd: cwd.clone(),
                                    permission: permission.clone(),
                                    provider: provider.clone(),
                                    model: model.clone(),
                                    reasoning_effort: reasoning_effort.clone(),
                                };
                                if let Ok(record) = self.remote.start_lan_with_options(
                                    reference,
                                    address,
                                    credentials,
                                    launch,
                                    prompt.clone(),
                                ) {
                                    return CommandResponse::ok(json!({ "delegation": record }));
                                }
                            }
                        }
                    }
                }
                let desktops = match self.account.desktops().await {
                    Ok(desktops) => desktops,
                    Err(error) => return CommandResponse::error(error.code, error.message),
                };
                let Some(target) = desktops
                    .into_iter()
                    .find(|desktop| desktop.device_id == peer)
                else {
                    return CommandResponse::error(
                        "REMOTE_PEER_UNKNOWN",
                        "that computer is not on this account",
                    );
                };
                if !target.online {
                    return CommandResponse::error(
                        "REMOTE_PEER_OFFLINE",
                        "that computer is not connected to the relay",
                    );
                }
                let connection = match self.account.connect(&peer, "dsh").await {
                    Ok(connection) => connection,
                    Err(error) => return CommandResponse::error(error.code, error.message),
                };
                let credentials = agi_peer::Credentials {
                    base: connection.base,
                    room: connection.room,
                    account_session: connection.account_session,
                    device_id: connection.device_id,
                    account_id: connection.account_id,
                    peer_token: connection.token,
                    app_version: self.version.clone(),
                };
                let reference = remote::PeerRef {
                    device_id: target.device_id,
                    name: target.name,
                };
                let launch = remote::RemoteLaunch {
                    cwd,
                    permission,
                    provider,
                    model,
                    reasoning_effort,
                };
                match self
                    .remote
                    .start_with_options(reference, credentials, launch, prompt)
                {
                    Ok(record) => CommandResponse::ok(json!({ "delegation": record })),
                    Err(message) => CommandResponse::error("REMOTE_START_FAILED", message),
                }
            }
            Command::RemoteAudit => CommandResponse::ok(json!({
                "events": self.remote.audit(100),
            })),
            Command::RemoteHealth { repair } => {
                CommandResponse::ok(remote_health::check(self, repair))
            }
            Command::RemoteLogs => {
                let (path, text) = remote_health::log_text(self);
                CommandResponse::ok(json!({ "path": path, "text": text }))
            }
            Command::RemoteInstall { profile, dry_run } => {
                run_remote_installer(false, profile.as_deref(), dry_run)
            }
            Command::RemoteUninstall { profile, dry_run } => {
                run_remote_installer(true, profile.as_deref(), dry_run)
            }
            Command::RemoteDelegations => {
                CommandResponse::ok(json!({ "delegations": self.remote.list() }))
            }
            Command::RemoteResult { id } => match self.remote.get(&id) {
                Some(record) => CommandResponse::ok(json!({
                    "delegation": record,
                    "finalText": record.final_text,
                })),
                None => CommandResponse::error("REMOTE_UNKNOWN", "no such delegation"),
            },
            Command::RemoteCancel { id } => {
                if self.remote.get(&id).is_none() {
                    return CommandResponse::error("REMOTE_UNKNOWN", "no such delegation");
                }
                let cancelled = self.remote.cancel(&id);
                CommandResponse::ok(json!({ "cancelled": cancelled }))
            }
            Command::RemoteAnswer {
                id,
                approval,
                outcome,
            } => {
                if !matches!(outcome.as_str(), "allowed-once" | "rejected") {
                    return CommandResponse::error(
                        "MALFORMED_FRAME",
                        "remote approval outcome must be allowed-once or rejected",
                    );
                }
                let Some(record) = self.remote.get(&id) else {
                    return CommandResponse::error("REMOTE_UNKNOWN", "no such delegation");
                };
                let Some(pending) = record.approval else {
                    return CommandResponse::error(
                        "REMOTE_NO_APPROVAL",
                        "that delegation is not waiting on an approval",
                    );
                };
                if pending.request_id != approval {
                    return CommandResponse::error(
                        "REMOTE_APPROVAL_UNKNOWN",
                        "that approval is not the one this delegation is waiting on",
                    );
                }
                if pending.risk != "low" {
                    return CommandResponse::error(
                        "HUMAN_REQUIRED",
                        "a high-risk approval must be answered on the remote computer",
                    );
                }
                let connection = match self.account.connect(&record.peer, "dsh").await {
                    Ok(connection) => connection,
                    Err(error) => return CommandResponse::error(error.code, error.message),
                };
                let credentials = agi_peer::Credentials {
                    base: connection.base,
                    room: connection.room,
                    account_session: connection.account_session,
                    device_id: connection.device_id,
                    account_id: connection.account_id,
                    peer_token: connection.token,
                    app_version: self.version.clone(),
                };
                let client = match agi_peer::PeerClient::connect(credentials).await {
                    Ok(client) => client,
                    Err(error) => {
                        return CommandResponse::error("REMOTE_FAILED", error.to_string())
                    }
                };
                if let Err(error) = client.answer_approval(&approval, &outcome).await {
                    return CommandResponse::error("REMOTE_FAILED", error.to_string());
                }
                self.remote.clear_approval(&id, &outcome);
                CommandResponse::ok(json!({ "accepted": true }))
            }
            Command::RemoteSend { id, message } => {
                if message.trim().is_empty() {
                    return CommandResponse::error("MALFORMED_FRAME", "the follow-up is empty");
                }
                let Some(record) = self.remote.get(&id) else {
                    return CommandResponse::error("REMOTE_UNKNOWN", "no such delegation");
                };
                let Some(session) = record.session_id.clone() else {
                    return CommandResponse::error(
                        "REMOTE_NO_SESSION",
                        "that delegation has no remote session yet",
                    );
                };
                let reference = remote::PeerRef {
                    device_id: record.peer.clone(),
                    name: record.peer_name.clone(),
                };
                let cwd = record.cwd.clone();
                let started = if record.route == "lan" {
                    let Some(address) = record.address else {
                        return CommandResponse::error(
                            "REMOTE_NO_SESSION",
                            "that LAN delegation did not record its address",
                        );
                    };
                    let credentials = match self.lan_credentials().await {
                        Ok(credentials) => credentials,
                        Err(error) => return CommandResponse::error("LAN_UNAVAILABLE", error),
                    };
                    self.remote.start_lan_with_session(
                        reference,
                        address,
                        credentials,
                        cwd,
                        message,
                        session,
                    )
                } else {
                    let desktops = match self.account.desktops().await {
                        Ok(desktops) => desktops,
                        Err(error) => return CommandResponse::error(error.code, error.message),
                    };
                    let Some(target) = desktops
                        .into_iter()
                        .find(|desktop| desktop.device_id == record.peer)
                    else {
                        return CommandResponse::error(
                            "REMOTE_PEER_UNKNOWN",
                            "that computer is not on this account",
                        );
                    };
                    if !target.online {
                        return CommandResponse::error(
                            "REMOTE_PEER_OFFLINE",
                            "that computer is not connected to the relay",
                        );
                    }
                    let connection = match self.account.connect(&record.peer, "dsh").await {
                        Ok(connection) => connection,
                        Err(error) => return CommandResponse::error(error.code, error.message),
                    };
                    let credentials = agi_peer::Credentials {
                        base: connection.base,
                        room: connection.room,
                        account_session: connection.account_session,
                        device_id: connection.device_id,
                        account_id: connection.account_id,
                        peer_token: connection.token,
                        app_version: self.version.clone(),
                    };
                    self.remote
                        .start_with_session(reference, credentials, cwd, message, session)
                };
                match started {
                    Ok(next) => CommandResponse::ok(json!({
                        "delegation": next,
                        "continuedFrom": id,
                    })),
                    Err(message) => CommandResponse::error("REMOTE_START_FAILED", message),
                }
            }
            Command::AccountLogout => match self.account.logout().await {
                Ok(()) => CommandResponse::done(),
                Err(error) => CommandResponse::error(error.code, error.message),
            },
            Command::SetTheme { theme } => {
                if !valid_theme(&theme) {
                    return CommandResponse::error("MALFORMED_FRAME", "unknown theme");
                }
                self.set_theme(&theme);
                CommandResponse::done()
            }
            Command::Reconnect { engine: id } => {
                let Some(engine) = self.engine(id) else {
                    return CommandResponse::error("UNKNOWN_ENGINE", "no such engine");
                };
                match engine.reconnect().await {
                    Ok(()) => CommandResponse::done(),
                    Err(error) => failure(error),
                }
            }
            Command::UsageOverview => self.usage_overview().await,
            Command::Activity { session, limit } => {
                let Some(engine) = self.engine(session.engine) else {
                    return CommandResponse::error("UNKNOWN_ENGINE", "no such engine");
                };
                match engine.activity(&session, limit.unwrap_or(120)).await {
                    Ok(entries) => CommandResponse::ok(entries),
                    Err(error) => failure(error),
                }
            }
            Command::Stats { session } => {
                let Some(engine) = self.engine(session.engine) else {
                    return CommandResponse::error("UNKNOWN_ENGINE", "no such engine");
                };
                match engine.stats(&session).await {
                    Ok(stats) => CommandResponse::ok(stats),
                    Err(error) => failure(error),
                }
            }
            Command::Deliverables { session } => {
                let Some(engine) = self.engine(session.engine) else {
                    return CommandResponse::error("UNKNOWN_ENGINE", "no such engine");
                };
                match engine.deliverables(&session).await {
                    Ok(files) => CommandResponse::ok(files),
                    Err(error) => failure(error),
                }
            }
            Command::Models { engine: id } => {
                let Some(engine) = self.engine(id) else {
                    return CommandResponse::error("UNKNOWN_ENGINE", "no such engine");
                };
                match engine.models().await {
                    Ok(models) => CommandResponse::ok(models),
                    Err(error) => failure(error),
                }
            }
            Command::Permissions { engine: id } => {
                let Some(engine) = self.engine(id) else {
                    return CommandResponse::error("UNKNOWN_ENGINE", "no such engine");
                };
                match engine.permissions().await {
                    Ok(presets) => CommandResponse::ok(presets),
                    Err(error) => failure(error),
                }
            }
            Command::Create { engine: id, cwd } => {
                let Some(engine) = self.engine(id) else {
                    return CommandResponse::error("UNKNOWN_ENGINE", "no such engine");
                };
                match engine.create(cwd.as_deref()).await {
                    Ok(session) => CommandResponse::ok(session),
                    Err(error) => failure(error),
                }
            }
            Command::Rename { session, title } => {
                let Some(engine) = self.engine(session.engine) else {
                    return CommandResponse::error("UNKNOWN_ENGINE", "no such engine");
                };
                match engine.rename(&session, &title).await {
                    Ok(()) => CommandResponse::done(),
                    Err(error) => failure(error),
                }
            }
            Command::Archive {
                session,
                stop_activity,
            } => {
                let Some(engine) = self.engine(session.engine) else {
                    return CommandResponse::error("UNKNOWN_ENGINE", "no such engine");
                };
                match engine
                    .archive(&session, stop_activity.unwrap_or(false))
                    .await
                {
                    Ok(()) => CommandResponse::done(),
                    Err(error) => failure(error),
                }
            }
            Command::Workspaces { engine: id } => {
                let Some(engine) = self.engine(id) else {
                    return CommandResponse::error("UNKNOWN_ENGINE", "no such engine");
                };
                match engine.workspaces().await {
                    Ok(workspaces) => CommandResponse::ok(workspaces),
                    Err(error) => failure(error),
                }
            }
            Command::Balance { engine: id } => {
                let Some(engine) = self.engine(id) else {
                    return CommandResponse::error("UNKNOWN_ENGINE", "no such engine");
                };
                match engine.balance().await {
                    Ok(balance) => CommandResponse::ok(balance),
                    Err(error) => failure(error),
                }
            }
            Command::ReadFile { session, path } => {
                let Some(engine) = self.engine(session.engine) else {
                    return CommandResponse::error("UNKNOWN_ENGINE", "no such engine");
                };
                match engine.read_file(&session, &path).await {
                    Ok(preview) => CommandResponse::ok(preview),
                    Err(error) => failure(error),
                }
            }
            Command::Pairing { refresh } => {
                let source = self
                    .pairing
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                match source {
                    Some(source) => CommandResponse::ok(source.pairing_view(refresh)),
                    None => CommandResponse::error(
                        "NO_PAIRING",
                        "this build has no phone server to pair with",
                    ),
                }
            }
            Command::ModelState => CommandResponse::ok(self.model_service.state()),
            Command::ModelCheck => CommandResponse::ok(self.model_service.check().await),
            Command::ModelDownload { version } => {
                match self.model_service.start_download(Some(version)) {
                    Ok(started) => CommandResponse::ok(started),
                    Err(error) => CommandResponse::error("MODEL_DOWNLOAD_REFUSED", error),
                }
            }
            Command::ModelCancel => CommandResponse::ok(self.model_service.cancel()),
            Command::ModelActivate { id } => match self.activate_model(&id) {
                Ok(manifest) => CommandResponse::ok(manifest.id),
                Err(error) => CommandResponse::error("MODEL_ACTIVATE_FAILED", error),
            },
            Command::ModelRemove { version } => match self.model_service.remove(&version) {
                Ok(()) => CommandResponse::done(),
                Err(error) => CommandResponse::error("MODEL_REMOVE_FAILED", error),
            },
            Command::ModelRollback => match self.model_service.rollback() {
                Ok(manifest) => CommandResponse::ok(manifest.version),
                Err(error) => CommandResponse::error("MODEL_ROLLBACK_FAILED", error),
            },
            Command::EngineRescan => CommandResponse::ok(self.rescan_engines()),
            Command::EngineConfigure { engine, enabled } => {
                // The dependency first, the flag second: a flag that says
                // "configured" while the hooks failed turns the agent page's
                // one-click fix into a lie.
                if enabled && engine == EngineId::Claude {
                    if let Err(error) = self.install_claude_hooks() {
                        tracing::warn!("hooks: {error}");
                        return CommandResponse::error("HOOKS_NOT_INSTALLED", error);
                    }
                }
                self.set_engine_configured(engine, enabled);
                CommandResponse::ok(self.engine_configured(engine))
            }
            Command::Dependencies { engine } => {
                CommandResponse::ok(dependencies::check(self, engine))
            }
            Command::DependenciesRepair { engine } => {
                CommandResponse::ok(dependencies::repair(self, engine).await)
            }
            Command::DshPluginCheck => CommandResponse::ok(self.refresh_dsh_plugin(true).await),
            Command::DshPluginUpdate => match self.install_dsh_plugin(None).await {
                Ok(view) => CommandResponse::ok(view),
                Err(error) => CommandResponse::error("DSH_PLUGIN_UPDATE_FAILED", error),
            },
            Command::OnboardingComplete => CommandResponse::ok(self.set_onboarding_done(true)),
            Command::OnboardingReset => CommandResponse::ok(self.set_onboarding_done(false)),
            Command::DatasetShare { enabled } => {
                CommandResponse::ok(dataset::set_enabled(self, enabled))
            }
            Command::DatasetExclude { projects } => {
                CommandResponse::ok(dataset::set_excluded(self, projects))
            }
            Command::Ping => CommandResponse::done(),
            Command::Messages {
                session,
                limit,
                before,
            } => {
                let engine = match self.engine(session.engine) {
                    Some(engine) => engine,
                    None => return CommandResponse::error("UNKNOWN_ENGINE", "no such engine"),
                };
                match engine.records(&session, limit.unwrap_or(200), before).await {
                    Ok(records) => CommandResponse::ok(records),
                    Err(error) => failure(error),
                }
            }
            Command::Prompt { session, text } => {
                let engine = match self.engine(session.engine) {
                    Some(engine) => engine,
                    None => return CommandResponse::error("UNKNOWN_ENGINE", "no such engine"),
                };
                match engine.prompt(&session, &text).await {
                    Ok(()) => CommandResponse::done(),
                    Err(error) => failure(error),
                }
            }
            Command::Steer { session, text } => {
                let engine = match self.engine(session.engine) {
                    Some(engine) => engine,
                    None => return CommandResponse::error("UNKNOWN_ENGINE", "no such engine"),
                };
                match engine.steer(&session, &text).await {
                    Ok(()) => CommandResponse::done(),
                    Err(error) => failure(error),
                }
            }
            Command::Fork { session, at_seq } => {
                let engine = match self.engine(session.engine) {
                    Some(engine) => engine,
                    None => return CommandResponse::error("UNKNOWN_ENGINE", "no such engine"),
                };
                match engine.fork(&session, at_seq).await {
                    Ok(key) => CommandResponse::ok(key),
                    Err(error) => failure(error),
                }
            }
            Command::Cancel { session } => {
                let engine = match self.engine(session.engine) {
                    Some(engine) => engine,
                    None => return CommandResponse::error("UNKNOWN_ENGINE", "no such engine"),
                };
                match engine.cancel(&session).await {
                    Ok(()) => CommandResponse::done(),
                    Err(error) => failure(error),
                }
            }
            Command::Compact { session } => {
                let engine = match self.engine(session.engine) {
                    Some(engine) => engine,
                    None => return CommandResponse::error("UNKNOWN_ENGINE", "no such engine"),
                };
                match engine.compact(&session).await {
                    Ok(()) => CommandResponse::done(),
                    Err(error) => failure(error),
                }
            }
            Command::Answer {
                id,
                outcome,
                answers,
            } => {
                let engine_id = self.store_read().interaction_engine.get(&id).copied();
                let Some(engine_id) = engine_id else {
                    return CommandResponse::error(
                        "UNKNOWN_INTERACTION",
                        "nothing is waiting on that id",
                    );
                };
                let Some(engine) = self.engine(engine_id) else {
                    return CommandResponse::error("UNKNOWN_ENGINE", "no such engine");
                };
                let answer = agi_engine::Answer { outcome, answers };
                match engine.answer(&id, answer).await {
                    Ok(()) => CommandResponse::done(),
                    Err(error) => failure(error),
                }
            }
            Command::SetModel { session, model } => {
                let engine = match self.engine(session.engine) {
                    Some(engine) => engine,
                    None => return CommandResponse::error("UNKNOWN_ENGINE", "no such engine"),
                };
                match engine.set_model(&session, &model).await {
                    Ok(()) => CommandResponse::done(),
                    Err(error) => failure(error),
                }
            }
            Command::SetPermission { session, preset } => {
                let engine = match self.engine(session.engine) {
                    Some(engine) => engine,
                    None => return CommandResponse::error("UNKNOWN_ENGINE", "no such engine"),
                };
                match engine.set_permission(&session, &preset).await {
                    Ok(()) => CommandResponse::done(),
                    Err(error) => failure(error),
                }
            }
        }
    }
}

/// Something that can issue a pairing code.
///
/// A trait rather than a direct call because the phone server depends on the
/// kernel, not the other way round: the kernel asks whoever is composed, and a
/// build without a phone server simply has nobody to ask.
/// The paired phones, for anything outside the window that lists them.
pub trait DeviceStatusSource: Send + Sync {
    /// Every phone that may connect, with whether it is connected now.
    fn device_views(&self) -> Vec<agi_protocol::api::DeviceView>;
}

pub trait PairingSource: Send + Sync {
    /// The code to show, refreshed when asked.
    fn pairing_view(&self, refresh: bool) -> agi_protocol::api::PairingView;
}

/// What the window needs to know about the tunnel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayStatus {
    pub connected: bool,
    /// The room the relay assigned, once it has.
    pub room: Option<String>,
}

/// A tunnel that can say whether it is up.
pub trait RelayStatusSource: Send + Sync {
    fn relay_status(&self) -> RelayStatus;
}

/// Where the account service lives: the official relay unless a launch says
/// otherwise.
fn account_base() -> String {
    std::env::var("AGI_ACCOUNT_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| account::OFFICIAL_ACCOUNT_URL.to_string())
}

/// Turn an engine failure into the API's error shape.
fn failure(error: EngineError) -> CommandResponse {
    CommandResponse::error(error.code(), error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agi_engine::Answer;
    use agi_protocol::api::{ModelUsage, UsageDay};
    use agi_protocol::engine::Capabilities;
    use async_trait::async_trait;

    /// An adapter that answers from memory, so the kernel's own logic is what
    /// these tests exercise.
    struct Fake {
        id: EngineId,
        sessions: Vec<Session>,
        /// `None` means this adapter cannot say — what a machine without the
        /// product installed does.
        usage: Option<Vec<UsageDay>>,
        events: broadcast::Sender<EngineEvent>,
    }

    impl Fake {
        fn new(id: EngineId, sessions: Vec<Session>, usage: Option<Vec<UsageDay>>) -> Arc<Self> {
            let (events, _) = broadcast::channel(8);
            Arc::new(Self {
                id,
                sessions,
                usage,
                events,
            })
        }
    }

    #[test]
    fn a_launch_with_nobody_listening_holds_no_judge() {
        let core = Core::new(Vec::new());
        assert!(
            !core.has_judge(),
            "the judge is bought by a phone socket, not by a launch"
        );
        assert_eq!(core.phones_attached(), 0);
    }

    #[test]
    fn the_phone_count_is_a_count_and_never_goes_negative() {
        let core = Core::new(Vec::new());
        core.phone_attached();
        core.phone_attached();
        assert_eq!(core.phones_attached(), 2, "two sockets are two leases");
        core.phone_detached();
        assert_eq!(core.phones_attached(), 1);
        core.phone_detached();
        core.phone_detached();
        assert_eq!(core.phones_attached(), 0, "an extra detach is not -1");
    }

    #[tokio::test]
    async fn an_idle_desktop_lets_the_judge_go() {
        let core = Core::new(Vec::new());
        core.install_test_judge();
        // A phone that is still attached is never idle, whatever the clock says.
        core.phone_attached();
        assert!(!core.release_judge_if_idle(Duration::ZERO).await);

        core.phone_detached();
        assert!(
            core.release_judge_if_idle(Duration::ZERO).await,
            "once nothing is attached the model is the desktop's to give back"
        );
        assert!(!core.has_judge());
        assert!(
            !core.release_judge_if_idle(Duration::ZERO).await,
            "releasing nothing twice must not kill a sidecar somebody restarted"
        );
    }

    #[tokio::test]
    async fn a_running_turn_keeps_the_judge_while_the_phone_is_asleep() {
        let core = Core::new(Vec::new());
        core.install_test_judge();
        {
            let mut store = core.store_write();
            let mut session = Session::bare(SessionKey::new(EngineId::Dsh, "s-1"), 0);
            session.running = true;
            store.sessions.insert(session.key.clone(), session);
        }

        // No socket is open -- the phone is in a pocket -- and the turn is still
        // running. The verdict is what takes the row off the island, so the judge
        // stays even though nobody is listening right now.
        assert_eq!(core.phones_attached(), 0);
        assert!(core.any_running());
        assert!(!core.release_judge_if_idle(Duration::ZERO).await);
        assert!(core.has_judge());

        // Once the turn is over the same call lets it go.
        {
            let mut store = core.store_write();
            for session in store.sessions.values_mut() {
                session.running = false;
            }
        }
        assert!(core.release_judge_if_idle(Duration::ZERO).await);
        assert!(!core.has_judge());
    }

    #[tokio::test]
    async fn a_phone_that_comes_back_installs_the_judge_again() {
        let core = Core::new(Vec::new());
        core.install_test_judge();
        core.phone_detached();
        assert!(core.release_judge_if_idle(Duration::ZERO).await);
        assert!(!core.has_judge());

        // The manifest is what a phone-less desktop kept; on a machine with no
        // model installed there is nothing to rebuild, and that is not a failure.
        core.phone_attached();
        assert_eq!(core.phones_attached(), 1);
    }

    #[test]
    fn a_turn_that_streamed_can_be_read_back_for_replay() {
        let core = Core::new(Vec::new());
        let key = SessionKey::new(EngineId::Dsh, "s-1");
        assert!(
            core.stream_of(&key).is_none(),
            "a session that never ran has nothing to replay, and that is not an error"
        );

        core.advance(&key, &record(1, 4, RecordKind::TurnStart, None));
        core.advance(
            &key,
            &record(2, 4, RecordKind::AssistantMessage, Some("我先看一下…")),
        );

        let replay = core.stream_of(&key).expect("the turn is on record");
        assert_eq!(replay.turn, Some(4));
        assert_eq!(replay.state, SessionState::StreamNarration);
        assert_eq!(replay.interval, 2);
        assert_eq!(replay.text_seq, "我先看一下…".len() as u32);
        assert!(!replay.truncated);
        assert_eq!(replay.entries.len(), 2);
        assert_eq!(replay.entries[0].state, SessionState::Thinking);
        assert_eq!(replay.entries[1].state, SessionState::StreamNarration);
        assert_eq!(replay.entries[1].text, "我先看一下…");

        // A new turn starts the replay over instead of appending to it.
        core.advance(&key, &record(3, 5, RecordKind::TurnStart, None));
        let next = core.stream_of(&key).unwrap();
        assert_eq!(next.turn, Some(5));
        assert_eq!(next.entries.len(), 1, "{:?}", next.entries);
        assert_eq!(next.entries[0].state, SessionState::Thinking);
    }

    #[test]
    fn the_lamp_needs_enabled_plus_a_live_turn_without_a_standing_socket() {
        let core = Core::new(vec![
            Fake::new(EngineId::Codex, Vec::new(), None) as Arc<dyn Engine>
        ]);
        // Not enabled here: even a connected adapter must not light up.
        let idle = core.status_of(EngineId::Codex).unwrap();
        assert!(idle.connected, "the fake adapter can take a prompt");
        assert!(!idle.engaged, "but the operator never switched it on");

        core.set_engine_configured(EngineId::Codex, true);
        let idle = core.status_of(EngineId::Codex).unwrap();
        assert!(idle.connected);
        assert!(!idle.engaged, "connected is not the same as a live turn");

        {
            let mut store = core.store_write();
            let mut session = Session::bare(SessionKey::new(EngineId::Codex, "c-1"), 0);
            session.running = true;
            store.sessions.insert(session.key.clone(), session);
        }
        let live = core.status_of(EngineId::Codex).unwrap();
        assert!(
            live.engaged,
            "a running turn is the evidence the lamp wants"
        );

        // DSH is the exception: its plugin is the standing link, so the lamp is
        // on while attached even when no turn is running.
        let dsh = Core::new(vec![
            Fake::new(EngineId::Dsh, Vec::new(), None) as Arc<dyn Engine>
        ]);
        dsh.set_engine_configured(EngineId::Dsh, true);
        let attached = dsh.status_of(EngineId::Dsh).unwrap();
        assert!(attached.connected);
        assert!(attached.engaged);
    }

    #[tokio::test]
    async fn a_healthy_codex_link_is_not_reconnected_by_the_dependency_button() {
        let core = Core::new(vec![
            Fake::new(EngineId::Codex, Vec::new(), None) as Arc<dyn Engine>
        ]);
        let report = crate::dependencies::repair(&core, EngineId::Codex).await;
        assert!(report.ok, "{:?}", report.items);
        assert!(report.changed.is_empty(), "{:?}", report.changed);
        assert!(report.error.is_none(), "{:?}", report.error);
    }

    #[test]
    fn the_packaged_sidecar_is_found_below_resources_bin() {
        let dir = tempfile::tempdir().unwrap();
        let macos = dir.path().join("Contents/MacOS");
        let resources = dir.path().join("Contents/Resources/bin");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::create_dir_all(&resources).unwrap();
        let exe = macos.join("AGI Mizar");
        std::fs::write(&exe, b"app").unwrap();
        let sidecar = resources.join("agi-remote");
        std::fs::write(&sidecar, b"sidecar").unwrap();
        // The same resolver the report and the YAML entry spell the path with:
        // `dunce` on Windows, so no `\\?\` prefix reaches either surface.
        assert_eq!(
            find_remote_installer(&exe),
            Some(agi_platform::canonicalize(&sidecar).unwrap())
        );
    }

    #[test]
    fn remote_prefs_are_off_by_default_and_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        let core = Core::new(Vec::new());
        core.load_preferences(path.clone());
        assert!(
            !core.remote_prefs().enabled,
            "a new install accepts no peers"
        );
        core.set_remote_prefs(RemotePrefs {
            enabled: true,
            allow_lan: true,
            allow_wan: false,
            allow_all_workspaces: false,
            workspaces: vec!["/tmp/remote".to_string()],
            blocked: Vec::new(),
        });
        assert!(core.remote_workspace_allowed("/tmp/remote/"));
        assert!(!core.remote_workspace_allowed("/tmp/other"));
        core.set_remote_prefs(RemotePrefs {
            enabled: true,
            allow_lan: true,
            allow_wan: false,
            allow_all_workspaces: true,
            workspaces: Vec::new(),
            blocked: Vec::new(),
        });
        assert!(core.remote_workspace_allowed("/tmp/anything"));
        assert!(core.remote_workspace_allowed("C:\\Users\\anywhere"));
        // The same directory, spelled the way another platform would: Windows
        // separators, a trailing separator, and (on Windows) different case.
        if cfg!(windows) {
            assert!(core.remote_workspace_allowed("\\tmp\\remote\\"));
            assert!(core.remote_workspace_allowed("/TMP/REMOTE"));
        } else {
            assert!(core.remote_workspace_allowed("/tmp/remote"));
        }

        let again = Core::new(Vec::new());
        again.load_preferences(path);
        assert_eq!(again.remote_prefs(), core.remote_prefs());
    }

    #[test]
    fn the_remote_installer_is_found_in_the_bundle_layout() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join(if cfg!(windows) {
            "AGI Mizar.exe"
        } else {
            "AGI Mizar"
        });
        std::fs::write(&app, b"app").unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let remote = bin.join(if cfg!(windows) {
            "agi-remote.exe"
        } else {
            "agi-remote"
        });
        std::fs::write(&remote, b"remote").unwrap();
        let found = find_remote_installer(&app).expect("the sidecar under bin/ is found");
        assert_eq!(found.file_name(), remote.file_name());
    }

    #[test]
    fn the_theme_is_remembered_between_launches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        let core = Core::new(Vec::new());
        core.load_preferences(path.clone());
        assert_eq!(core.theme(), "system");

        core.set_theme("dark");
        assert_eq!(core.theme(), "dark");

        // A second launch reads what the first one wrote.
        let again = Core::new(Vec::new());
        again.load_preferences(path);
        assert_eq!(again.theme(), "dark");

        // A name this window cannot paint is refused rather than stored.
        core.set_theme("neon");
        assert_eq!(core.theme(), "dark");
    }

    #[async_trait]
    impl Engine for Fake {
        fn id(&self) -> EngineId {
            self.id
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                attachments: false,
                file_preview: false,
                fork: false,
                archive: false,
                activity: false,
                balance: false,
                models: false,
                permissions: false,
            }
        }

        fn status(&self) -> (bool, Option<String>) {
            (true, None)
        }

        async fn sessions(&self) -> Result<Vec<Session>, EngineError> {
            Ok(self.sessions.clone())
        }

        async fn records(
            &self,
            _session: &SessionKey,
            _limit: usize,
            _before: Option<i64>,
        ) -> Result<Vec<Record>, EngineError> {
            Ok(Vec::new())
        }

        async fn prompt(&self, _session: &SessionKey, _text: &str) -> Result<(), EngineError> {
            Ok(())
        }

        async fn cancel(&self, _session: &SessionKey) -> Result<(), EngineError> {
            Ok(())
        }

        async fn answer(&self, _id: &str, _answer: Answer) -> Result<(), EngineError> {
            Ok(())
        }

        async fn set_model(&self, _session: &SessionKey, _model: &str) -> Result<(), EngineError> {
            Ok(())
        }

        async fn set_permission(
            &self,
            _session: &SessionKey,
            _preset: &str,
        ) -> Result<(), EngineError> {
            Ok(())
        }

        async fn usage_history(&self) -> Result<Vec<UsageDay>, EngineError> {
            match &self.usage {
                Some(days) => Ok(days.clone()),
                None => Err(self.unsupported("USAGE_UNSUPPORTED")),
            }
        }

        fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
            self.events.subscribe()
        }
    }

    fn session(engine: EngineId, id: &str, origin: Option<&str>) -> Session {
        Session {
            origin: origin.map(str::to_string),
            ..Session::bare(SessionKey::new(engine, id), 0)
        }
    }

    /// One record of a turn, as an engine would hand it to the kernel.
    fn record(seq: i64, turn: u32, kind: RecordKind, text: Option<&str>) -> Record {
        Record {
            seq,
            time: seq * 1_000,
            turn: Some(turn),
            step: None,
            kind,
            text: text.map(str::to_string),
            tool: None,
        }
    }

    fn day(date: &str, messages: u64, input: u64, hour: usize, model: &str) -> UsageDay {
        let mut hours = [0u64; 24];
        hours[hour] = messages;
        UsageDay {
            date: date.to_string(),
            messages,
            input_tokens: input,
            output_tokens: input / 10,
            hours,
            models: vec![ModelUsage {
                model: model.to_string(),
                messages,
                input_tokens: input,
                output_tokens: input / 10,
            }],
        }
    }

    #[tokio::test]
    async fn the_overview_adds_three_engines_up_by_date() {
        let dsh = Fake::new(
            EngineId::Dsh,
            vec![
                session(EngineId::Dsh, "s1", None),
                session(EngineId::Dsh, "child", Some("subagent")),
            ],
            Some(vec![day("2026-10-20", 2, 0, 9, "dsh-model")]),
        );
        let claude = Fake::new(
            EngineId::Claude,
            vec![session(EngineId::Claude, "c1", None)],
            Some(vec![
                day("2026-10-20", 3, 500, 9, "claude-model"),
                day("2026-10-19", 1, 100, 14, "claude-model"),
            ]),
        );
        // A machine without Codex: the adapter says so instead of answering.
        let codex = Fake::new(
            EngineId::Codex,
            vec![session(EngineId::Codex, "x1", None)],
            None,
        );

        let core = Core::new(vec![
            Arc::clone(&dsh) as Arc<dyn Engine>,
            Arc::clone(&claude) as Arc<dyn Engine>,
            Arc::clone(&codex) as Arc<dyn Engine>,
        ]);
        core.start().await;

        let response = core.dispatch(Command::UsageOverview).await;
        assert!(response.ok, "{:?}", response.error);
        let wire = response.value.unwrap();

        // Top level only: the subagent is not a session the operator started,
        // but the Codex session still counts even though Codex has no history.
        assert_eq!(wire["sessions"], 3);
        let days = wire["days"].as_array().unwrap();
        assert_eq!(days.len(), 2);
        assert_eq!(days[0]["date"], "2026-10-19");
        assert_eq!(days[1]["date"], "2026-10-20");
        // The date both engines worked on is one row, added up.
        assert_eq!(days[1]["messages"], 5);
        assert_eq!(days[1]["inputTokens"], 500);
        assert_eq!(days[1]["outputTokens"], 50);
        let hours = days[1]["hours"].as_array().unwrap();
        assert_eq!(hours.len(), 24);
        assert_eq!(hours[9], 5);
        // Both models survive, most-used first.
        let models = days[1]["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["model"], "claude-model");
        assert_eq!(models[1]["model"], "dsh-model");
    }

    #[tokio::test]
    async fn an_engine_that_cannot_answer_costs_nobody_their_charts() {
        let nothing = Fake::new(EngineId::Dsh, Vec::new(), None);
        let core = Core::new(vec![Arc::clone(&nothing) as Arc<dyn Engine>]);
        core.start().await;

        let response = core.dispatch(Command::UsageOverview).await;
        assert!(response.ok, "{:?}", response.error);
        let wire = response.value.unwrap();
        assert_eq!(wire["sessions"], 0);
        assert_eq!(wire["days"], serde_json::json!([]));
    }

    #[test]
    fn the_judge_fallback_reads_the_latest_finished_turn() {
        let records = vec![
            record(1, 1, RecordKind::TurnStart, None),
            record(2, 1, RecordKind::UserMessage, Some("first")),
            record(3, 1, RecordKind::TurnEnd, Some("success")),
            record(4, 2, RecordKind::TurnStart, None),
            record(5, 2, RecordKind::UserMessage, Some("second")),
            record(6, 2, RecordKind::AssistantMessage, Some("done")),
            record(7, 2, RecordKind::TurnEnd, Some("success")),
            // A third turn is still open: the phone is watching it run.
            record(8, 3, RecordKind::TurnStart, None),
            record(9, 3, RecordKind::UserMessage, Some("third")),
        ];

        let (turn, slice) = last_finished_turn(&records).expect("a finished turn");
        assert_eq!(turn, 2);
        // The whole second turn, not just its tail and not the first turn.
        assert_eq!(slice.first().map(|r| r.seq), Some(4));
        assert_eq!(slice.last().map(|r| r.seq), Some(7));
        assert_eq!(slice.len(), 4);
    }

    #[test]
    fn the_judge_fallback_waits_while_the_only_turn_is_open() {
        let records = vec![
            record(1, 1, RecordKind::TurnStart, None),
            record(2, 1, RecordKind::UserMessage, Some("running")),
        ];

        assert!(last_finished_turn(&records).is_none());
        assert!(last_finished_turn(&[]).is_none());
    }
}

// ---------------------------------------------------------------- layout glue

/// The judge this launch should use, if any.
///
/// Order: an activated manifest (`MIZAR_MODEL`, or the only manifest present),
/// then `MIZAR_LAYOUT_URL` as a one-off escape hatch for development. Nothing
/// here knows what "Mizarllm" is — the manifest does.
///
/// Returns the manifest only. The service itself is built by `install_judge`,
/// which runs when a phone attaches: a launch nobody is listening to does not
/// get a model.
fn layout_from(service: &Arc<ModelService>) -> Option<ModelManifest> {
    // An explicit URL still wins: it is how a sidecar started by hand, or a
    // remote one, gets used without being installed through the app.
    if let Some(manifest) = env_manifest() {
        tracing::info!(model = %manifest.id, version = %manifest.version, "layout: MIZAR_LAYOUT_URL");
        return Some(manifest);
    }

    let manifest = service.manifest()?;
    tracing::info!(
        model = %manifest.id,
        version = %manifest.version,
        "layout: model installed, started when a phone attaches"
    );
    Some(manifest)
}

/// A manifest built from `MIZAR_LAYOUT_URL`, for a sidecar started by hand.
fn env_manifest() -> Option<ModelManifest> {
    let endpoint = std::env::var("MIZAR_LAYOUT_URL").ok()?;
    let key = std::env::var("MIZAR_LAYOUT_KEY").is_ok();
    Some(ModelManifest {
        id: "env".to_string(),
        version: "0".to_string(),
        backend: Backend::Http {
            endpoint,
            timeout_ms: 1500,
            api_key_env: key.then(|| "MIZAR_LAYOUT_KEY".to_string()),
        },
        labels: LabelMap {
            done: SessionState::Completed.code(),
            needs_user: SessionState::AwaitingUser.code(),
            stalled: SessionState::Failed.code(),
            waiting: Some(SessionState::Background.code()),
        },
        note: "from MIZAR_LAYOUT_URL".to_string(),
    })
}

/// The records belonging to one turn: everything since the previous `TurnEnd`.
fn turn_records(bucket: &VecDeque<Record>, turn: Option<u32>) -> Vec<Record> {
    let mut out = Vec::new();
    for record in bucket.iter().rev() {
        if record.kind == RecordKind::TurnEnd && !out.is_empty() {
            break;
        }
        if let (Some(wanted), Some(actual)) = (turn, record.turn) {
            if wanted != actual {
                break;
            }
        }
        out.push(record.clone());
    }
    out.reverse();
    out
}

/// The newest turn in `records` that actually finished, with the records of that
/// turn.
///
/// [`Core::judge_finished_turn`] asks an engine for its newest records when a
/// session stops running; a turn that is still open (a later `turn/start` with no
/// `turn/end`) must not be judged from the previous turn's text, and the slice
/// has to start at the matching `turn/start` or the judge sees only the tail.
fn last_finished_turn(records: &[Record]) -> Option<(u32, Vec<Record>)> {
    let end = records
        .iter()
        .rposition(|record| record.kind == RecordKind::TurnEnd)?;
    let turn = records[end].turn?;
    let start = records[..end]
        .iter()
        .rposition(|record| record.kind == RecordKind::TurnStart && record.turn == Some(turn))
        .unwrap_or(0);
    Some((turn, records[start..=end].to_vec()))
}

/// Normalise the engine-specific turn-end text into the vocabulary the model was
/// trained on. Engines disagree here: DSH writes the provider's reason, Codex
/// writes its own status, Claude writes nothing.
fn reason_of(text: Option<&str>) -> &'static str {
    match text.unwrap_or("completed") {
        "error" | "failed" | "failure" => "error",
        "aborted" | "interrupted" | "cancelled" | "canceled" => "aborted",
        _ => "completed",
    }
}

/// `ToolResult` carries no error flag, so a failed call is recognised by its
/// output. Lossy on purpose: shadow mode, and the alternative is dropping the
/// failure signal entirely.
fn looks_failed(output: &str) -> bool {
    let head = output.trim_start();
    ["Error", "error", "failed", "FAILED", "Fatal", "fatal"]
        .iter()
        .any(|p| head.starts_with(p))
        || head.contains("exit code 1")
}

/// Map one turn's records onto the judge's view.
fn facts_for(session: &SessionKey, turn: u32, records: &[Record]) -> agi_layout::Facts {
    let first = records.first().map(|r| r.time).unwrap_or(0);
    let last = records.last().map(|r| r.time).unwrap_or(first);
    let minutes = ((last - first).max(0) as f64) / 60_000.0;

    let items: Vec<Item<'_>> = records
        .iter()
        .filter_map(|r| match r.kind {
            RecordKind::UserMessage => r.text.as_deref().map(Item::User),
            RecordKind::AssistantMessage => r.text.as_deref().map(Item::Assistant),
            RecordKind::ToolCall => Some(Item::Tool(
                r.tool.as_ref().map(|t| t.name.as_str()).unwrap_or("tool"),
            )),
            RecordKind::ToolResult => r
                .tool
                .as_ref()
                .and_then(|t| t.output.as_deref())
                .filter(|o| looks_failed(o))
                .map(Item::ToolFailed),
            RecordKind::Step => Some(Item::Step),
            RecordKind::TurnEnd => Some(Item::TurnEnd(reason_of(r.text.as_deref()))),
            _ => None,
        })
        .collect();

    facts_from_turn(&format!("{}:{}", session.id, turn), minutes, items)
}

// ------------------------------------------------------------- session states

/// Where model manifests live.
///
/// `MIZAR_MODEL_DIR` wins, then `<data dir>/models`. A directory that does not
/// exist is not a problem: without models the kernel falls back to rules.
fn models_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MIZAR_MODEL_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var("MIZAR_DATA_DIR") {
        return PathBuf::from(dir).join("models");
    }
    // Beside the endpoint, the account and the device roster: one directory to
    // back up, one to look in when something is missing. The old default was
    // `models/models` relative to the working directory, which for a Finder
    // launch meant `/models/models` and never found anything.
    agi_platform::data_dir().join("models")
}

/// The live signal a record carries, if any.
fn live_of(record: &Record) -> Option<Live> {
    match record.kind {
        RecordKind::TurnStart => Some(Live::Thinking),
        RecordKind::AssistantMessage if record.text.is_some() => Some(Live::Writing),
        RecordKind::ToolCall | RecordKind::ToolResult => Some(Live::Tool),
        RecordKind::Step => Some(Live::Thinking),
        RecordKind::TurnEnd => None,
        _ => None,
    }
}

/// Renew every held state every few seconds, so the phone can tell "still
/// running" from "the host died" without any other traffic.
///
/// Terminal states are skipped by the machine itself: "failed" stays true after
/// a disconnect, so it needs no renewal.
fn start_heartbeat(
    states: Arc<Mutex<HashMap<SessionKey, StateMachine>>>,
    tx: broadcast::Sender<(SessionKey, Vec<u8>)>,
) {
    // A plain thread, not a tokio task: the kernel builds this outside any
    // runtime (the Tauri main thread), and `Handle::try_current()` there is
    // simply `None`, which silently disabled the ticker. Renewing a state once
    // every five seconds does not need an async runtime.
    std::thread::Builder::new()
        .name("mizar-state-heartbeat".to_string())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(HEARTBEAT_MS));
            let Ok(mut map) = states.lock() else { continue };
            for (session, machine) in map.iter_mut() {
                if let Some(frame) = machine.heartbeat() {
                    let _ = tx.send((session.clone(), frame.encode()));
                }
            }
        })
        .ok();
}
