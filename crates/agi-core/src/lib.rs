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

pub mod account;
pub mod endpoint;
pub mod server;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use agi_engine::{Engine, EngineError, EngineEvent};
use agi_protocol::api::{
    ApiEvent, Command, CommandResponse, EngineStatus, StateSnapshot, UsageOverview,
};
use agi_protocol::domain::{Interaction, Record, Session, SessionKey};
use agi_protocol::engine::EngineId;
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
}

fn default_theme() -> String {
    "system".to_string()
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: default_theme(),
        }
    }
}

/// Whether a theme name is one this window can paint.
pub fn valid_theme(theme: &str) -> bool {
    matches!(theme, "system" | "light" | "dark")
}

pub struct Core {
    engines: Vec<Arc<dyn Engine>>,
    store: RwLock<Store>,
    events: broadcast::Sender<ApiEvent>,
    version: String,
    /// The phone server, when this build has one. Attached after construction
    /// because it needs the kernel it serves.
    pairing: RwLock<Option<Arc<dyn PairingSource>>>,
    preferences: RwLock<Preferences>,
    /// Where preferences are written, once a launch tells us.
    preferences_path: RwLock<Option<PathBuf>>,
    /// The relay account this window is signed in to.
    account: account::AccountService,
    /// The tunnel's own state, when this build dials a relay.
    relay_status: RwLock<Option<Arc<dyn RelayStatusSource>>>,
}

impl Core {
    /// Build a kernel over the given engines.
    pub fn new(engines: Vec<Arc<dyn Engine>>) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Arc::new(Self {
            engines,
            store: RwLock::new(Store::default()),
            events,
            version: env!("CARGO_PKG_VERSION").to_string(),
            pairing: RwLock::new(None),
            preferences: RwLock::new(Preferences::default()),
            preferences_path: RwLock::new(None),
            account: account::AccountService::new(account_base()),
            relay_status: RwLock::new(None),
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
        let stored = Preferences {
            theme: theme.to_string(),
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(
            &path,
            serde_json::to_vec_pretty(&stored).unwrap_or_default(),
        );
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
    pub fn engine(&self, id: EngineId) -> Option<&Arc<dyn Engine>> {
        self.engines.iter().find(|engine| engine.id() == id)
    }

    /// Start listening to every engine and seed the store from what they know.
    ///
    /// Called once, after construction, because it needs the async runtime. A
    /// failure to list one engine is logged and does not stop the others: a
    /// kernel where Codex is unavailable should still show DSH.
    pub async fn start(self: &Arc<Self>) {
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
    fn on_engine_event(&self, engine: EngineId, event: EngineEvent) {
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
                {
                    let mut store = self.store_write();
                    let bucket = store.records.entry(session.clone()).or_default();
                    bucket.push_back(record.clone());
                    while bucket.len() > MAX_RECORDS {
                        bucket.pop_front();
                    }
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
        let engine = self.engine(id)?;
        let (connected, detail) = engine.status();
        let sessions = self
            .store_read()
            .sessions
            .keys()
            .filter(|key| key.engine == id)
            .count();
        Some(EngineStatus {
            engine: id,
            connected,
            capabilities: engine.capabilities(),
            detail,
            sessions,
        })
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
                Ok(url) => CommandResponse::ok(json!({ "authorizeUrl": url })),
                Err(error) => CommandResponse::error(error.code, error.message),
            },
            Command::AccountGithubExchange { handoff } => {
                match self.account.github_exchange(&handoff).await {
                    Ok(user) => CommandResponse::ok(json!({ "user": user })),
                    Err(error) => CommandResponse::error(error.code, error.message),
                }
            }
            Command::AccountAppleStart => match self.account.apple_start().await {
                Ok(url) => CommandResponse::ok(json!({ "authorizeUrl": url })),
                Err(error) => CommandResponse::error(error.code, error.message),
            },
            Command::AccountAppleExchange { handoff } => {
                match self.account.apple_exchange(&handoff).await {
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
