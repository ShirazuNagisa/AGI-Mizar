//! DSH Link v1: the wire between the `dsh-mizar` helper plugin and this app.
//!
//! The plugin dials this application, not the other way round, so this module
//! describes both directions: frames it sends us, and frames we send back. The
//! authoritative description is `dsh/docs/protocol-link-v1.md` in the plugin
//! repository; this file must stay a mechanical mirror of it.
//!
//! ### Parsing is tolerant, sending is exact
//!
//! A frame this build does not understand is `None`, not an error — a newer
//! plugin may send a frame type from the future, and dropping the connection
//! over it would turn a version skew into an outage. The reverse direction has no
//! such freedom: we send only what we know, and a field we omit is a field the
//! plugin will not see.
//!
//! ### Payloads are opaque
//!
//! `payload`, `value`, `request` and `details` carry DSH's own JSON. Nothing here
//! interprets them; the projection layer does, and it is allowed to be wrong
//! about a field without breaking the transport.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol version of this link.
pub const LINK_VERSION: u32 = 1;

/// The path the desktop serves the link on.
pub const LINK_PATH: &str = "/dsh";

/// What the plugin reports about itself.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginIdentity {
    pub name: String,
    pub version: String,
    #[serde(rename = "linkVersion")]
    pub link_version: u32,
}

/// What the plugin reports about the DSH process it lives in.
#[derive(Debug, Clone, Deserialize)]
pub struct HostIdentity {
    pub platform: String,
    pub home: String,
    pub node: String,
}

/// The first frame on every connection.
#[derive(Debug, Clone, Deserialize)]
pub struct HelloFrame {
    pub token: String,
    pub plugin: PluginIdentity,
    pub dsh: HostIdentity,
    #[serde(default)]
    pub endpoints: Vec<String>,
    #[serde(default)]
    pub services: Vec<String>,
}

/// One post-commit record or projection change, forwarded verbatim.
#[derive(Debug, Clone, Deserialize)]
pub struct EventFrame {
    /// Monotonic across sessions; a gap means we must re-read.
    pub seq: i64,
    /// ISO-8601 timestamp of forwarding.
    pub at: String,
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    /// `session/event` or `projection/changed`.
    pub name: String,
    pub payload: Value,
}

/// A unary result, successful or not.
#[derive(Debug, Clone, Deserialize)]
pub struct ResultFrame {
    pub id: String,
    pub ok: bool,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub details: Option<Value>,
}

impl ResultFrame {
    /// The value, or the failure rendered as one line.
    pub fn into_result(self) -> Result<Value, UpstreamError> {
        if self.ok {
            return Ok(self.value.unwrap_or(Value::Null));
        }
        Err(UpstreamError {
            code: self.code.unwrap_or_else(|| "REMOTE_ERROR".to_string()),
            message: self.message.unwrap_or_else(|| "the plugin reported a failure".to_string()),
            details: self.details,
        })
    }
}

/// A failure DSH produced, passed through without re-mapping.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamError {
    pub code: String,
    pub message: String,
    pub details: Option<Value>,
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for UpstreamError {}

/// One item of a streamed result.
#[derive(Debug, Clone, Deserialize)]
pub struct StreamItemFrame {
    pub id: String,
    #[serde(default)]
    pub value: Option<Value>,
}

/// A stream ended. `reason` is `done`, `cancelled` or `failed`.
#[derive(Debug, Clone, Deserialize)]
pub struct StreamEndFrame {
    pub id: String,
    pub reason: String,
}

/// An agent is blocked on a permission prompt.
#[derive(Debug, Clone, Deserialize)]
pub struct ApprovalFrame {
    pub id: String,
    pub request: Value,
}

/// An agent is blocked on a question batch.
#[derive(Debug, Clone, Deserialize)]
pub struct QuestionFrame {
    pub id: String,
    pub request: Value,
}

/// An operator-facing notice from the plugin. Never fatal.
#[derive(Debug, Clone, Deserialize)]
pub struct NoticeFrame {
    pub level: String,
    pub message: String,
}

/// Everything the plugin can send.
#[derive(Debug, Clone)]
pub enum LinkFrame {
    Hello(HelloFrame),
    Event(EventFrame),
    Result(ResultFrame),
    StreamItem(StreamItemFrame),
    StreamEnd(StreamEndFrame),
    Approval(ApprovalFrame),
    Question(QuestionFrame),
    Notice(NoticeFrame),
}

impl LinkFrame {
    /// Parse one WebSocket message.
    ///
    /// Returns `None` for text that is not JSON, is not an object, or names a
    /// frame this build does not implement — all three are "ignore this", never
    /// "drop the connection".
    pub fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let kind = value.get("type")?.as_str()?;

        match kind {
            "hello" => serde_json::from_value(value).ok().map(LinkFrame::Hello),
            "event" => serde_json::from_value(value).ok().map(LinkFrame::Event),
            "result" => serde_json::from_value(value).ok().map(LinkFrame::Result),
            "stream.item" => serde_json::from_value(value).ok().map(LinkFrame::StreamItem),
            "stream.end" => serde_json::from_value(value).ok().map(LinkFrame::StreamEnd),
            "approval" => serde_json::from_value(value).ok().map(LinkFrame::Approval),
            "question" => serde_json::from_value(value).ok().map(LinkFrame::Question),
            "notice" => serde_json::from_value(value).ok().map(LinkFrame::Notice),
            _ => None,
        }
    }
}

/// How an endpoint is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InvokeMode {
    Unary,
    Stream,
}

