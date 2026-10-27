//! What an engine adapter has to provide.
//!
//! The trait is deliberately small. Every method here is something a person does
//! to an agent — read its sessions, read a conversation, send a prompt, stop it,
//! answer it, choose a model or a permission level. Anything an engine can do
//! beyond that is reached through the engine's own escape hatch, not through this
//! interface: a trait that grows one method per product feature becomes a union
//! of three products rather than an abstraction over them.
//!
//! ### Events, not polling
//!
//! An adapter pushes [`EngineEvent`]s; the core never asks "what changed". That
//! is what makes the UI feel immediate on three engines with three different
//! update mechanisms (a WebSocket firehose, a JSON-RPC notification stream, a
//! file being appended to).

use agi_protocol::domain::{Interaction, Record, Session, SessionKey};
use agi_protocol::engine::{Capabilities, EngineId};
use async_trait::async_trait;
use serde_json::Value;

/// Something that happened in an engine.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// A session appeared, or its summary changed.
    SessionUpdated(Session),
    /// A session is gone from this engine's list.
    SessionRemoved(SessionKey),
    /// One committed record.
    RecordAppended { session: SessionKey, record: Record },
    /// An agent is blocked on the operator.
    InteractionRequested(Interaction),
    /// An interaction was answered, here or elsewhere.
    InteractionResolved { id: String },
    /// The engine connected or disconnected.
    StatusChanged {
        connected: bool,
        detail: Option<String>,
    },
    /// Something to show that is not an error.
    Notice { level: String, message: String },
}

/// How an operator answered an interaction.
#[derive(Debug, Clone, Default)]
pub struct Answer {
    /// An approval outcome: `allowed-once` or `rejected`.
    pub outcome: Option<String>,
    /// A question batch: one entry per question.
    pub answers: Option<Vec<Value>>,
}

/// Why an engine call failed.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The engine is not attached right now.
    #[error("{engine} is not connected")]
    NotConnected { engine: EngineId },
    /// The engine cannot do this, and its capabilities said so.
    #[error("{engine} does not support {feature}")]
    Unsupported {
        engine: EngineId,
        feature: &'static str,
    },
    /// The engine reported a failure. The code is the engine's own.
    #[error("{code}: {message}")]
    Upstream {
        code: String,
        message: String,
        details: Option<Value>,
    },
    /// The session is not one this engine knows.
    #[error("unknown session {0}")]
    UnknownSession(SessionKey),
    /// The call did not finish in time.
    #[error("timed out: {0}")]
    Timeout(&'static str),
}

impl EngineError {
    /// A failure with no engine code of its own.
    pub fn upstream(message: impl Into<String>) -> Self {
        Self::Upstream {
            code: "REMOTE_ERROR".to_string(),
            message: message.into(),
            details: None,
        }
    }

    /// The code a client should branch on.
    pub fn code(&self) -> &str {
        match self {
            Self::NotConnected { .. } => "NOT_CONNECTED",
            Self::Unsupported { .. } => "UNSUPPORTED",
            Self::Upstream { code, .. } => code,
            Self::UnknownSession(_) => "UNKNOWN_SESSION",
            Self::Timeout(_) => "TIMEOUT",
        }
    }
}

/// One agent product, adapted to the shared model.
#[async_trait]
pub trait Engine: Send + Sync {
    /// Which product this is.
    fn id(&self) -> EngineId;

    /// What this adapter can do. The UI renders from this.
    fn capabilities(&self) -> Capabilities;

    /// Whether a prompt can be sent right now, and a one-line reason.
    fn status(&self) -> (bool, Option<String>);

    /// Every session this engine currently lists.
    async fn sessions(&self) -> Result<Vec<Session>, EngineError>;

    /// A page of records, newest last, at most `limit` of them.
    ///
    /// `before` pages backwards: only records with a smaller `seq`.
    async fn records(
        &self,
        session: &SessionKey,
        limit: usize,
        before: Option<i64>,
    ) -> Result<Vec<Record>, EngineError>;

    /// Send a prompt.
    async fn prompt(&self, session: &SessionKey, text: &str) -> Result<(), EngineError>;

    /// Stop the running turn.
    async fn cancel(&self, session: &SessionKey) -> Result<(), EngineError>;

    /// Answer a pending interaction.
    async fn answer(&self, id: &str, answer: Answer) -> Result<(), EngineError>;

    /// Choose a model.
    async fn set_model(&self, session: &SessionKey, model: &str) -> Result<(), EngineError>;

    /// Choose a permission preset.
    async fn set_permission(&self, session: &SessionKey, preset: &str) -> Result<(), EngineError>;

    /// The engine's event stream. One receiver per subscriber.
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EngineEvent>;
}
