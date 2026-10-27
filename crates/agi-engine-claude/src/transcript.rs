//! Reading a Claude Code transcript.
//!
//! Claude Code writes one JSONL file per session. There is no protocol to speak
//! and no server to ask: the file *is* the record, appended as the session runs.
//! So this module is a parser, and the only interesting decisions are about what
//! counts as a turn and what counts as a message.
//!
//! ### What delimits a turn
//!
//! A turn begins at a **human prompt**: a `user` record whose content carries
//! text and no tool result, and which is not a sidechain. Everything between one
//! such record and the next belongs to that turn — assistant messages, tool
//! calls, tool results, and Claude Code's own bookkeeping. Nothing in the file
//! says "turn 3"; the count is derived, and deriving it from the prompt is the
//! only rule that survives compaction and resumed sessions.
//!
//! ### Why the sequence number is a byte offset
//!
//! A record needs a stable `seq` that does not shift when the file grows, because
//! "what have I already read" is compared against it. Line numbers shift on
//! rewrite; timestamps collide at millisecond resolution and are missing from
//! some records (`custom-title`, `cost-state`). The byte offset of the line does
//! not move. `seq = offset * 10 + slot`, where the slot orders records that share
//! a line — a prompt line can carry both the turn marker and the message.
//!
//! ### What is deliberately not here
//!
//! Nothing is filtered for size. A transcript is read from a local file, so the
//! bandwidth argument that shaped the phone protocol does not apply; the caller
//! asks for a window.

use agi_protocol::domain::{Record, RecordKind, ToolInfo};
use serde_json::Value;

/// A parsed transcript: the records, plus what the session list needs.
#[derive(Debug, Clone, Default)]
pub struct ParsedTranscript {
    /// Oldest first.
    pub records: Vec<Record>,
    /// The operator's own title, else the first prompt.
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    /// How many turns the file contains.
    pub turns: u32,
}

/// One turn being assembled.
struct TurnAccum {
    number: u32,
    /// Sequence of the `turn/start` record.
    start_seq: i64,
    start_ms: i64,
    /// Latest timestamp seen in this turn, for the end marker.
    last_ms: i64,
    /// Sequence and byte offset of the turn's last record, for the end marker.
    last_seq: i64,
    /// The last thing the model said in this turn.
    last_assistant: Option<(i64, String)>,
}

