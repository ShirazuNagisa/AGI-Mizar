//! Turning Codex threads and turns into the shared model.
//!
//! Codex's shape is a thread holding turns holding items. The shared model is a
//! flat, ordered list of records, so this module is where a tree becomes a
//! transcript.
//!
//! ### Sequence numbers
//!
//! Codex gives a turn an `id` and a `startedAt` in **seconds** — coarse enough
//! that two turns can share one. A transcript needs a stable, unique `seq` per
//! record, and it must not shift when a later turn is appended (that is what a
//! "what did I already read" cursor is compared against). So the sequence is
//! derived from the turn's position and the item's position inside it:
//!
//! ```text
//! turn 1  turn/start     1_000_000
//!         user message   1_000_010
//!         tool call      1_000_020
//!         turn/end       1_001_999
//! turn 2  turn/start     2_000_000
//! ```
//!
//! Stable across reads, unique within a thread, and obviously ordered.

use agi_protocol::domain::{Record, RecordKind, Session, SessionKey, ToolInfo};
use agi_protocol::engine::EngineId;
use serde_json::Value;

/// How far into a turn's band the closing record sits.
const TURN_END_OFFSET: i64 = 999_999;

/// Items that mean "the model did something", for step counting and tool rows.
const TOOL_ITEMS: [&str; 8] = [
    "commandExecution",
    "fileChange",
    "mcpToolCall",
    "dynamicToolCall",
    "collabAgentToolCall",
    "subAgentActivity",
    "webSearch",
    "imageView",
];

/// Read one `thread/list` row into the shared model.
///
/// Timestamps arrive in seconds and leave in milliseconds: everything above this
/// layer — sorting, the API, the UI — works in one unit.
pub fn session_from_thread(thread: &Value) -> Option<Session> {
    let id = thread.get("id").and_then(Value::as_str)?;
    if id.is_empty() {
        return None;
    }
    let seconds = |key: &str| thread.get(key).and_then(Value::as_i64);
    let updated_at = seconds("updatedAt")
        .or_else(|| seconds("createdAt"))
        .unwrap_or(0)
        * 1_000;

    let title = thread
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .or_else(|| thread.get("preview").and_then(Value::as_str))
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());

    Some(Session {
        key: SessionKey::new(EngineId::Codex, id),
        cwd: thread
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string),
        title,
        model: thread
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        updated_at,
        // Codex's own status is authoritative for threads this server has loaded.
        // `notLoaded` says nothing about a turn another window is running, and
        // guessing from the rollout file is a file-format dependency this layer
        // does not need.
        running: thread
            .get("status")
            .and_then(|status| status.get("type"))
            .and_then(Value::as_str)
            == Some("active"),
        parent: thread
            .get("parentThreadId")
            .and_then(Value::as_str)
            .map(str::to_string),
        origin: thread
            .get("forkedFromId")
            .and_then(Value::as_str)
            .map(|_| "fork".to_string()),
        turns: thread
            .get("turns")
            .and_then(Value::as_array)
            .map(|turns| turns.len() as u32)
            .unwrap_or(0),
        // Codex reports usage per turn, not a live window reading.
        context: None,
    })
}

