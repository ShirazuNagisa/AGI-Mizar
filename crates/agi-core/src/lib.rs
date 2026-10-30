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
pub mod endpoint;
pub mod models;
pub mod server;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use agi_engine::{Engine, EngineError, EngineEvent};
use agi_layout::model::{Backend, LabelMap, ModelManifest};
use agi_layout::{facts_from_turn, Item, Layout, LayoutService, Live, StateMachine};
use agi_protocol::api::{
    ApiEvent, Command, CommandResponse, EngineStatus, OnboardingView, StateSnapshot, UsageOverview,
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
        }
    }
}

/// Whether a theme name is one this window can paint.
pub fn valid_theme(theme: &str) -> bool {
    matches!(theme, "system" | "light" | "dark")
}

/// Installs the Claude Code hooks, from whatever knows where the shim is.
type HookInstaller = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Looks for agents again, given the ones already hosted.
type RescanFn =
    Arc<dyn Fn(&[EngineId]) -> (Vec<Arc<dyn Engine>>, HashMap<EngineId, String>) + Send + Sync>;

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
}

impl Core {
    /// Build a kernel over the given engines.
    pub fn new(engines: Vec<Arc<dyn Engine>>) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let (frame_tx, _) = broadcast::channel(EVENT_BUFFER);
        let model_service = ModelService::open(models_dir());
        let layout = layout_from(&model_service);
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
            relay_status: RwLock::new(None),
            layout: RwLock::new(layout.0.clone()),
            layouts: Arc::new(Mutex::new(HashMap::new())),
            model_service: Arc::clone(&model_service),
            manifest: RwLock::new(model_service.manifest()),
            states: Arc::new(Mutex::new(HashMap::new())),
            frames: frame_tx.clone(),
            unavailable: RwLock::new(HashMap::new()),
            late: RwLock::new(HashMap::new()),
            claude_hooks: RwLock::new(None),
            rescan: RwLock::new(None),
            heartbeat_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
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

    /// The account this window is signed in to, when it is.
    pub fn account_user(&self) -> Option<agi_protocol::account::AccountUser> {
        self.account.user()
    }

    /// The session and device id the tunnel presents, when signed in.
    pub fn relay_account(&self) -> Option<(String, String)> {
        self.account.session()
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

            if record.kind == RecordKind::AssistantMessage {
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
        let Some(service) = self.layout.read().ok().and_then(|l| l.clone()) else {
            return;
        };
        let Some(turn) = turn else {
            return;
        };
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
                {
                    let mut store = self.store_write();
                    store.sessions.insert(session.key.clone(), session.clone());
                }
                self.publish(ApiEvent::SessionUpdated { session });
            }
            EngineEvent::SessionRemoved(key) => {
                {
                    let mut store = self.store_write();
                    store.sessions.remove(&key);
                    store.records.remove(&key);
                }
                self.publish(ApiEvent::SessionRemoved { session: key });
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
        let sessions = self
            .store_read()
            .sessions
            .keys()
            .filter(|key| key.engine == id)
            .count();

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
                    capabilities: agi_protocol::engine::Capabilities::default(),
                    detail: Some(reason),
                    sessions,
                })
            }
        }
    }

    /// Everything a freshly opened window needs.
    pub fn snapshot(&self) -> StateSnapshot {
        let store = self.store_read();
        let engines = EngineId::all()
            .into_iter()
            .filter_map(|id| self.status_of(id))
            .collect();

        let mut sessions: Vec<Session> = store.sessions.values().cloned().collect();
        // Newest first, which is the only order a session list is ever read in.
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));

        StateSnapshot {
            version: self.version.clone(),
            theme: self.theme(),
            engines,
            sessions,
            interactions: store.interactions.values().cloned().collect(),
            onboarding: OnboardingView {
                done: self.onboarding_done(),
            },
            model: self.model_service.state(),
            devices: self.device_views(),
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
                    #[cfg(target_os = "macos")]
                    let opened = std::process::Command::new("open").arg(&url).spawn();
                    #[cfg(not(target_os = "macos"))]
                    let opened: Result<std::process::Child, std::io::Error> = Err(
                        std::io::Error::new(std::io::ErrorKind::Unsupported, "no opener"),
                    );
                    match opened {
                        Ok(_) => CommandResponse::ok(json!({ "opened": true })),
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
            Command::OnboardingComplete => CommandResponse::ok(self.set_onboarding_done(true)),
            Command::OnboardingReset => CommandResponse::ok(self.set_onboarding_done(false)),
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
}

// ---------------------------------------------------------------- layout glue

/// The judge this launch should use, if any.
///
/// Order: an activated manifest (`MIZAR_MODEL`, or the only manifest present),
/// then `MIZAR_LAYOUT_URL` as a one-off escape hatch for development. Nothing
/// here knows what "Mizarllm" is — the manifest does.
fn layout_from(service: &Arc<ModelService>) -> (Option<Arc<LayoutService>>, Option<ModelManifest>) {
    // An explicit URL still wins: it is how a sidecar started by hand, or a
    // remote one, gets used without being installed through the app.
    if let Some(manifest) = env_manifest() {
        let judge = manifest.judge();
        tracing::info!(model = %manifest.id, version = %manifest.version, "layout: MIZAR_LAYOUT_URL");
        return (Some(Arc::new(LayoutService::new(judge))), Some(manifest));
    }

    let Some(manifest) = service.manifest() else {
        return (None, None);
    };
    // The judge asks the service for the port, so a sidecar started after this
    // point is still reachable.
    let judge = Arc::new(models::SidecarJudge::new(Arc::clone(service)));
    tracing::info!(model = %manifest.id, version = %manifest.version, "layout: model activated");
    (Some(Arc::new(LayoutService::new(judge))), Some(manifest))
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
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".agi-mizar").join("models")
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