/// Parse a transcript.
///
/// `running` says whether the newest turn is still open: an open turn gets no end
/// marker, which is what lets the UI draw it as running instead of finished.
pub fn parse(text: &str, running: bool) -> ParsedTranscript {
    let mut out = ParsedTranscript::default();
    let mut turns: Vec<TurnAccum> = Vec::new();
    let mut offset = 0usize;
    let mut custom_title: Option<String> = None;

    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            // A half-written last line is normal while a session runs.
            continue;
        };
        let seq_base = start as i64 * 10;
        let time = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_ms)
            .unwrap_or(0);

        if out.cwd.is_none() {
            if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
                out.cwd = Some(cwd.to_string());
            }
        }

        // Claude Code's own title record wins over a guessed one.
        if value.get("type").and_then(Value::as_str) == Some("custom-title") {
            if let Some(title) = value.get("customTitle").and_then(Value::as_str) {
                if !title.trim().is_empty() {
                    custom_title = Some(title.trim().to_string());
                }
            }
            continue;
        }

        if is_human_prompt(&value) {
            let number = turns.len() as u32 + 1;
            let text = message_text(&value);
            if out.title.is_none() {
                out.title = text.as_deref().and_then(title_of);
            }
            turns.push(TurnAccum {
                number,
                start_seq: seq_base,
                start_ms: time,
                last_ms: time,
                last_seq: seq_base,
                last_assistant: None,
            });
            if let Some(text) = text {
                out.records.push(Record {
                    seq: seq_base + 1,
                    time,
                    turn: Some(number),
                    step: None,
                    kind: RecordKind::UserMessage,
                    text: Some(text),
                    tool: None,
                });
            }
            continue;
        }

        // Everything below belongs to the turn in progress.
        let Some(turn) = turns.last_mut() else {
            continue;
        };
        if time > 0 {
            turn.last_ms = time;
            turn.last_seq = seq_base;
        }

        match value.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let message = value.get("message");
                if let Some(model) = message
                    .and_then(|message| message.get("model"))
                    .and_then(Value::as_str)
                {
                    out.model = Some(model.to_string());
                }

                let mut slot = 2;
                for block in blocks(message) {
                    match block.get("type").and_then(Value::as_str) {
                        Some("tool_use") => {
                            out.records.push(Record {
                                seq: seq_base + slot,
                                time,
                                turn: Some(turn.number),
                                step: None,
                                kind: RecordKind::ToolCall,
                                text: None,
                                tool: Some(ToolInfo {
                                    call_id: block
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                    name: block
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or("tool")
                                        .to_string(),
                                    arguments: block
                                        .get("input")
                                        .map(|input| input.to_string())
                                        .filter(|text| text != "null"),
                                    output: None,
                                }),
                            });
                            slot += 1;
                        }
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                if !text.trim().is_empty() {
                                    // Only the last thing said in a turn becomes
                                    // the turn's message; earlier text is the
                                    // model narrating between tool calls.
                                    turn.last_assistant = Some((seq_base + 1, text.to_string()));
                                }
                            }
                        }
                        // `thinking` is not something the model said to anybody.
                        _ => {}
                    }
                }
            }
            Some("user") => {
                // Tool results arrive as user-role records. They are the other
                // half of a call, not a message.
                for block in blocks(value.get("message")) {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let output = block.get("content").and_then(|content| match content {
                        Value::String(text) => Some(text.clone()),
                        Value::Array(parts) => Some(
                            parts
                                .iter()
                                .filter_map(|part| part.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        _ => None,
                    });
                    out.records.push(Record {
                        seq: seq_base + 2,
                        time,
                        turn: Some(turn.number),
                        step: None,
                        kind: RecordKind::ToolResult,
                        text: None,
                        tool: Some(ToolInfo {
                            call_id: block
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            name: String::new(),
                            arguments: None,
                            output,
                        }),
                    });
                }
            }
            _ => {}
        }
    }

    // Turn markers, once the whole file is known: a turn ends when the next one
    // began, or when the session is not running and this is the newest one.
    let count = turns.len();
    for (index, turn) in turns.iter().enumerate() {
        out.records.push(Record {
            seq: turn.start_seq,
            time: turn.start_ms,
            turn: Some(turn.number),
            step: None,
            kind: RecordKind::TurnStart,
            text: None,
            tool: None,
        });

        let is_newest = index + 1 == count;
        let open = is_newest && running;
        if open {
            continue;
        }
        if let Some((seq, text)) = &turn.last_assistant {
            out.records.push(Record {
                seq: *seq,
                time: turn.last_ms,
                turn: Some(turn.number),
                step: None,
                kind: RecordKind::AssistantMessage,
                text: Some(text.clone()),
                tool: None,
            });
        }
        out.records.push(Record {
            seq: turn.last_seq * 10 + 9,
            time: turn.last_ms,
            turn: Some(turn.number),
            step: None,
            kind: RecordKind::TurnEnd,
            text: None,
            tool: None,
        });
    }

    out.turns = count as u32;
    out.title = custom_title.or(out.title);
    out.records.sort_by_key(|record| record.seq);
    out
}

/// Whether a record is a prompt a person typed.
///
/// Three kinds of record look like one and are not: a tool result (written as a
/// user-role message), a sidechain (a subagent's own conversation), and an
/// `isMeta` record — the runtime reminders and skill content Claude Code injects.
/// Measured on a real home: the first `user` line of a session started by a skill
/// is 900 characters of skill text, and taking it as the title makes every such
/// session look identical.
pub fn is_human_prompt(value: &Value) -> bool {
    if value.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    let message = value.get("message");
    if message
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        != Some("user")
    {
        return false;
    }

    // The older form is a plain string; resumed sessions still contain it.
    if let Some(text) = message
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
    {
        return !text.trim().is_empty();
    }

    let mut has_text = false;
    for block in blocks(message) {
        match block.get("type").and_then(Value::as_str) {
            // A tool result travels in a user-role record; it is not a prompt.
            Some("tool_result") => return false,
            Some("text") | Some("image") => has_text = true,
            _ => {}
        }
    }
    has_text
}

/// The content blocks of a message.
fn blocks(message: Option<&Value>) -> Vec<&Value> {
    message
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .map(|parts| parts.iter().collect())
        .unwrap_or_default()
}