/// Everything this application sends to the plugin.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum DesktopFrame {
    /// Accept the handshake.
    #[serde(rename = "welcome")]
    Welcome {
        v: u32,
        #[serde(rename = "linkVersion")]
        link_version: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        desktop: Option<String>,
    },
    /// Call any registered DSH endpoint.
    #[serde(rename = "invoke")]
    Invoke {
        v: u32,
        id: String,
        endpoint: String,
        args: Value,
        mode: InvokeMode,
    },
    /// Call an in-process service.
    #[serde(rename = "service")]
    Service {
        v: u32,
        id: String,
        service: String,
        method: String,
        args: Vec<Value>,
    },
    /// Stop a running stream.
    #[serde(rename = "cancel")]
    Cancel { v: u32, id: String },
    /// Answer a pending approval or question batch.
    #[serde(rename = "answer")]
    Answer {
        v: u32,
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        outcome: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        answers: Option<Vec<Value>>,
    },
    /// Ask the plugin to stop forwarding some event names.
    #[serde(rename = "filter")]
    Filter { v: u32, drop: Vec<String> },
}

impl DesktopFrame {
    /// The handshake answer.
    pub fn welcome(desktop: impl Into<String>) -> Self {
        Self::Welcome {
            v: LINK_VERSION,
            link_version: LINK_VERSION,
            desktop: Some(desktop.into()),
        }
    }

    /// A unary endpoint call.
    pub fn invoke(id: impl Into<String>, endpoint: impl Into<String>, args: Value) -> Self {
        Self::Invoke {
            v: LINK_VERSION,
            id: id.into(),
            endpoint: endpoint.into(),
            args,
            mode: InvokeMode::Unary,
        }
    }

    /// A streamed endpoint call.
    pub fn stream(id: impl Into<String>, endpoint: impl Into<String>, args: Value) -> Self {
        Self::Invoke {
            v: LINK_VERSION,
            id: id.into(),
            endpoint: endpoint.into(),
            args,
            mode: InvokeMode::Stream,
        }
    }

    /// An in-process service call.
    pub fn service(
        id: impl Into<String>,
        service: impl Into<String>,
        method: impl Into<String>,
        args: Vec<Value>,
    ) -> Self {
        Self::Service {
            v: LINK_VERSION,
            id: id.into(),
            service: service.into(),
            method: method.into(),
            args,
        }
    }

    /// An approval answer.
    pub fn approval_answer(id: impl Into<String>, outcome: impl Into<String>) -> Self {
        Self::Answer {
            v: LINK_VERSION,
            id: id.into(),
            outcome: Some(outcome.into()),
            answers: None,
        }
    }

    /// A question answer batch.
    pub fn question_answer(id: impl Into<String>, answers: Vec<Value>) -> Self {
        Self::Answer {
            v: LINK_VERSION,
            id: id.into(),
            outcome: None,
            answers: Some(answers),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_hello() {
        let text = r#"{"v":1,"type":"hello","token":"t","plugin":{"name":"dsh-mizar","version":"0.9.0","linkVersion":1},"dsh":{"platform":"darwin","home":"/Users/x","node":"v22"},"endpoints":["session/list"],"services":["workspaceRegistry"]}"#;
        let Some(LinkFrame::Hello(hello)) = LinkFrame::parse(text) else {
            panic!("expected hello")
        };
        assert_eq!(hello.token, "t");
        assert_eq!(hello.plugin.link_version, 1);
        assert_eq!(hello.endpoints, vec!["session/list"]);
    }

    #[test]
    fn parses_an_event_with_an_opaque_payload() {
        let text = r#"{"v":1,"type":"event","seq":7,"at":"2026-10-27T00:00:00Z","sessionId":"s1","name":"session/event","payload":{"event":{"type":"turn/start","data":{"turn":1}}}}"#;
        let Some(LinkFrame::Event(event)) = LinkFrame::parse(text) else {
            panic!("expected event")
        };
        assert_eq!(event.seq, 7);
        assert_eq!(event.session_id.as_deref(), Some("s1"));
        assert_eq!(event.payload["event"]["type"], "turn/start");
    }

    #[test]
    fn parses_a_failed_result_and_keeps_the_upstream_code() {
        let text = r#"{"v":1,"type":"result","id":"c1","ok":false,"code":"session/writer-held","message":"busy"}"#;
        let Some(LinkFrame::Result(result)) = LinkFrame::parse(text) else {
            panic!("expected result")
        };
        let error = result.into_result().unwrap_err();
        assert_eq!(error.code, "session/writer-held");
    }

    #[test]
    fn ignores_a_frame_from_the_future() {
        assert!(LinkFrame::parse(r#"{"v":1,"type":"telemetry","x":1}"#).is_none());
        assert!(LinkFrame::parse("not json").is_none());
        assert!(LinkFrame::parse("[]").is_none());
    }

    #[test]
    fn sends_frames_in_the_shape_the_plugin_parses() {
        let json = serde_json::to_value(DesktopFrame::welcome("AGI Mizar 0.1.0")).unwrap();
        assert_eq!(json["type"], "welcome");
        assert_eq!(json["linkVersion"], 1);

        let json = serde_json::to_value(DesktopFrame::stream("c1", "session/follow", serde_json::json!({}))).unwrap();
        assert_eq!(json["type"], "invoke");
        assert_eq!(json["mode"], "stream");

        let json = serde_json::to_value(DesktopFrame::approval_answer("a1", "allowed-once")).unwrap();
        assert_eq!(json["type"], "answer");
        assert_eq!(json["outcome"], "allowed-once");
        assert!(json.get("answers").is_none());

        let json = serde_json::to_value(DesktopFrame::Filter {
            v: LINK_VERSION,
            drop: vec!["assistant/attempt".into()],
        })
        .unwrap();
        assert_eq!(json["drop"][0], "assistant/attempt");
    }
}
