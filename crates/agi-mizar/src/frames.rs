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
//! [`record_to_envelope`] then puts that event back inside the
//! `{"type":"event","event":{…}}` envelope a `records[]` element is defined to
//! be — the shape the golden fixture pins and the phones decode.

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
    FileUnknown,
    FileForbidden,
    FileTooLarge,
    UploadUnknown,
    UploadBadChunk,
    UploadTooLarge,
    UploadIncomplete,
    AttachmentUnknown,
    AttachmentTooLarge,
    AttachmentUnsupported,
    AttachmentFailed,
    /// A peer asked for a directory outside the operator's remote allowlist.
    WorkspaceNotAllowed,
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
            Self::FileUnknown => "FILE_UNKNOWN",
            Self::FileForbidden => "FILE_FORBIDDEN",
            Self::FileTooLarge => "FILE_TOO_LARGE",
            Self::UploadUnknown => "UPLOAD_UNKNOWN",
            Self::UploadBadChunk => "UPLOAD_BAD_CHUNK",
            Self::UploadTooLarge => "UPLOAD_TOO_LARGE",
            Self::UploadIncomplete => "UPLOAD_INCOMPLETE",
            Self::AttachmentUnknown => "ATTACHMENT_UNKNOWN",
            Self::AttachmentTooLarge => "ATTACHMENT_TOO_LARGE",
            Self::AttachmentUnsupported => "ATTACHMENT_UNSUPPORTED",
            Self::AttachmentFailed => "ATTACHMENT_FAILED",
            Self::WorkspaceNotAllowed => "WORKSPACE_NOT_ALLOWED",
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
            // The preview page branches on these three: forbidden and unknown
            // get different advice, and too-large suggests a different viewer.
            "FILE_UNKNOWN" => Self::FileUnknown,
            "FILE_FORBIDDEN" => Self::FileForbidden,
            "FILE_TOO_LARGE" => Self::FileTooLarge,
            // A caller's mistake — an offset past the end, a boundary that is
            // not a turn end — is the client's to fix, not a remote failure.
            "FILE_SLICE_INVALID" | "MALFORMED_FRAME" => Self::MalformedFrame,
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
    /// The frame exactly as it arrived.
    ///
    /// `hello` is the one frame whose fields the protocol puts at the top level
    /// rather than inside `payload`, so the raw object has to be kept to read
    /// them without also accepting a `session.list` that carries a stray token.
    root: Value,
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
            root: value,
        })
    }

    /// The `hello` fields, when this is one.
    pub fn hello(&self) -> Option<Hello> {
        if self.kind != "hello" {
            return None;
        }
        // The app and the protocol put the handshake fields at the top level.
        // An earlier build of this server only looked inside `payload`, so a
        // real phone opened a socket and was answered "send hello first" — the
        // node stand-in had been written to match the bug. Accept both.
        let field = |name: &str| {
            self.root
                .get(name)
                .or_else(|| self.payload.get(name))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        Some(Hello {
            token: field("token")?,
            app_version: field("appVersion"),
            device_model: field("deviceModel"),
            engine: field("engine"),
            account_id: field("accountId"),
        })
    }
}

impl ClientFrame {
    /// Build one method frame, for a method that fans out into others.
    ///
    /// `session.sync` answers a batch by re-dispatching each item through the
    /// same match the single-call methods use; this is the frame that lets it do
    /// that without duplicating -- and then drifting from -- their logic. The
    /// root object is unused by every method except `hello`, which the caller
    /// here never builds.
    ///
    /// - Parameters:
    ///   - id: the correlation id the answer would carry (the caller discards
    ///     it; this is not a frame that goes to a phone).
    ///   - kind: the method name.
    ///   - payload: its arguments.
    /// - Returns: the frame.
    pub fn method(id: impl Into<String>, kind: &str, payload: Value) -> Self {
        Self {
            id: Some(id.into()),
            kind: kind.to_string(),
            payload,
            root: Value::Null,
        }
    }
}