/// The readable text of a message, if any.
fn message_text(value: &Value) -> Option<String> {
    if let Some(text) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
    {
        return (!text.trim().is_empty()).then(|| text.to_string());
    }
    let text = blocks(value.get("message"))
        .into_iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Whether a prompt is a wrapper Claude Code or the harness generated.
///
/// Slash commands arrive as `<command-message>…</command-message>`, background
/// jobs as `<task-notification>…`, local output as `<local-command-stdout>`.
/// They are real turns — the operator did something — but they are not a title:
/// every one of them would read the same, and none of them says what the session
/// is about.
pub fn looks_injected(text: &str) -> bool {
    text.trim_start().starts_with('<')
}

/// A one-line title from a prompt, or `None` when it would not describe anything.
pub fn title_of(text: &str) -> Option<String> {
    if looks_injected(text) {
        return None;
    }
    Some(short_title(text))
}

/// A one-line title from a prompt.
fn short_title(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= 60 {
        return line.to_string();
    }
    let mut shortened: String = line.chars().take(57).collect();
    shortened.push('…');
    shortened
}

/// Parse the timestamps Claude Code writes: `2026-10-27T05:23:06.355Z`.
///
/// Written by hand rather than pulled from a date crate: this is the only date
/// arithmetic in the workspace, and the format is fixed.
pub fn parse_iso8601_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, rest) = text.split_once('T')?;
    let rest = rest.strip_suffix('Z').unwrap_or(rest);

    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;

    let (clock, fraction) = match rest.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (rest, None),
    };
    let mut clock_parts = clock.split(':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    let second: i64 = clock_parts.next()?.parse().ok()?;

    // `355` means 355 ms; `35` means 350 ms.
    let millis = match fraction {
        Some(digits) => {
            let digits: String = digits.chars().take(3).collect();
            let padded = format!("{digits:0<3}");
            padded.parse::<i64>().ok()?
        }
        None => 0,
    };

    let days = days_from_civil(year, month, day);
    Some(days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1_000 + millis)
}

