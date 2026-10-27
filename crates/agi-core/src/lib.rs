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
//! No persistence. The kernel is a live view of what the engines report; the
//! session logs are the engines' own durable record, and a second copy would be a
//! second truth. SQLite arrives with the features that need it (search, settings,
//! the deliverable index), not before.

pub mod endpoint;
pub mod server;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, RwLock};

use agi_engine::{Engine, EngineError, EngineEvent};
use agi_protocol::api::{ApiEvent, Command, CommandResponse, EngineStatus, StateSnapshot};
use agi_protocol::domain::{Interaction, Record, Session, SessionKey};
use agi_protocol::engine::EngineId;
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
pub struct Core {
    engines: Vec<Arc<dyn Engine>>,
    store: RwLock<Store>,
    events: broadcast::Sender<ApiEvent>,
    version: String,
    /// The phone server, when this build has one. Attached after construction
    /// because it needs the kernel it serves.
    pairing: RwLock<Option<Arc<dyn PairingSource>>>,
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
        })
    }

    /// Attach the phone server, so the window can show a pairing code.
    pub fn attach_pairing(&self, source: Arc<dyn PairingSource>) {
        *self
            .pairing
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(source);
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
            engines,
            sessions,
            interactions: store.interactions.values().cloned().collect(),
        }
    }

    /// Run one command.
    pub async fn dispatch(&self, command: Command) -> CommandResponse {
        match command {
            Command::State => CommandResponse::ok(self.snapshot()),
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

/// Turn an engine failure into the API's error shape.
fn failure(error: EngineError) -> CommandResponse {
    CommandResponse::error(error.code(), error.to_string())
}