/// Flatten a thread's turns into records, oldest first.
///
/// `turns` must already be in chronological order — `thread/turns/list` answers
/// newest-first and the caller reverses it.
pub fn records_from_turns(turns: &[Value]) -> Vec<Record> {
    let mut records = Vec::new();

    for (index, turn) in turns.iter().enumerate() {
        let turn_number = (index + 1) as u32;
        let base = (index as i64 + 1) * 1_000_000;
        let started_ms = turn.get("startedAt").and_then(Value::as_i64).unwrap_or(0) * 1_000;

        records.push(Record {
            seq: base,
            time: started_ms,
            turn: Some(turn_number),
            step: None,
            kind: RecordKind::TurnStart,
            text: None,
            tool: None,
        });

        let items = turn
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut last_assistant: Option<(i64, String)> = None;
        let mut step = 0u32;

        for (item_index, item) in items.iter().enumerate() {
            let seq = base + 10 + item_index as i64 * 10;
            let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();

            match kind {
                "userMessage" => {
                    if let Some(text) = content_text(item.get("content")) {
                        records.push(Record {
                            seq,
                            time: started_ms,
                            turn: Some(turn_number),
                            step: None,
                            kind: RecordKind::UserMessage,
                            text: Some(text),
                            tool: None,
                        });
                    }
                }
                // Only the last thing the model said in a finished turn is the
                // turn's message; earlier ones are the model thinking out loud
                // between tool calls.
                "agentMessage" => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        if !text.trim().is_empty() {
                            last_assistant = Some((seq, text.to_string()));
                        }
                    }
                }
                other if TOOL_ITEMS.contains(&other) => {
                    step += 1;
                    records.push(Record {
                        seq,
                        time: started_ms,
                        turn: Some(turn_number),
                        step: Some(step),
                        kind: RecordKind::ToolCall,
                        text: None,
                        tool: Some(tool_of(other, item)),
                    });
                    if let Some(output) = tool_output(item) {
                        records.push(Record {
                            seq: seq + 1,
                            time: started_ms,
                            turn: Some(turn_number),
                            step: Some(step),
                            kind: RecordKind::ToolResult,
                            text: None,
                            tool: Some(ToolInfo {
                                call_id: call_id(item),
                                name: other.to_string(),
                                arguments: None,
                                output: Some(output),
                            }),
                        });
                    }
                }
                // `reasoning` and anything new: the raw item is still on the wire
                // through `thread/read`, and a row per thought is noise.
                _ => {}
            }
        }

        let status = turn
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("completed");
        if status != "inProgress" {
            if let Some((seq, text)) = last_assistant {
                records.push(Record {
                    seq,
                    time: started_ms,
                    turn: Some(turn_number),
                    step: None,
                    kind: RecordKind::AssistantMessage,
                    text: Some(text),
                    tool: None,
                });
            } else if status == "failed" {
                // A turn whose model never ran has no message. Rendering nothing
                // makes a failure indistinguishable from a hang.
                let message = turn
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("the turn failed");
                records.push(Record {
                    seq: base + 900_000,
                    time: started_ms,
                    turn: Some(turn_number),
                    step: None,
                    kind: RecordKind::AssistantMessage,
                    text: Some(message.to_string()),
                    tool: None,
                });
            }

            records.push(Record {
                seq: base + TURN_END_OFFSET,
                time: started_ms,
                turn: Some(turn_number),
                step: None,
                kind: RecordKind::TurnEnd,
                text: Some(status.to_string()),
                tool: None,
            });
        }
    }

    records
}

