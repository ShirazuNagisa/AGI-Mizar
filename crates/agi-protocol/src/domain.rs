//! The engine-neutral domain model.
//!
//! Three products with three different vocabularies converge here: DSH's
//! `SessionEvent`, Codex's `thread`/`turn`/`item`, and Claude Code's transcript
//! JSONL. Everything above this module — the local API, the UI, the store — reads
//! only these types, which is what keeps an engine change out of the rest of the
//! application.
//!
//! The model is deliberately narrow. It carries what a conversation screen
//! draws, not what an engine logs: a record is a message, a turn boundary, a tool
//! call, or a marker. Anything an engine knows that this model cannot express is
//! still reachable through the engine's own escape hatch (`invoke` for DSH), just
//! not through the shared surface.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::engine::EngineId;

/// One session, addressed across engines.
///
/// Serialised as `"dsh:session-123"`. The id an engine uses is opaque; the
/// prefix is what keeps two engines from colliding on a bare string, which they
/// eventually would — both DSH and Claude Code mint UUID-shaped ids.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionKey {
    /// Which engine owns it.
    pub engine: EngineId,
    /// The engine's own id, unmodified.
    pub id: String,
}

impl SessionKey {
    /// Build a key.
    pub fn new(engine: EngineId, id: impl Into<String>) -> Self {
        Self {
            engine,
            id: id.into(),
        }
    }

    /// The engine's own id, for calls that go back to it.
    pub fn raw(&self) -> &str {
        &self.id
    }
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.engine.as_str(), self.id)
    }
}

/// Why a key could not be parsed.
#[derive(Debug, thiserror::Error)]
#[error("not a session key: {0}")]
pub struct SessionKeyError(String);

impl FromStr for SessionKey {
    type Err = SessionKeyError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (prefix, id) = text
            .split_once(':')
            .ok_or_else(|| SessionKeyError(text.to_string()))?;
        let engine = EngineId::parse(prefix).ok_or_else(|| SessionKeyError(text.to_string()))?;
        if id.is_empty() {
            return Err(SessionKeyError(text.to_string()));
        }
        Ok(Self {
            engine,
            id: id.to_string(),
        })
    }
}

impl TryFrom<String> for SessionKey {
    type Error = SessionKeyError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<SessionKey> for String {
    fn from(value: SessionKey) -> Self {
        value.to_string()
    }
}

/// One session as the list draws it.
///
/// Not : the context reading is a float. Nothing keys a map by a session, so
/// the weaker bound costs nothing and the alternative is a percentage stored as
/// an integer someone has to remember to divide.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub key: SessionKey,
    /// Working directory, when the engine has one.
    pub cwd: Option<String>,
    pub title: Option<String>,
    /// The model this session is using, as the engine names it.
    pub model: Option<String>,
    /// Epoch milliseconds of the last activity the engine reported.
    pub updated_at: i64,
    /// A turn is running right now.
    pub running: bool,
    /// Set for a delegated child (a subagent, a fork's source, a sub-thread).
    pub parent: Option<String>,
    /// `subagent`, `fork`, or whatever the engine calls it. Opaque to the UI.
    pub origin: Option<String>,
    /// How many turns the engine has recorded.
    pub turns: u32,
    /// How full the model's context window is, when the engine can say.
    ///
    /// `None` is "this host cannot tell", which is not the same as zero: a
    /// session that has not run yet has no reading at all, and drawing an empty
    /// ring for it would be a claim nobody made.
    pub context: Option<ContextPressure>,
}

/// How much of the model's context window a session is using.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ContextPressure {
    /// The window's size in tokens.
    pub window: u64,
    /// Tokens currently occupied.
    pub used: u64,
    /// `used / window`, as the engine computes it.
    pub percent: f64,
}

impl Session {
    /// A session with only its identity known.
    pub fn bare(key: SessionKey, updated_at: i64) -> Self {
        Self {
            key,
            cwd: None,
            title: None,
            model: None,
            updated_at,
            running: false,
            parent: None,
            origin: None,
            turns: 0,
            context: None,
        }
    }
}

/// One record in a conversation, in the order the engine committed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The engine's own sequence number within the session.
    pub seq: i64,
    /// Epoch milliseconds.
    pub time: i64,
    pub turn: Option<u32>,
    pub step: Option<u32>,
    pub kind: RecordKind,
    /// Readable text, when the record carries any.
    pub text: Option<String>,
    /// Tool details, on [`RecordKind::ToolCall`] and [`RecordKind::ToolResult`].
    pub tool: Option<ToolInfo>,
}

impl Record {
    /// A text record.
    pub fn message(seq: i64, time: i64, turn: Option<u32>, kind: RecordKind, text: String) -> Self {
        Self {
            seq,
            time,
            turn,
            step: None,
            kind,
            text: Some(text),
            tool: None,
        }
    }
}

/// What a record is, for a renderer that switches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    TurnStart,
    TurnEnd,
    Step,
    UserMessage,
    AssistantMessage,
    ToolCall,
    ToolResult,
    Notice,
}

/// A tool invocation and its result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    /// Pairs a call with its result.
    pub call_id: String,
    pub name: String,
    /// The model's arguments, unparsed, as the engine received them.
    pub arguments: Option<String>,
    /// The result text, on a result record.
    pub output: Option<String>,
}

/// One model the engine offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// The engine's own identifier, passed back verbatim.
    pub id: String,
    pub label: String,
    pub provider: Option<String>,
    /// Reasoning levels this model accepts, when it has any.
    pub reasoning: Vec<String>,
}

/// One permission preset the engine offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPreset {
    /// The engine's own identifier, passed back verbatim.
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

/// What an agent is waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionKind {
    /// A tool needs permission to run.
    Approval,
    /// The model asked the operator something.
    Question,
}

