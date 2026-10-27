//! Turning DSH's log records into the shared model.
//!
//! One function, one direction: a `session/event` payload goes in, a
//! [`Projected`] comes out or nothing does. Keeping it pure is what makes the
//! interesting cases testable without a DSH host — and there are interesting
//! cases, all of them learned the hard way in 0.8.x:
//!
//! - **A user message is not always a person.** DSH injects runtime context and
//!   the skill catalogue through the same `user/message` event. Measured on a
//!   live session, an eleven-character prompt arrived with 483 characters of
//!   runtime context and 5921 of skill catalogue attached. Those are dropped.
//! - **An assistant message's parts are one level deeper than a user's**:
//!   `data.message.content` rather than `data.content`. Reading the wrong one is
//!   how every reply once went missing while the computer showed the exchange.
//! - **A step that only called tools has no text part at all.** That is not an
//!   empty message; it is a step with nothing to say.
//!
//! Anything this module does not recognise is dropped rather than guessed at.
//! The raw payload is still on the wire, so a field this build has never heard of
//! costs a UI nicety, not correctness.

use agi_protocol::domain::{Record, RecordKind, ToolInfo};
use serde_json::Value;

/// One event, narrowed to what the shared model can carry.
#[derive(Debug, Clone, PartialEq)]
pub struct Projected {
    /// The DSH sequence number within the session.
    pub seq: i64,
    /// Epoch milliseconds.
    pub time: i64,
    pub kind: RecordKind,
    pub turn: Option<u32>,
    pub step: Option<u32>,
    pub text: Option<String>,
    pub tool: Option<ToolInfo>,
    /// Set when this event changes whether a turn is running.
    pub running: Option<bool>,
}

impl Projected {
    /// The record to append, when this event is worth a row in a conversation.
    pub fn record(&self) -> Option<Record> {
        match self.kind {
            // Step markers are bookkeeping; the UI counts steps from the
            // messages around them, and a row per step is noise.
            RecordKind::Step => None,
            _ => Some(Record {
                seq: self.seq,
                time: self.time,
                turn: self.turn,
                step: self.step,
                kind: self.kind,
                text: self.text.clone(),
                tool: self.tool.clone(),
            }),
        }
    }
}

/// Read a string field.
fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_string)
}

/// Read a non-negative integer field as `u32`.
fn number(value: &Value, key: &str) -> Option<u32> {
    value.get(key)?.as_u64().and_then(|n| u32::try_from(n).ok())
}

/// Read an integer field as `i64`.
fn integer(value: &Value, key: &str) -> Option<i64> {
    value.get(key)?.as_i64()
}