/// Join the `text` parts of a content array.
fn content_text(parts: Option<&Value>) -> Option<String> {
    let parts = parts?.as_array()?;
    let joined = parts
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

/// The command a tool item ran, or a one-line description.
fn tool_of(kind: &str, item: &Value) -> ToolInfo {
    let arguments = match kind {
        "commandExecution" => item.get("command").map(|command| match command {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            other => other.to_string(),
        }),
        _ => item
            .get("arguments")
            .and_then(Value::as_str)
            .map(str::to_string),
    };

    ToolInfo {
        call_id: call_id(item),
        name: match kind {
            "commandExecution" => "shell".to_string(),
            "fileChange" => "apply_patch".to_string(),
            other => item
                .get("tool")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(other)
                .to_string(),
        },
        arguments,
        output: None,
    }
}

/// A tool item's captured output, when it has one.
fn tool_output(item: &Value) -> Option<String> {
    for key in ["aggregatedOutput", "output", "stdout", "result"] {
        if let Some(text) = item.get(key).and_then(Value::as_str) {
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

/// The item id, which pairs a call with its result.
fn call_id(item: &Value) -> String {
    item.get("id")
        .or_else(|| item.get("callId"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_a_thread_row() {
        let session = session_from_thread(&json!({
            "id": "th_1",
            "name": "Fix the parser",
            "cwd": "/tmp/work",
            "model": "gpt-5",
            "updatedAt": 1_700_000_000i64,
            "status": { "type": "active" }
        }))
        .unwrap();
        assert_eq!(session.key.to_string(), "codex:th_1");
        assert_eq!(session.title.as_deref(), Some("Fix the parser"));
        assert_eq!(session.updated_at, 1_700_000_000_000);
        assert!(session.running);
    }

    #[test]
    fn falls_back_from_name_to_preview() {
        let session = session_from_thread(&json!({
            "id": "th_2", "name": "  ", "preview": "what is this", "status": { "type": "idle" }
        }))
        .unwrap();
        assert_eq!(session.title.as_deref(), Some("what is this"));
        assert!(!session.running);
    }

    #[test]
    fn flattens_a_turn_into_ordered_records() {
        let turns = vec![json!({
            "id": "t1", "status": "completed", "startedAt": 1_700_000_000i64,
            "items": [
                { "type": "userMessage", "content": [{ "type": "text", "text": "list the files" }] },
                { "type": "commandExecution", "id": "c1", "command": "ls", "aggregatedOutput": "a.txt" },
                { "type": "agentMessage", "text": "one file" }
            ]
        })];

        let records = records_from_turns(&turns);
        let kinds: Vec<RecordKind> = records.iter().map(|record| record.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RecordKind::TurnStart,
                RecordKind::UserMessage,
                RecordKind::ToolCall,
                RecordKind::ToolResult,
                RecordKind::AssistantMessage,
                RecordKind::TurnEnd,
            ]
        );

        // Sequence numbers are ordered and unique.
        let mut seqs: Vec<i64> = records.iter().map(|record| record.seq).collect();
        let sorted = seqs.clone();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs, sorted, "seq must be ordered");
        assert_eq!(seqs.len(), records.len(), "seq must be unique");

        assert_eq!(records[1].text.as_deref(), Some("list the files"));
        assert_eq!(records[2].tool.as_ref().unwrap().name, "shell");
        assert_eq!(
            records[3].tool.as_ref().unwrap().output.as_deref(),
            Some("a.txt")
        );
        assert_eq!(records[4].text.as_deref(), Some("one file"));
    }

    #[test]
    fn only_the_last_agent_message_is_the_turn_message() {
        let turns = vec![json!({
            "id": "t1", "status": "completed", "startedAt": 1i64,
            "items": [
                { "type": "agentMessage", "text": "thinking" },
                { "type": "commandExecution", "id": "c1", "command": "ls" },
                { "type": "agentMessage", "text": "done" }
            ]
        })];
        let records = records_from_turns(&turns);
        let assistant: Vec<&Record> = records
            .iter()
            .filter(|record| record.kind == RecordKind::AssistantMessage)
            .collect();
        assert_eq!(assistant.len(), 1);
        assert_eq!(assistant[0].text.as_deref(), Some("done"));
    }

    #[test]
    fn a_running_turn_has_no_end_record() {
        let turns = vec![json!({
            "id": "t1", "status": "inProgress", "startedAt": 1i64,
            "items": [{ "type": "userMessage", "content": [{ "type": "text", "text": "go" }] }]
        })];
        let records = records_from_turns(&turns);
        assert!(records
            .iter()
            .all(|record| record.kind != RecordKind::TurnEnd));
    }

    #[test]
    fn a_failed_turn_without_a_message_says_why() {
        let turns = vec![json!({
            "id": "t1", "status": "failed", "startedAt": 1i64,
            "error": { "message": "model refused" },
            "items": []
        })];
        let records = records_from_turns(&turns);
        let message = records
            .iter()
            .find(|record| record.kind == RecordKind::AssistantMessage)
            .unwrap();
        assert_eq!(message.text.as_deref(), Some("model refused"));
    }
}