/// One currency's balance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wallet {
    /// `CNY` or `USD`.
    pub currency: String,
    /// A decimal string, exactly as the provider wrote it.
    pub amount: String,
}

/// What the provider account is worth right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Balance {
    /// The provider answered.
    Ready {
        wallets: Vec<Wallet>,
        #[serde(default)]
        bonus: Vec<Wallet>,
        #[serde(default)]
        top_up_url: Option<String>,
    },
    /// Credentials exist but the account is not signed in.
    SignedOut,
    /// The provider is reachable but would not answer.
    Unavailable,
    /// This host has no account provider at all.
    Unsupported,
}

/// A file read out of a session's working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePreview {
    /// The path as the model named it.
    pub path: String,
    /// The file's own name.
    pub name: String,
    /// Bytes on disk.
    pub size: u64,
    /// UTF-8 text, when the file is text and within the cap.
    pub text: Option<String>,
    /// Whether `text` is a prefix of the file.
    pub truncated: bool,
}

/// What an attachment is, as the phone labels it when it starts an upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentKind {
    Image,
    File,
}

impl AttachmentKind {
    /// Parse the wire spelling.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "image" => Some(Self::Image),
            "file" => Some(Self::File),
            _ => None,
        }
    }

    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::File => "file",
        }
    }
}

/// One blob the host holds for a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptAttachment {
    pub kind: AttachmentKind,
    pub name: String,
    pub media_type: String,
    /// The bytes, for kinds the prompt carries inline.
    pub bytes: Vec<u8>,
    /// Where the host stored them, for kinds that travel as a path.
    pub path: Option<String>,
}

/// A file's facts, as the phone's preview page asks for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileStat {
    /// The path as the request named it, echoed back.
    pub path: String,
    /// The file's own name, without its directory.
    pub name: String,
    /// Bytes on disk.
    pub byte_size: u64,
    /// Last modification, epoch milliseconds.
    pub modified_at: i64,
    /// The media type this host associates with the extension.
    pub media_type: String,
    /// Whether the phone has a viewer for this type.
    pub previewable: bool,
}

/// One slice of a file, as the phone reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSlice {
    /// Where this slice starts.
    pub offset: u64,
    /// How many bytes it holds.
    pub length: u64,
    /// The whole file's size, so a client can show progress without a `stat`.
    pub byte_size: u64,
    /// The bytes of this slice; the wire carries them base64-encoded.
    pub data: Vec<u8>,
    /// Whether this slice reaches the end of the file.
    pub eof: bool,
}

/// One line of "what the running turn is doing".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEntry {
    /// Stable across refreshes, so a list can be merged rather than replaced.
    pub id: String,
    /// The engine's own sequence, for ordering.
    pub seq: i64,
    pub at: i64,
    pub kind: ActivityKind,
    /// A tool name, or the first words of a thought.
    pub title: String,
    /// One line. Anything longer belongs in the transcript.
    pub text: String,
    /// Which turn this line belongs to, when the engine numbers its turns.
    ///
    /// The phone draws the feed per turn and clears it when the turn ends, so a
    /// line without one cannot be placed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
}

/// What kind of work an activity line describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Thinking,
    Command,
    Output,
    Tool,
}

/// What a session has cost so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionStats {
    pub turns: u32,
    pub steps: u32,
    /// Milliseconds spent waiting on the model.
    pub llm_ms: i64,
    /// Milliseconds spent running tools.
    pub tool_ms: i64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// A file the model said it produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredFile {
    /// Relative to the session's working directory, as the model named it.
    pub path: String,
    /// The name to show.
    pub name: String,
    pub description: Option<String>,
}

/// One workspace the host knows about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub path: String,
    pub name: Option<String>,
    /// The sessions the host assigns to it, in its own order.
    pub session_ids: Vec<String>,
}

/// One question in a batch, as the agent asked it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// The id the answer must name.
    pub id: String,
    /// The question itself.
    pub text: String,
    /// A short label the agent suggested, when it offered one.
    pub header: Option<String>,
    /// Supporting detail, when the agent supplied any.
    pub detail: Option<String>,
    /// The choices, in the order the agent offered them.
    pub options: Vec<String>,
    /// Whether more than one option may be chosen.
    pub multi_select: bool,
}

/// One pending approval or question, as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interaction {
    /// The id to answer with. Engine-scoped, never parsed by the UI.
    pub id: String,
    pub session: Option<SessionKey>,
    pub kind: InteractionKind,
    /// The tool name, or the first question.
    pub title: String,
    /// A reason or a question body, when the engine supplied one.
    pub detail: Option<String>,
    /// Epoch milliseconds.
    pub requested_at: i64,
    /// The questions of a [`InteractionKind::Question`], in the agent's order.
    ///
    /// Empty for an approval. A question batch that arrives with none is not
    /// answerable and is not stored.
    pub questions: Vec<Question>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_keys_round_trip_through_text() {
        let key = SessionKey::new(EngineId::Claude, "abc-123");
        assert_eq!(key.to_string(), "claude:abc-123");
        assert_eq!("claude:abc-123".parse::<SessionKey>().unwrap(), key);
    }

    #[test]
    fn session_keys_round_trip_through_json() {
        let key = SessionKey::new(EngineId::Dsh, "session-1");
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, "\"dsh:session-1\"");
        assert_eq!(serde_json::from_str::<SessionKey>(&json).unwrap(), key);
    }

    #[test]
    fn a_key_without_an_engine_or_an_id_is_refused() {
        assert!("session-1".parse::<SessionKey>().is_err());
        assert!("nope:session-1".parse::<SessionKey>().is_err());
        assert!("dsh:".parse::<SessionKey>().is_err());
    }
}