/// Join the `text` parts of a content array.
///
/// A part typed `reasoning` is not something the model said to anybody, and a
/// step that only called tools has no text part at all — both answer `None`,
/// which is the same call the renderer makes.
fn parts_text(parts: &Value) -> Option<String> {
    let array = parts.as_array()?;
    let joined = array
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Who really wrote a `user/message`.
///
/// Measured across twelve real session logs: `user/message` arrives with
/// **fourteen** different `source.kind` values, and only one of them is a person.
/// The rest are the harness talking to itself — runtime context, the skill
/// catalogue, sampled clocks, job notices, subagent settlements, model switches.
///
/// Treating them all as messages puts words in the operator's mouth: a transcript
/// full of `Time sampled while preparing turn 1, step 1: …` rendered as blue
/// bubbles they never typed. So the kinds are sorted into three answers rather
/// than one boolean:
///
/// - **Person** — `user`. Anything missing a kind counts here too: an older host
///   that does not send `source` is not evidence of injection.
/// - **Notice** — real events worth a grey line: a background job finishing, a
///   subagent reporting back, a permission policy change, a model switch, a
///   working-directory change.
/// - **Drop** — bookkeeping nobody reads: sampled clocks, the runtime snapshot,
///   the skill catalogue, injected instructions, compaction checkpoints, goal
///   scaffolding.
enum Authorship {
    Person,
    Notice,
    Drop,
}

/// Classify one `user/message` by its source kind.
fn authorship(data: &Value) -> Authorship {
    let kind = data
        .get("source")
        .and_then(|source| source.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("user");

    match kind {
        "user" => Authorship::Person,
        // Noise: high volume, zero information per line.
        "time-context" | "runtime-context" | "skill-catalog" | "agent-instructions"
        | "compact-checkpoint" | "goal" | "tool-goal" => Authorship::Drop,
        // Everything else is a real event with a short, useful text.
        _ => Authorship::Notice,
    }
}

/// The `{seq, time, type, data}` record inside a forwarded payload.
///
/// The plugin forwards `{header, event}`; a build that forwarded the record
/// itself is read as the record, so one wire change cannot silently empty every
/// conversation.
fn event_of(payload: &Value) -> Option<&Value> {
    match payload.get("event") {
        Some(event) if event.is_object() => Some(event),
        _ if payload.get("type").is_some() => Some(payload),
        _ => None,
    }
}

/// Project one forwarded `session/event` payload.
///
/// Returns `None` for records the shared model has no place for: system prompts,
/// request headers, retry bookkeeping, and streaming attempts that never
/// committed a message.
pub fn project(payload: &Value) -> Option<Projected> {
    let event = event_of(payload)?;
    let kind = event.get("type")?.as_str()?;
    let data = event.get("data").unwrap_or(&Value::Null);
    let seq = integer(event, "seq").unwrap_or(0);
    let time = integer(event, "time").unwrap_or(0);
    let turn = number(data, "turn");
    let step = number(data, "step");

    let base = Projected {
        seq,
        time,
        kind: RecordKind::Notice,
        turn,
        step,
        text: None,
        tool: None,
        running: None,
    };

    match kind {
        "turn/start" => Some(Projected {
            kind: RecordKind::TurnStart,
            running: Some(true),
            ..base
        }),
        "turn/end" => Some(Projected {
            kind: RecordKind::TurnEnd,
            running: Some(false),
            text: data
                .get("reason")
                .and_then(|reason| reason.get("kind"))
                .and_then(Value::as_str)
                .map(str::to_string),
            ..base
        }),
        "step/start" | "step/end" => Some(Projected {
            kind: RecordKind::Step,
            ..base
        }),
        "user/message" => {
            let text = parts_text(data.get("content")?)?;
            match authorship(data) {
                Authorship::Drop => None,
                Authorship::Person => Some(Projected {
                    kind: RecordKind::UserMessage,
                    text: Some(text),
                    ..base
                }),
                Authorship::Notice => Some(Projected {
                    kind: RecordKind::Notice,
                    text: Some(text),
                    ..base
                }),
            }
        }
        "assistant/message" => {
            let message = data.get("message")?;
            let text = parts_text(message.get("content")?)?;
            Some(Projected {
                kind: RecordKind::AssistantMessage,
                text: Some(text),
                ..base
            })
        }
        "tool/call" => Some(Projected {
            kind: RecordKind::ToolCall,
            tool: Some(ToolInfo {
                call_id: text(data, "callId").unwrap_or_default(),
                name: text(data, "name").unwrap_or_else(|| "tool".to_string()),
                arguments: text(data, "arguments"),
                output: None,
            }),
            ..base
        }),
        "tool/result" => Some(Projected {
            kind: RecordKind::ToolResult,
            tool: Some(ToolInfo {
                call_id: text(data, "callId").unwrap_or_default(),
                name: String::new(),
                arguments: None,
                output: result_text(data),
            }),
            ..base
        }),
        // `assistant/attempt` is a stream that committed no message, and the rest
        // are request plumbing. Both are reachable through the raw payload; they
        // are not conversation.
        _ => None,
    }
}

/// The four buckets DSH reports for one session's usage.
///
/// They are disjoint: billed input is the sum of the first three, and output is
/// billed separately. Both readers of these numbers — the window's usage line
/// and the phone's usage frame — go through here so a rename cannot make one of
/// them quietly report zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TokenParts {
    pub uncached: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

impl TokenParts {
    /// Everything billed as input.
    pub fn billed_input(&self) -> u64 {
        self.uncached + self.cache_read + self.cache_write
    }
}

/// Read the buckets, or `None` when this is not a usage projection.
///
/// `uncachedInputTokens` and `outputTokens` are both required: without them
/// there is no usage to report. The cache buckets are genuinely optional — a
/// provider that does not cache never reports them.
pub(crate) fn token_parts(usage: &Value) -> Option<TokenParts> {
    let uncached = usage.get("uncachedInputTokens").and_then(Value::as_u64)?;
    let output = usage.get("outputTokens").and_then(Value::as_u64)?;
    Some(TokenParts {
        uncached,
        output,
        cache_read: usage
            .get("cacheReadTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write: usage
            .get("cacheWriteTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

/// Billed input and output tokens, read from the `tokenUsage` projection.
///
/// The projection's wire view is four disjoint buckets — `uncachedInputTokens`,
/// `cacheReadTokens`, `cacheWriteTokens` and `outputTokens` — and there is no
/// `inputTokens` key among them: that name belongs to the raw attempt records,
/// and is mapped into `uncachedInputTokens` before the projection is served.
/// The input figure an operator expects is the sum of the three, which is why
/// reading the one that sounds obvious showed a flat zero next to a correct
/// output count.
pub(crate) fn token_usage(usage: &Value) -> (u64, u64) {
    match token_parts(usage) {
        Some(parts) => (parts.billed_input(), parts.output),
        None => (0, 0),
    }
}

/// The readable part of a `tool/result` payload.
fn result_text(data: &Value) -> Option<String> {
    if let Some(found) = parts_text(data.get("content")?) {
        return Some(found);
    }
    for key in ["output", "text", "error"] {
        if let Some(found) = data.get(key).and_then(Value::as_str) {
            if !found.is_empty() {
                return Some(found.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build the payload shape the plugin sends.
    fn payload(event: Value) -> Value {
        json!({ "header": { "origin": "session" }, "event": event })
    }

    #[test]
    fn projects_a_turn_boundary_and_its_running_flag() {
        let start = project(&payload(
            json!({"type":"turn/start","seq":1,"time":10,"data":{"turn":1}}),
        ))
        .unwrap();
        assert_eq!(start.kind, RecordKind::TurnStart);
        assert_eq!(start.running, Some(true));
        assert_eq!(start.turn, Some(1));

        let end = project(&payload(json!({
            "type":"turn/end","seq":9,"time":20,
            "data":{"turn":1,"reason":{"kind":"success"}}
        })))
        .unwrap();
        assert_eq!(end.kind, RecordKind::TurnEnd);
        assert_eq!(end.running, Some(false));
        assert_eq!(end.text.as_deref(), Some("success"));
    }

    #[test]
    fn reads_a_user_message_from_its_content_parts() {
        let event = project(&payload(json!({
            "type":"user/message","seq":2,"time":11,
            "data":{"turn":1,"content":[{"type":"text","text":"hello"}]}
        })))
        .unwrap();
        assert_eq!(event.kind, RecordKind::UserMessage);
        assert_eq!(event.text.as_deref(), Some("hello"));
    }

    #[test]
    fn only_a_person_is_a_user_message() {
        let event = json!({
            "type":"user/message","seq":3,"time":12,
            "data":{"turn":1,"source":{"kind":"user"},"content":[{"type":"text","text":"hello"}]}
        });
        assert_eq!(
            project(&payload(event)).unwrap().kind,
            RecordKind::UserMessage
        );
    }

    #[test]
    fn a_message_without_a_source_counts_as_a_person() {
        let event = json!({
            "type":"user/message","seq":3,"time":12,
            "data":{"turn":1,"content":[{"type":"text","text":"hello"}]}
        });
        assert_eq!(
            project(&payload(event)).unwrap().kind,
            RecordKind::UserMessage
        );
    }

    #[test]
    fn drops_the_harness_talking_to_itself() {
        // Every one of these was measured in a real log, and every one of them
        // would otherwise render as something the operator typed.
        for kind in [
            "runtime-context",
            "skill-catalog",
            "time-context",
            "agent-instructions",
            "compact-checkpoint",
            "goal",
            "tool-goal",
        ] {
            let event = json!({
                "type":"user/message","seq":3,"time":12,
                "data":{"turn":1,"source":{"kind":kind},"content":[{"type":"text","text":"x"}]}
            });
            assert!(
                project(&payload(event)).is_none(),
                "{kind} should be dropped"
            );
        }
    }

    #[test]
    fn keeps_a_real_event_as_a_notice() {
        for kind in [
            "tool-jobs",
            "subagent-settled",
            "user-approval",
            "model-selection",
            "working-directory",
        ] {
            let event = json!({
                "type":"user/message","seq":3,"time":12,
                "data":{"turn":1,"source":{"kind":kind},"content":[{"type":"text","text":"x"}]}
            });
            let projected = project(&payload(event)).unwrap();
            assert_eq!(
                projected.kind,
                RecordKind::Notice,
                "{kind} should be a notice"
            );
            assert_eq!(projected.text.as_deref(), Some("x"));
        }
    }

    #[test]
    fn reads_an_assistant_message_one_level_deeper() {
        let event = project(&payload(json!({
            "type":"assistant/message","seq":4,"time":13,
            "data":{"turn":1,"step":1,"message":{"content":[{"type":"text","text":"done"}]}}
        })))
        .unwrap();
        assert_eq!(event.kind, RecordKind::AssistantMessage);
        assert_eq!(event.text.as_deref(), Some("done"));
    }

    #[test]
    fn an_assistant_step_with_no_text_is_not_a_message() {
        let event = json!({
            "type":"assistant/message","seq":5,"time":14,
            "data":{"turn":1,"step":1,"message":{"content":[{"type":"reasoning","text":"hmm"}]}}
        });
        assert!(project(&payload(event)).is_none());
    }

    #[test]
    fn projects_a_tool_call_and_its_result() {
        let call = project(&payload(json!({
            "type":"tool/call","seq":6,"time":15,
            "data":{"turn":1,"step":1,"callId":"c1","name":"bash","arguments":"{\"cmd\":\"ls\"}"}
        })))
        .unwrap();
        assert_eq!(call.kind, RecordKind::ToolCall);
        let tool = call.tool.unwrap();
        assert_eq!(tool.name, "bash");
        assert_eq!(tool.call_id, "c1");

        let result = project(&payload(json!({
            "type":"tool/result","seq":7,"time":16,
            "data":{"turn":1,"step":1,"callId":"c1","content":[{"type":"text","text":"a.txt"}]}
        })))
        .unwrap();
        assert_eq!(result.kind, RecordKind::ToolResult);
        assert_eq!(result.tool.unwrap().output.as_deref(), Some("a.txt"));
    }

    #[test]
    fn ignores_bookkeeping_and_streaming_attempts() {
        for event in [
            json!({"type":"request/header","seq":1,"time":1,"data":{}}),
            json!({"type":"assistant/attempt","seq":2,"time":2,"data":{"turn":1,"step":1,"stream":[]}}),
            json!({"type":"system/message","seq":3,"time":3,"data":{}}),
        ] {
            assert!(project(&payload(event)).is_none());
        }
    }

    #[test]
    fn accepts_a_bare_record_as_well_as_the_wrapped_payload() {
        let bare = json!({
            "type":"user/message","seq":2,"time":11,
            "data":{"turn":1,"content":[{"type":"text","text":"hi"}]}
        });
        assert_eq!(project(&bare).unwrap().text.as_deref(), Some("hi"));
    }

    #[test]
    fn step_markers_are_projected_but_not_appended_as_records() {
        let step = project(&payload(json!({
            "type":"step/start","seq":2,"time":11,"data":{"turn":1,"step":1}
        })))
        .unwrap();
        assert_eq!(step.kind, RecordKind::Step);
        assert!(step.record().is_none());
    }

    #[test]
    fn reads_usage_from_the_shape_dsh_actually_sends() {
        // Copied from a live `tokenUsage` projection: four keys, no `inputTokens`.
        let usage = json!({
            "uncachedInputTokens": 1200,
            "outputTokens": 18100,
            "cacheReadTokens": 900000,
            "cacheWriteTokens": 40,
        });
        assert_eq!(token_usage(&usage), (901_240, 18_100));
    }

    #[test]
    fn a_usage_projection_without_its_two_load_bearing_fields_is_no_usage() {
        // Half a frame is not a smaller frame: the app requires both, and the
        // same rule keeps the window's line from inventing a zero.
        assert_eq!(token_usage(&json!({"outputTokens": 7})), (0, 0));
        assert_eq!(token_usage(&json!({"cacheReadTokens": 5})), (0, 0));
        assert_eq!(token_usage(&json!({})), (0, 0));

        // Cache buckets, on the other hand, are genuinely optional.
        let parts = token_parts(&json!({"uncachedInputTokens": 10, "outputTokens": 2})).unwrap();
        assert_eq!(parts.billed_input(), 10);
        assert_eq!(parts.cache_read, 0);
    }
}
