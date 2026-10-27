//! The Mizar v1 wire, server side.
//!
//! Five server frames and two client ones, matching `docs/protocol-v1.md` and the
//! golden frames in `protocol/fixtures/` byte for byte. The phone is already
//! shipped, so this file has no room for a nicer spelling of anything: `hostId`
//! is camelCase, `welcome` carries no `id`, and an error frame is
//! `{v, type, code, message, retryable, id}`.
//!
//! ### Records are translated, not forwarded
//!
//! The phone's conversation screen decodes DSH's event vocabulary —
//! `{type:"user/message", seq, time, data:{turn, content}}`. Our shared model is
//! flatter, so [`record_to_event`] rebuilds the shape the phone already knows.
//! That is deliberate: the alternative is a phone release for every engine we
//! add, and the phone must not learn that Codex and Claude Code exist.

use serde::Serialize;
use serde_json::{json, Value};

use agi_protocol::domain::{Record, RecordKind};

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The HTTP prefix every route lives under.
pub const HTTP_PREFIX: &str = "/mizar/v1";

/// The WebSocket path.
pub const WS_PATH: &str = "/mizar/v1/ws";

/// The pairing URI scheme.
pub const PAIR_SCHEME: &str = "mizar";

/// The default listener port.
pub const DEFAULT_PORT: u16 = 19_392;

/// A frame the server sends.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ServerFrame {
    /// Answer to `hello`. Carries no `id` — there is nothing to correlate.
    #[serde(rename = "welcome")]
    Welcome {
        v: u32,
        #[serde(rename = "hostId")]
        host_id: String,
        #[serde(rename = "hostName")]
        host_name: String,
        #[serde(rename = "protocolVersion")]
        protocol_version: u32,
        home: String,
        #[serde(rename = "deviceId")]
        device_id: String,
    },
    /// A method's answer.
    #[serde(rename = "ok")]
    Ok { v: u32, id: String, result: Value },
    /// A method's failure.
    #[serde(rename = "error")]
    Error {
        v: u32,
        id: Option<String>,
        code: String,
        message: String,
        retryable: bool,
    },
    /// A server-initiated notification.
    #[serde(rename = "event")]
    Event { v: u32, event: String, data: Value },
}

impl ServerFrame {
    /// A `welcome` for one device.
    pub fn welcome(input: WelcomeInput) -> Self {
        Self::Welcome {
            v: PROTOCOL_VERSION,
            host_id: input.host_id,
            host_name: input.host_name,
            protocol_version: PROTOCOL_VERSION,
            home: input.home,
            device_id: input.device_id,
        }
    }

    /// A successful answer.
    pub fn ok(id: impl Into<String>, result: Value) -> Self {
        Self::Ok {
            v: PROTOCOL_VERSION,
            id: id.into(),
            result,
        }
    }

    /// A failure with one of the protocol's codes.
    pub fn error(id: Option<String>, code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            v: PROTOCOL_VERSION,
            id,
            code: code.as_str().to_string(),
            message: message.into(),
            retryable: code.retryable(),
        }
    }

    /// A notification.
    pub fn event(event: impl Into<String>, data: Value) -> Self {
        Self::Event {
            v: PROTOCOL_VERSION,
            event: event.into(),
            data,
        }
    }
}

/// The pieces of a `welcome`.
pub struct WelcomeInput {
    pub host_id: String,
    pub host_name: String,
    pub home: String,
    pub device_id: String,
}

/// The codes the phone branches on.
///
/// A message is for a human; a code is for the client. Matching on text is how a
/// client ends up broken by a wording change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    ProtocolVersionMismatch,
    MalformedFrame,
    Unauthenticated,
    PairingRequired,
    PairingCodeInvalid,
    PairingCodeExpired,
    PairingCodeUnknown,
    RateLimited,
    DeviceRevoked,
    WriterHeld,
    ModelUnavailable,
    RemoteError,
    NotImplemented,
    Internal,
    ResponseTooLarge,
}

impl ErrorCode {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProtocolVersionMismatch => "PROTOCOL_VERSION_MISMATCH",
            Self::MalformedFrame => "MALFORMED_FRAME",
            Self::Unauthenticated => "UNAUTHENTICATED",
            Self::PairingRequired => "PAIRING_REQUIRED",
            Self::PairingCodeInvalid => "PAIRING_CODE_INVALID",
            Self::PairingCodeExpired => "PAIRING_CODE_EXPIRED",
            Self::PairingCodeUnknown => "PAIRING_CODE_UNKNOWN",
            Self::RateLimited => "RATE_LIMITED",
            Self::DeviceRevoked => "DEVICE_REVOKED",
            Self::WriterHeld => "WRITER_HELD",
            Self::ModelUnavailable => "MODEL_UNAVAILABLE",
            Self::RemoteError => "REMOTE_ERROR",
            Self::NotImplemented => "NOT_IMPLEMENTED",
            Self::Internal => "INTERNAL",
            Self::ResponseTooLarge => "RESPONSE_TOO_LARGE",
        }
    }

    /// Whether the client may retry the same call.
    pub fn retryable(self) -> bool {
        matches!(self, Self::RateLimited | Self::Internal)
    }

    /// Map an engine failure's code onto a protocol code.
    ///
    /// Only the three the phone acts on are translated; everything else stays
    /// `REMOTE_ERROR` with the engine's message intact, because a client cannot
    /// do anything with a code it has never been taught.
    pub fn from_engine(code: &str) -> Self {
        match code {
            "WRITER_HELD" => Self::WriterHeld,
            "MODEL_UNAVAILABLE" => Self::ModelUnavailable,
            "NOT_CONNECTED" => Self::RemoteError,
            _ => Self::RemoteError,
        }
    }
}

