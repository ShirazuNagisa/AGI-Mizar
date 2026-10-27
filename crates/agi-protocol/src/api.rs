//! The local API between the core and any client of it.
//!
//! The front end is not the only possible client: the desktop UI is one, the
//! `agi-daemon` binary's CLI is another, and a future headless deployment is a
//! third. Keeping this surface engine-neutral and transport-neutral is what lets
//! all three exist without a second model layer.
//!
//! ### Why one command endpoint
//!
//! A REST surface would spread the contract over a dozen paths and make every
//! addition a routing decision. One `POST /api/v1/command` with a tagged enum
//! means the contract is a single Rust type, the generated TypeScript is a single
//! union, and an unknown `op` is a clear error instead of a 404 that might mean
//! "old client".

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{Interaction, Record, Session, SessionKey};
use crate::engine::{Capabilities, EngineId};

/// Everything a freshly opened window needs to draw itself.
#[derive(Debug, Clone, Serialize)]
pub struct StateSnapshot {
    /// The application version, for the about panel and bug reports.
    pub version: String,
    pub engines: Vec<EngineStatus>,
    pub sessions: Vec<Session>,
    pub interactions: Vec<Interaction>,
}

/// One engine's connection state, as the UI's engine switch reads it.
#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    pub engine: EngineId,
    /// Whether prompts can be sent right now.
    pub connected: bool,
    pub capabilities: Capabilities,
    /// A short human-readable fact: the plugin version, the app-server version,
    /// or why the engine is not connected.
    pub detail: Option<String>,
    pub sessions: usize,
}

/// One command from a client.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    /// Everything, again. Cheap: the core already holds it.
    State,
    /// Read a page of a session's records.
    Messages {
        session: SessionKey,
        #[serde(default)]
        limit: Option<usize>,
        /// Return records with a `seq` strictly below this.
        #[serde(default)]
        before: Option<i64>,
    },
    /// Send a prompt to a session.
    Prompt { session: SessionKey, text: String },
    /// Stop the running turn.
    Cancel { session: SessionKey },
    /// Answer a pending approval or question batch.
    Answer {
        id: String,
        #[serde(default)]
        outcome: Option<String>,
        #[serde(default)]
        answers: Option<Vec<Value>>,
    },
    /// Choose a model for a session.
    SetModel { session: SessionKey, model: String },
    /// Choose a permission preset for a session.
    SetPermission { session: SessionKey, preset: String },
    /// What the running turn is doing.
    Activity {
        session: SessionKey,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// What a session has cost.
    Stats { session: SessionKey },
    /// Files the model said it produced.
    Deliverables { session: SessionKey },
    /// Every model an engine offers.
    Models { engine: EngineId },
    /// Every permission preset an engine offers.
    Permissions { engine: EngineId },
    /// Start a session in a directory.
    Create {
        engine: EngineId,
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Give a session a name.
    Rename { session: SessionKey, title: String },
    /// Hide a session without deleting it.
    Archive {
        session: SessionKey,
        #[serde(default)]
        stop_activity: Option<bool>,
    },
    /// The host's own workspace grouping.
    Workspaces { engine: EngineId },
    /// What the provider account is worth.
    Balance { engine: EngineId },
    /// Read a file a session delivered.
    ReadFile { session: SessionKey, path: String },

    /// The pairing code to show a phone, and a QR for its URI.
    Pairing {
        /// Issue a fresh code instead of returning the one already on screen.
        #[serde(default)]
        refresh: bool,
    },
    /// Liveness probe, used by the shell before it shows a window.
    Ping,
}

/// What the window draws to pair a phone.
#[derive(Debug, Clone, Serialize)]
pub struct PairingView {
    /// The code as a person reads it, `K7QP-3M2X`.
    pub code: String,
    /// `mizar://pair?...`, which the QR encodes.
    pub uri: String,
    /// Every address a phone on this network could dial, best first.
    pub addresses: Vec<String>,
    /// An SVG QR of `uri`, drawn by the host so the window needs no library.
    pub svg: Option<String>,
    /// Epoch milliseconds after which the code is dead.
    pub expires_at: i64,
}

/// The answer to one command.
#[derive(Debug, Clone, Serialize)]
pub struct CommandResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

impl CommandResponse {
    /// A successful answer.
    pub fn ok(value: impl Serialize) -> Self {
        Self {
            ok: true,
            value: serde_json::to_value(value).ok(),
            error: None,
        }
    }

    /// An empty success.
    pub fn done() -> Self {
        Self {
            ok: true,
            value: None,
            error: None,
        }
    }

    /// A failure the client can branch on.
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            value: None,
            error: Some(ApiError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

/// A failure at the API boundary.
#[derive(Debug, Clone, Serialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

/// What the server pushes to connected clients.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiEvent {
    /// A session appeared or its summary changed.
    SessionUpdated { session: Session },
    /// A session is gone (archived, deleted, or its engine disconnected).
    SessionRemoved { session: SessionKey },
    /// One new record in a session.
    RecordAppended { session: SessionKey, record: Record },
    /// An agent is waiting on the operator.
    InteractionRequested { interaction: Interaction },
    /// An interaction was answered or withdrawn, wherever it was answered.
    InteractionResolved { id: String },
    /// An engine connected, disconnected, or changed its detail.
    EngineStatus { status: EngineStatus },
    /// Something the operator should see but that is not an error.
    Notice { level: String, message: String },
}