/// What a `hello` frame carries.
#[derive(Debug, Clone)]
pub struct Hello {
    pub token: String,
    pub app_version: Option<String>,
    pub device_model: Option<String>,
    /// Which agent this connection is for, when the phone chose one.
    ///
    /// The account flow puts the choice in the token instead, because there it
    /// has to survive a reconnect the relay brokers. A phone that paired by
    /// scanning a code has no such broker: its token is the plain "this phone
    /// may talk to this computer" one, and the agent is asked for per socket.
    /// A token that already carries a choice wins over this — a scoped token
    /// must not be talked out of its scope by the client.
    pub engine: Option<String>,
    /// The account the phone is signed in to, when it is signed in at all.
    ///
    /// Additive, and absent from every phone built before this: a host that
    /// requires it would strand them. What it buys is the one case neither the
    /// pairing code nor the device token can answer -- a phone signed in to one
    /// account reaching a computer signed in to another.
    pub account_id: Option<String>,
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
        RecordKind::UserMessage => {
            // DSH's UserMessage has no `turn` field; only turn/start and
            // turn/end own one. Writing 0 invented a turn no entry had and made
            // a prompt look like it belonged to the turn numbered zero.
            let mut data = serde_json::Map::new();
            if let Some(turn) = record.turn {
                data.insert("turn".into(), json!(turn));
            }
            data.insert(
                "content".into(),
                json!([{ "type": "text", "text": record.text.clone().unwrap_or_default() }]),
            );
            data.insert("source".into(), json!({ "kind": "user" }));
            json!({
                "type": "user/message", "seq": record.seq, "time": record.time,
                "data": data
            })
        }
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
        // A notice is the conversation talking about itself -- a compaction,
        // a summary that replaced older turns -- and the phone draws it as one
        // centred line in the stream. Dropping these is why a compacted
        // conversation looked like it had simply lost its history.
        RecordKind::Notice => {
            let mut data = serde_json::Map::new();
            if let Some(turn) = record.turn {
                data.insert("turn".into(), json!(turn));
            }
            data.insert(
                "text".into(),
                json!(record.text.clone().unwrap_or_default()),
            );
            json!({
                "type": "notice", "seq": record.seq, "time": record.time,
                "data": data
            })
        }
        RecordKind::ToolCall | RecordKind::ToolResult => return None,
    };
    Some(event)
}

/// Put one translated record inside the envelope `records[]` is defined to carry.
///
/// A `session.messages` result is a list of *envelopes*, not of bare events:
/// `{"type":"event","event":{type,seq,time,data}}`, as
/// `protocol/fixtures/session-messages.json` shows. The envelope is what leaves
/// room for other element kinds later;
/// the phones currently tolerate a bare event too (`payload = event ?? raw`),
/// which is exactly why the drift was invisible. Both clients still decode the
/// envelope, so aligning the host with the golden shape breaks nothing.
///
/// Returns `None` exactly when [`record_to_event`] does: a tool call or result is
/// not part of the conversation summary.
pub fn record_to_envelope(record: &Record) -> Option<Value> {
    let event = record_to_event(record)?;
    Some(json!({ "type": "event", "event": event }))
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
    /// `claude_desktop` / `codex_desktop` for GUI transcripts, `null` for CLI.
    /// Additive: an old client ignores it.
    pub source: Option<String>,
    /// The row's title, when the host knows one.
    ///
    /// Additive, and it is what removes a request per session from the phone's
    /// history sync: the sidebar used to learn every title from a separate
    /// `session.stats` call, so naming 342 rows was 342 round trips over the
    /// relay. The host already holds the title in the same snapshot this row
    /// comes from, so carrying it here costs bytes instead of round trips.
    pub title: Option<String>,
    /// The model the session is using, when the engine has said.
    ///
    /// Same reason as `title`, and it is what the phone draws above the
    /// composer. Additive.
    pub model: Option<String>,
    /// How full the model's context window is, when the engine can tell.
    ///
    /// `null` is "this host cannot say", not zero. Additive, and it is the
    /// third field that used to cost one `session.stats` request per row.
    pub context: Option<SessionRowContext>,
}

/// The context reading on a `session.list` row, in the shape the phones decode.
///
/// Deliberately not `agi_protocol::domain::ContextPressure`: that struct's
/// token field serialises as `used` for the desktop window, while the phone's
/// `ContextPressure` decodes `pressureTokens` (the same spelling
/// `session.messages` and `session.stats` already put on the wire). Reusing the
/// domain type here would have produced a context object with a window and a
/// percentage but no token count -- a ring that draws but cannot say how full
/// it is.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SessionRowContext {
    /// The model's window, in tokens.
    pub window: u64,
    /// Tokens currently occupied, as the engine measures them.
    #[serde(rename = "pressureTokens")]
    pub pressure_tokens: u64,
    /// Occupancy, rounded and clamped -- an integer, as the app decodes it.
    pub percent: u64,
}

/// The largest `records[]` page this host puts in one frame, in serialized bytes.
///
/// The relay refuses to forward a frame over `maxMessageBytes` (900 KiB by
/// default) and **drops it** -- the socket stays up, the phone waits out its
/// 60 s timeout, and the session never syncs. iOS's `URLSessionWebSocketTask`
/// closes the socket outright above one mebibyte, so "send it anyway and let the
/// transport split it" is not available. The page is therefore cut here, with
/// room for the envelope and the other fields.
pub const PAGE_BYTES_MAX: usize = 700 * 1024;