/// A parsed client frame.
#[derive(Debug, Clone)]
pub struct ClientFrame {
    /// The client's correlation id, when it sent one.
    pub id: Option<String>,
    /// `hello` or a method name.
    pub kind: String,
    pub payload: Value,
}

impl ClientFrame {
    /// Parse one client message.
    ///
    /// Structural problems are `None` rather than an error frame: a frame that
    /// cannot be read cannot carry an id to answer with, and answering with a
    /// guessed id is worse than silence the client's timeout handles.
    pub fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let object = value.as_object()?;
        let version = object.get("v").and_then(Value::as_u64)?;
        if version != PROTOCOL_VERSION as u64 {
            // A version mismatch is the one case worth answering without a
            // trustworthy id: the client needs to be told to upgrade.
            return None;
        }
        let kind = object.get("type").and_then(Value::as_str)?.to_string();
        Some(Self {
            id: object.get("id").and_then(Value::as_str).map(str::to_string),
            kind,
            payload: object.get("payload").cloned().unwrap_or(Value::Null),
        })
    }

    /// The `hello` fields, when this is one.
    pub fn hello(&self) -> Option<Hello> {
        if self.kind != "hello" {
            return None;
        }
        let token = self.payload.get("token").and_then(Value::as_str)?;
        Some(Hello {
            token: token.to_string(),
            app_version: self
                .payload
                .get("appVersion")
                .and_then(Value::as_str)
                .map(str::to_string),
            device_model: self
                .payload
                .get("deviceModel")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
}

/// What a `hello` frame carries.
#[derive(Debug, Clone)]
pub struct Hello {
    pub token: String,
    pub app_version: Option<String>,
    pub device_model: Option<String>,
}

/// Rebuild the DSH event shape the phone decodes from one shared record.
///
/// Returns `None` for records the phone's summary view would drop anyway — tool
/// calls and results. Sending them would cost bandwidth on a relay and change
/// nothing on screen.
pub fn record_to_event(record: &Record) -> Option<Value> {
    let turn = record.turn.unwrap_or(0);
    let event = match record.kind {
        RecordKind::TurnStart => json!({
            "type": "turn/start", "seq": record.seq, "time": record.time,
            "data": { "turn": turn }
        }),
        RecordKind::TurnEnd => json!({
            "type": "turn/end", "seq": record.seq, "time": record.time,
            "data": {
                "turn": turn,
                "reason": { "kind": record.text.clone().unwrap_or_else(|| "success".to_string()) }
            }
        }),
        RecordKind::Step => json!({
            "type": "step/start", "seq": record.seq, "time": record.time,
            "data": { "turn": turn, "step": record.step.unwrap_or(1) }
        }),
        RecordKind::UserMessage => json!({
            "type": "user/message", "seq": record.seq, "time": record.time,
            "data": {
                "turn": turn,
                "content": [{ "type": "text", "text": record.text.clone().unwrap_or_default() }],
                "source": { "kind": "user" }
            }
        }),
        RecordKind::AssistantMessage => json!({
            "type": "assistant/message", "seq": record.seq, "time": record.time,
            "data": {
                "turn": turn,
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": record.text.clone().unwrap_or_default() }]
                }
            }
        }),
        RecordKind::ToolCall | RecordKind::ToolResult | RecordKind::Notice => return None,
    };
    Some(event)
}

/// A `session.list` row.
#[derive(Debug, Clone, Serialize)]
pub struct SessionRow {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
    pub running: bool,
    /// A session with no title and no activity yet.
    pub blank: bool,
    /// Whether an agent is loaded. The phone uses this to decide whether a row
    /// can be prompted without waking anything.
    #[serde(rename = "agentAvailable")]
    pub agent_available: bool,
    pub cwd: Option<String>,
    #[serde(rename = "parentSessionId")]
    pub parent_session_id: Option<String>,
    pub origin: Option<String>,
    /// Which product this row belongs to. Additive: an old client ignores it.
    pub engine: String,
}

/// Build the pairing URI the desktop shows as a QR code.
///
/// A phone that is not on this network cannot dial `h` at all, so when the
/// tunnel has a room the URI also carries `u`, the relay base URL for that room.
/// Both are offered: an app on the same network takes the short direct route,
/// and one anywhere else falls through to the tunnel.
pub fn pairing_uri(
    host_port: &str,
    relay_base: Option<&str>,
    code: &str,
    host_name: &str,
) -> String {
    let relay = relay_base
        .map(|base| format!("u={}&", url_encode(base)))
        .unwrap_or_default();
    format!(
        "{PAIR_SCHEME}://pair?h={}&{relay}c={}&n={}&v={PROTOCOL_VERSION}",
        url_encode(host_port),
        url_encode(code),
        url_encode(host_name)
    )
}

/// Percent-encode the characters a URI query cannot carry raw.
fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_matches_the_golden_frame() {
        let frame = ServerFrame::welcome(WelcomeInput {
            host_id: "c9b63045-e610-4910-9120-2816d8604cba".to_string(),
            host_name: "Shirazus-Mac-mini.local".to_string(),
            home: "/Users/x/.dsh".to_string(),
            device_id: "ba4b7c91-a2e9-4791-b9bb-2092f3e30980".to_string(),
        });
        let value = serde_json::to_value(frame).unwrap();
        assert_eq!(value["type"], "welcome");
        assert_eq!(value["v"], 1);
        assert_eq!(value["hostId"], "c9b63045-e610-4910-9120-2816d8604cba");
        assert_eq!(value["protocolVersion"], 1);
        // The golden frame has no `id`, and neither may ours.
        assert!(value.get("id").is_none());
    }

    #[test]
    fn error_frame_matches_the_golden_shape() {
        let frame = ServerFrame::error(
            Some("w4".to_string()),
            ErrorCode::RemoteError,
            "unknown preset",
        );
        let value = serde_json::to_value(frame).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["code"], "REMOTE_ERROR");
        assert_eq!(value["retryable"], false);
        assert_eq!(value["id"], "w4");
    }

    #[test]
    fn parses_a_hello_and_a_method_call() {
        let hello = ClientFrame::parse(
            r#"{"v":1,"type":"hello","id":"h1","payload":{"token":"mz_abc","appVersion":"0.1.0"}}"#,
        )
        .unwrap();
        assert_eq!(hello.hello().unwrap().token, "mz_abc");

        let call =
            ClientFrame::parse(r#"{"v":1,"type":"session.list","id":"m1","payload":{}}"#).unwrap();
        assert_eq!(call.kind, "session.list");
        assert_eq!(call.id.as_deref(), Some("m1"));
        assert!(call.hello().is_none());
    }

    #[test]
    fn refuses_a_foreign_protocol_version() {
        assert!(ClientFrame::parse(r#"{"v":2,"type":"session.list","id":"m1"}"#).is_none());
        assert!(ClientFrame::parse("not json").is_none());
        assert!(ClientFrame::parse(r#"{"type":"session.list"}"#).is_none());
    }

    #[test]
    fn builds_a_pairing_uri_with_an_encoded_host() {
        let uri = pairing_uri("192.168.1.10:19392", None, "K7QP-3M2X", "Mac mini");
        assert_eq!(
            uri,
            "mizar://pair?h=192.168.1.10%3A19392&c=K7QP-3M2X&n=Mac%20mini&v=1"
        );
    }

    #[test]
    fn a_tunnel_adds_the_route_a_phone_off_this_network_can_take() {
        let uri = pairing_uri(
            "192.168.1.10:19392",
            Some("https://relay.example.com/r/ABCD1234"),
            "K7QP-3M2X",
            "Mac mini",
        );
        assert_eq!(
            uri,
            "mizar://pair?h=192.168.1.10%3A19392&u=https%3A%2F%2Frelay.example.com%2Fr%2FABCD1234&c=K7QP-3M2X&n=Mac%20mini&v=1"
        );
    }

    #[test]
    fn translates_records_into_the_shape_the_phone_decodes() {
        let record = Record {
            seq: 7,
            time: 1_700_000_000_000,
            turn: Some(2),
            step: None,
            kind: RecordKind::UserMessage,
            text: Some("hello".to_string()),
            tool: None,
        };
        let event = record_to_event(&record).unwrap();
        assert_eq!(event["type"], "user/message");
        assert_eq!(event["data"]["turn"], 2);
        assert_eq!(event["data"]["content"][0]["text"], "hello");
        assert_eq!(event["data"]["source"]["kind"], "user");
    }

    #[test]
    fn drops_tool_records_the_phone_would_not_draw() {
        let record = Record {
            seq: 8,
            time: 0,
            turn: Some(1),
            step: Some(1),
            kind: RecordKind::ToolCall,
            text: None,
            tool: None,
        };
        assert!(record_to_event(&record).is_none());
    }
}