/// Days since 1970-01-01 for a civil date.
///
/// Howard Hinnant's algorithm, which is exact for every date a transcript can
/// carry and has no leap-second or leap-year table to get wrong.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// One transcript line, as Claude Code writes it.
    fn line(value: Value) -> String {
        format!("{value}\n")
    }

    fn prompt(text: &str, at: &str) -> String {
        line(json!({
            "type": "user",
            "uuid": "u1",
            "timestamp": at,
            "sessionId": "s1",
            "cwd": "/tmp/work",
            "message": { "role": "user", "content": [{ "type": "text", "text": text }] }
        }))
    }

    fn assistant(blocks: Value, at: &str) -> String {
        line(json!({
            "type": "assistant",
            "uuid": "a1",
            "timestamp": at,
            "message": { "role": "assistant", "model": "claude-opus-4", "content": blocks }
        }))
    }

    #[test]
    fn parses_the_timestamps_claude_writes() {
        // 2026-10-27T05:23:06.355Z
        let parsed = parse_iso8601_ms("2026-10-27T05:23:06.355Z").unwrap();
        assert_eq!(parsed, 1_793_078_586_355);
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601_ms("2026-10-27T05:23:06Z").map(|ms| ms % 1000), Some(0));
        assert_eq!(parse_iso8601_ms("nonsense"), None);
    }

    #[test]
    fn one_prompt_is_one_turn() {
        let text = format!(
            "{}{}",
            prompt("hello", "2026-10-27T05:00:00Z"),
            assistant(json!([{ "type": "text", "text": "hi" }]), "2026-10-27T05:00:05Z")
        );
        let parsed = parse(&text, false);
        let kinds: Vec<RecordKind> = parsed.records.iter().map(|record| record.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RecordKind::TurnStart,
                RecordKind::UserMessage,
                RecordKind::AssistantMessage,
                RecordKind::TurnEnd,
            ]
        );
        assert_eq!(parsed.turns, 1);
        assert_eq!(parsed.title.as_deref(), Some("hello"));
        assert_eq!(parsed.cwd.as_deref(), Some("/tmp/work"));
        assert_eq!(parsed.model.as_deref(), Some("claude-opus-4"));
    }

    #[test]
    fn a_running_session_has_no_end_marker_on_its_newest_turn() {
        let text = prompt("go", "2026-10-27T05:00:00Z");
        let parsed = parse(&text, true);
        assert!(parsed
            .records
            .iter()
            .all(|record| record.kind != RecordKind::TurnEnd));

        // The same file, no longer running, closes the turn.
        let parsed = parse(&text, false);
        assert_eq!(
            parsed.records.last().map(|record| record.kind),
            Some(RecordKind::TurnEnd)
        );
    }

    #[test]
    fn a_tool_result_is_not_a_prompt() {
        let text = format!(
            "{}{}{}",
            prompt("list files", "2026-10-27T05:00:00Z"),
            assistant(
                json!([{ "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": { "command": "ls" } }]),
                "2026-10-27T05:00:02Z"
            ),
            line(json!({
                "type": "user",
                "timestamp": "2026-10-27T05:00:03Z",
                "message": {
                    "role": "user",
                    "content": [{ "type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt" }]
                }
            }))
        );
        let parsed = parse(&text, false);
        // One prompt, therefore one turn.
        assert_eq!(parsed.turns, 1);
        let calls: Vec<&Record> = parsed
            .records
            .iter()
            .filter(|record| record.kind == RecordKind::ToolCall)
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool.as_ref().unwrap().name, "Bash");
        let results: Vec<&Record> = parsed
            .records
            .iter()
            .filter(|record| record.kind == RecordKind::ToolResult)
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool.as_ref().unwrap().output.as_deref(), Some("a.txt"));
    }

    #[test]
    fn only_the_last_text_of_a_turn_is_its_message() {
        let text = format!(
            "{}{}{}",
            prompt("do it", "2026-10-27T05:00:00Z"),
            assistant(json!([{ "type": "text", "text": "starting" }]), "2026-10-27T05:00:01Z"),
            assistant(json!([{ "type": "text", "text": "done" }]), "2026-10-27T05:00:09Z")
        );
        let parsed = parse(&text, false);
        let messages: Vec<&Record> = parsed
            .records
            .iter()
            .filter(|record| record.kind == RecordKind::AssistantMessage)
            .collect();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text.as_deref(), Some("done"));
    }

    #[test]
    fn a_custom_title_wins_over_the_first_prompt() {
        let text = format!(
            "{}{}",
            prompt("an extremely long opening request", "2026-10-27T05:00:00Z"),
            line(json!({ "type": "custom-title", "customTitle": "Parser work" }))
        );
        let parsed = parse(&text, false);
        assert_eq!(parsed.title.as_deref(), Some("Parser work"));
    }

    #[test]
    fn sequence_numbers_are_ordered_and_unique() {
        let text = format!(
            "{}{}{}",
            prompt("one", "2026-10-27T05:00:00Z"),
            assistant(json!([{ "type": "text", "text": "a" }]), "2026-10-27T05:00:01Z"),
            prompt("two", "2026-10-27T05:01:00Z")
        );
        let parsed = parse(&text, false);
        let mut seqs: Vec<i64> = parsed.records.iter().map(|record| record.seq).collect();
        let sorted = {
            let mut copy = seqs.clone();
            copy.sort_unstable();
            copy
        };
        assert_eq!(seqs, sorted);
        seqs.dedup();
        assert_eq!(seqs.len(), parsed.records.len());
    }

    #[test]
    fn a_truncated_last_line_is_ignored() {
        let text = format!("{}{{\"type\":\"assis", prompt("hi", "2026-10-27T05:00:00Z"));
        let parsed = parse(&text, true);
        assert_eq!(parsed.turns, 1);
        assert!(parsed
            .records
            .iter()
            .any(|record| record.kind == RecordKind::UserMessage));
    }

    #[test]
    fn a_wrapped_command_is_a_turn_but_not_a_title() {
        let text = format!(
            "{}{}",
            prompt("<command-message>performance-audit</command-message>", "2026-10-27T05:00:00Z"),
            prompt("actually look at the sidebar", "2026-10-27T05:01:00Z")
        );
        let parsed = parse(&text, false);
        assert_eq!(parsed.turns, 2, "a slash command is still a turn");
        assert_eq!(parsed.title.as_deref(), Some("actually look at the sidebar"));
    }

    #[test]
    fn injected_context_is_not_a_turn() {
        // A skill's content arrives as a user message; it is not a prompt.
        let text = line(json!({
            "type": "user",
            "isMeta": true,
            "timestamp": "2026-10-27T05:00:00Z",
            "message": { "role": "user", "content": [{ "type": "text", "text": "Base directory for this skill: …" }] }
        }));
        let parsed = parse(&text, false);
        assert_eq!(parsed.turns, 0);
        assert!(parsed.title.is_none());
    }

    #[test]
    fn a_string_content_prompt_still_counts() {
        let text = line(json!({
            "type": "user",
            "timestamp": "2026-10-27T05:00:00Z",
            "message": { "role": "user", "content": "fix the build" }
        }));
        let parsed = parse(&text, false);
        assert_eq!(parsed.turns, 1);
        assert_eq!(parsed.title.as_deref(), Some("fix the build"));
    }

    #[test]
    fn a_sidechain_is_not_a_turn() {
        let text = line(json!({
            "type": "user",
            "isSidechain": true,
            "timestamp": "2026-10-27T05:00:00Z",
            "message": { "role": "user", "content": [{ "type": "text", "text": "subagent prompt" }] }
        }));
        let parsed = parse(&text, false);
        assert_eq!(parsed.turns, 0);
        assert!(parsed.records.is_empty());
    }
}