/// Cut a page down to what one frame can carry, dropping the oldest records.
///
/// Newest-first is the only order that can be cut safely: the phone merges what
/// it receives, and `hasMore` tells it to page backwards for the rest. Cutting
/// the newest instead would leave the conversation's tail missing with no cursor
/// that asks for it.
///
/// One record is always kept, even over budget: a single assistant message
/// larger than the cap cannot be split here, and dropping it would lose the
/// answer rather than shorten the page.
///
/// - Parameters:
///   - records: the page's envelopes, oldest first.
///   - max_bytes: the budget for their serialized forms.
/// - Returns: the kept envelopes and how many were dropped from the front.
pub fn trim_page_bytes(records: Vec<Value>, max_bytes: usize) -> (Vec<Value>, usize) {
    let sizes: Vec<usize> = records.iter().map(|value| value.to_string().len()).collect();
    let mut total: usize = sizes.iter().sum();
    let mut dropped = 0;
    while total > max_bytes && dropped + 1 < records.len() {
        total -= sizes[dropped];
        dropped += 1;
    }
    if dropped == 0 {
        return (records, 0);
    }
    (records.into_iter().skip(dropped).collect(), dropped)
}

/// The oldest log position an envelope carries, when it carries one.
///
/// Used after trimming: the page's `oldestRawSeq` has to name the first record
/// the phone actually received, or the backwards page that fills the gap starts
/// at the wrong place.
pub fn envelope_seq(envelope: &Value) -> Option<i64> {
    envelope
        .get("event")
        .and_then(|event| event.get("seq"))
        .and_then(Value::as_i64)
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
        // The shape the app sends: the token is on the frame, not in a payload.
        let hello = ClientFrame::parse(
            r#"{"v":1,"type":"hello","id":"h1","token":"mz_abc","appVersion":"0.1.0","protocolVersion":1}"#,
        )
        .unwrap();
        assert_eq!(hello.hello().unwrap().token, "mz_abc");
        assert_eq!(hello.hello().unwrap().app_version.as_deref(), Some("0.1.0"));

        // The older nested shape stays readable.
        let nested =
            ClientFrame::parse(r#"{"v":1,"type":"hello","id":"h1","payload":{"token":"mz_abc"}}"#)
                .unwrap();
        assert_eq!(nested.hello().unwrap().token, "mz_abc");

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
    fn a_page_that_does_not_fit_is_cut_at_its_oldest_end() {
        // The relay drops an oversized frame and the phone waits out a timeout
        // for an answer that was discarded; the page is therefore cut here.
        // Newest-first is the only safe direction: the phone merges what it got
        // and `hasMore` sends it back for the rest.
        let page: Vec<Value> = (0..4)
            .map(|seq| {
                json!({
                    "type": "event",
                    "event": {
                        "type": "assistant/message", "seq": seq, "time": seq,
                        "data": { "text": "x".repeat(200) }
                    }
                })
            })
            .collect();
        let each = page[0].to_string().len();
        let (kept, dropped) = trim_page_bytes(page.clone(), each * 2);
        assert_eq!(dropped, 2, "two records had to go");
        assert_eq!(kept.len(), 2);
        assert_eq!(envelope_seq(&kept[0]), Some(2), "the cursor names what shipped");
        assert_eq!(envelope_seq(&kept[1]), Some(3));

        // One record is always kept: an answer larger than the cap cannot be
        // split here, and dropping it would lose the answer, not shorten a page.
        let single = vec![page[0].clone()];
        let (kept, dropped) = trim_page_bytes(single, 10);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 1);

        // Nothing to cut is not a reason to reallocate.
        let (kept, dropped) = trim_page_bytes(page, PAGE_BYTES_MAX);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 4);
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
    fn a_prompt_without_a_turn_does_not_invent_turn_zero() {
        // DSH's UserMessage has no turn field, and the golden fixture proves it.
        // Writing 0 made iOS treat a prompt as belonging to a turn no entry had.
        let record = Record {
            seq: 7,
            time: 1_700_000_000_000,
            turn: None,
            step: None,
            kind: RecordKind::UserMessage,
            text: Some("hello".to_string()),
            tool: None,
        };
        let event = record_to_event(&record).unwrap();
        assert!(event["data"].get("turn").is_none());
        assert_eq!(event["data"]["content"][0]["text"], "hello");
    }

    #[test]
    fn a_notice_without_a_turn_does_not_invent_turn_zero() {
        let record = Record {
            seq: 9,
            time: 1_700_000_000_001,
            turn: None,
            step: None,
            kind: RecordKind::Notice,
            text: Some("Background subagent obage-1 finished".to_string()),
            tool: None,
        };
        let event = record_to_event(&record).unwrap();
        assert!(event["data"].get("turn").is_none());
        assert_eq!(
            event["data"]["text"],
            "Background subagent obage-1 finished"
        );
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
