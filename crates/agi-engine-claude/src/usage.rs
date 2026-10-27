//! Folding Claude Code's transcripts into the days the home page charts.
//!
//! Claude Code's transcript *is* its usage record: every `assistant` line was
//! one billed request and carries the provider's own `usage` object, so the
//! tokens here are what the account was charged, not an estimate from text
//! length.
//!
//! ### What counts as a message
//!
//! - A `user` line that carries only `tool_result` blocks is a tool's output
//!   echoed back, not something a person said, and does not count. Measured on a
//!   real home: 1687 of 1806 `user` lines are exactly that.
//! - An `assistant` line is one API response, and Claude Code writes one per
//!   content block, so a turn can be three lines. Counting them all as messages
//!   would triple the number; counting only the lines that carry a `text` block
//!   gives one row per reply, and the thinking/tool-call lines still contribute
//!   their tokens.
//!
//! ### Model attribution
//!
//! A `user` line has no model. Its day and the token totals do not need one, but
//! the per-model rows would gain an `unknown` row for every prompt if it were
//! left blank — and the most-used-model chart would then be dominated by the
//! operator rather than by the models. Each file's prompts are therefore
//! attributed to the model that file used most, which is what a session is in
//! practice.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agi_engine::usage::{ScanCache, UNKNOWN_MODEL};
use agi_engine::UsageDays;
use agi_protocol::api::UsageDay;
use serde_json::Value;

use crate::transcript::parse_iso8601_ms;

/// How deep the projects tree is walked.
///
/// A session's own directory holds its `subagents/` children, and the cap only
/// exists so a hostile symlink cannot make this loop forever.
const MAX_DEPTH: usize = 8;

/// Every day of activity in every transcript under `projects`.
///
/// A missing or unreadable file is skipped: one transcript that cannot be parsed
/// must not blank a year of history.
///
/// ### Why this walks the tree itself
///
/// [`crate::paths::list_transcripts`] answers "which sessions does this host
/// have" and deliberately skips `subagents/agent-*.jsonl`, because a delegated
/// child is not a session the list should show. Usage is a different question:
/// every one of those lines was billed to the same account, so the scan reads
/// them too. A year of totals that is smaller than the invoice is not a total.
pub fn days(projects: &Path) -> Vec<UsageDay> {
    days_with_cache(projects, &mut ScanCache::new())
}

/// The same scan, reusing the parse of every transcript that has not changed.
pub fn days_with_cache(projects: &Path, cache: &mut ScanCache) -> Vec<UsageDay> {
    let files = transcript_files(projects);
    cache.days(&files, |path| {
        std::fs::read_to_string(path)
            .ok()
            .map(|text| file_days(&text))
    })
}

/// What one transcript file contributed, on its own.
fn file_days(text: &str) -> Vec<UsageDay> {
    let mut days = UsageDays::new();
    fold_transcript(&mut days, text);
    days.finish()
}

/// Every `*.jsonl` under `root`, subagent transcripts included, in a stable
/// order.
fn transcript_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, 0, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            walk(&path, depth + 1, out);
            continue;
        }
        if entry.file_name().to_string_lossy().ends_with(".jsonl") {
            out.push(path);
        }
    }
}

/// One message line, parsed but not yet attributed.
#[derive(Debug, Clone)]
struct Event {
    at: i64,
    /// The model this line named, if it named one.
    model: Option<String>,
    /// Whether this line is a message rather than plumbing.
    is_message: bool,
    input: u64,
    output: u64,
}

/// Fold one transcript's text into `days`.
pub fn fold_transcript(days: &mut UsageDays, text: &str) {
    let mut events: Vec<Event> = Vec::new();
    let mut model_counts: BTreeMap<String, u64> = BTreeMap::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            // A half-written last line is the normal state of a live session.
            continue;
        };
        let Some(event) = read_line(&value) else {
            continue;
        };
        if let Some(model) = event.model.as_deref() {
            *model_counts.entry(model.to_string()).or_default() += 1;
        }
        events.push(event);
    }

    let session_model = model_counts
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))
        .map(|(model, _)| model)
        .unwrap_or_else(|| UNKNOWN_MODEL.to_string());

    for event in events {
        let model = event.model.as_deref().unwrap_or(&session_model);
        if event.is_message {
            days.message(event.at, model, event.input, event.output);
        } else {
            days.tokens(event.at, model, event.input, event.output);
        }
    }
}

/// Read one transcript line, if it is a message or a billed request.
fn read_line(value: &Value) -> Option<Event> {
    let at = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_iso8601_ms)?;
    let kind = value.get("type").and_then(Value::as_str)?;
    let message = value.get("message");

    match kind {
        "user" => {
            if is_tool_result(message) {
                return None;
            }
            // A prompt costs nothing on the input side: the tokens it spends are
            // billed on the assistant reply that follows it.
            Some(Event {
                at,
                model: None,
                is_message: true,
                input: 0,
                output: 0,
            })
        }
        "assistant" => {
            let (input, output) = usage_of(message);
            Some(Event {
                at,
                model: model_of(message),
                is_message: says_something(message),
                input,
                output,
            })
        }
        // Titles, hooks, file snapshots and API errors are not conversation, and
        // the provider billed none of them.
        _ => None,
    }
}

/// Whether a `user` line is a tool's output rather than a person.
fn is_tool_result(message: Option<&Value>) -> bool {
    let Some(blocks) = message.and_then(|message| message.get("content")) else {
        return false;
    };
    let Some(blocks) = blocks.as_array() else {
        return false;
    };
    !blocks.is_empty()
        && blocks
            .iter()
            .all(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
}

/// Whether an assistant line said anything out loud.
fn says_something(message: Option<&Value>) -> bool {
    match message.and_then(|message| message.get("content")) {
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(blocks)) => blocks.iter().any(|block| {
            block.get("type").and_then(Value::as_str) == Some("text")
                && block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty())
        }),
        _ => false,
    }
}

/// The model that answered, when the line named a real one.
fn model_of(message: Option<&Value>) -> Option<String> {
    let model = message?
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())?;
    // `<synthetic>` is Claude Code's own marker for a line it wrote itself.
    if model.starts_with('<') {
        return None;
    }
    Some(model.to_string())
}

/// Input and output tokens of one billed request.
///
/// The provider splits input three ways and bills all three: `input_tokens` is
/// the uncached remainder, and the two cache counters are tokens that were still
/// paid for — at a different rate, but paid. Reporting only the first is why a
/// cached session once looked free.
fn usage_of(message: Option<&Value>) -> (u64, u64) {
    let Some(usage) = message.and_then(|message| message.get("usage")) else {
        return (0, 0);
    };
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let input = field("input_tokens")
        + field("cache_creation_input_tokens")
        + field("cache_read_input_tokens");
    (input, field("output_tokens"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    /// A local instant, written the way Claude Code writes it.
    fn iso_at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> String {
        chrono::Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("an unambiguous local instant")
            .with_timezone(&chrono::Utc)
            .format("%Y-%m-%dT%H:%M:%S.000Z")
            .to_string()
    }

    #[test]
    fn a_cached_scan_answers_the_same_days_as_a_fresh_one() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("project").join("session");
        std::fs::create_dir_all(&nested).unwrap();
        let at = iso_at(2026, 10, 27, 9, 30);
        std::fs::write(nested.join("a.jsonl"), format!("{}\n", prompt(&at))).unwrap();

        let fresh = days(dir.path());
        let mut cache = ScanCache::new();
        assert_eq!(days_with_cache(dir.path(), &mut cache), fresh);
        // Second pass answers from the cache and must not drift.
        assert_eq!(days_with_cache(dir.path(), &mut cache), fresh);
    }

    fn prompt(at: &str) -> String {
        json!({
            "type": "user",
            "sessionId": "s1",
            "timestamp": at,
            "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}
        })
        .to_string()
    }

    fn tool_result(at: &str) -> String {
        json!({
            "type": "user",
            "sessionId": "s1",
            "timestamp": at,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "a.txt"}
            ]}
        })
        .to_string()
    }

    fn assistant(at: &str, blocks: Value, model: &str, input: u64, output: u64) -> String {
        json!({
            "type": "assistant",
            "sessionId": "s1",
            "timestamp": at,
            "message": {
                "role": "assistant",
                "model": model,
                "content": blocks,
                "usage": {
                    "input_tokens": input,
                    "cache_creation_input_tokens": 100,
                    "cache_read_input_tokens": 400,
                    "output_tokens": output
                }
            }
        })
        .to_string()
    }

    #[test]
    fn a_prompt_and_its_reply_land_on_the_same_local_day() {
        let text = [
            prompt(&iso_at(2026, 10, 27, 9, 30)),
            assistant(
                &iso_at(2026, 10, 27, 9, 31),
                json!([{"type": "text", "text": "hello"}]),
                "claude-opus-5-5",
                7,
                11,
            ),
        ]
        .join("\n");

        let mut days = UsageDays::new();
        fold_transcript(&mut days, &text);
        let folded = days.finish();

        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].date, "2026-10-27");
        assert_eq!(folded[0].messages, 2);
        // 7 uncached + 100 written + 400 read; all of it was billed.
        assert_eq!(folded[0].input_tokens, 507);
        assert_eq!(folded[0].output_tokens, 11);
        assert_eq!(folded[0].hours[9], 2);
        assert_eq!(folded[0].models.len(), 1);
        // The prompt is attributed to the model the transcript used.
        assert_eq!(folded[0].models[0].model, "claude-opus-5-5");
        assert_eq!(folded[0].models[0].messages, 2);
    }

    #[test]
    fn a_tool_result_is_not_something_a_person_said() {
        let text = [
            prompt(&iso_at(2026, 10, 27, 9, 30)),
            assistant(
                &iso_at(2026, 10, 27, 9, 31),
                json!([{"type": "tool_use", "id": "t1", "name": "Read", "input": {}}]),
                "claude-opus-5-5",
                5,
                9,
            ),
            tool_result(&iso_at(2026, 10, 27, 9, 32)),
        ]
        .join("\n");

        let mut days = UsageDays::new();
        fold_transcript(&mut days, &text);
        let folded = days.finish();

        // The prompt, and nothing for the tool result or the tool call.
        assert_eq!(folded[0].messages, 1);
        // The tokens those two requests cost are still real.
        assert_eq!(folded[0].input_tokens, 505);
        assert_eq!(folded[0].output_tokens, 9);
        assert_eq!(folded[0].hours[9], 1);
    }

    #[test]
    fn a_sidechain_and_a_thinking_block_still_count_their_tokens() {
        let text = [
            prompt(&iso_at(2026, 10, 27, 9, 30)),
            assistant(
                &iso_at(2026, 10, 27, 9, 31),
                json!([{"type": "thinking", "thinking": "hmm"}]),
                "claude-opus-5-5",
                1,
                2,
            ),
            assistant(
                &iso_at(2026, 10, 27, 9, 32),
                json!([{"type": "text", "text": "done"}]),
                "<synthetic>",
                3,
                4,
            ),
        ]
        .join("\n");

        let mut days = UsageDays::new();
        fold_transcript(&mut days, &text);
        let folded = days.finish();

        assert_eq!(folded[0].messages, 2);
        assert_eq!(folded[0].output_tokens, 6);
        // `<synthetic>` is not a model: the row falls back to the file's model.
        let models: Vec<&str> = folded[0]
            .models
            .iter()
            .map(|entry| entry.model.as_str())
            .collect();
        assert_eq!(models, vec!["claude-opus-5-5"]);
    }

    #[test]
    fn a_line_that_cannot_be_read_is_skipped_rather_than_failing_the_scan() {
        let text = [
            "{ not json".to_string(),
            String::new(),
            json!({"type": "custom-title", "customTitle": "no timestamp"}).to_string(),
            prompt(&iso_at(2026, 10, 27, 9, 30)),
        ]
        .join("\n");

        let mut days = UsageDays::new();
        fold_transcript(&mut days, &text);
        assert_eq!(days.finish()[0].messages, 1);
    }

    #[test]
    fn scans_a_projects_directory_and_splits_two_local_days() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("-Users-someone-project");
        std::fs::create_dir_all(&project).unwrap();
        let file = project.join("0f9c2b1a-1111-2222-3333-444455556666.jsonl");
        std::fs::write(
            &file,
            [
                prompt(&iso_at(2026, 10, 27, 23, 58)),
                assistant(
                    &iso_at(2026, 10, 27, 23, 59),
                    json!([{"type": "text", "text": "late"}]),
                    "claude-opus-5-5",
                    10,
                    20,
                ),
                prompt(&iso_at(2026, 10, 28, 0, 1)),
            ]
            .join("\n"),
        )
        .unwrap();

        let folded = days(dir.path());
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0].date, "2026-10-27");
        assert_eq!(folded[0].messages, 2);
        assert_eq!(folded[0].hours[23], 2);
        assert_eq!(folded[1].date, "2026-10-28");
        assert_eq!(folded[1].messages, 1);
        assert_eq!(folded[1].hours[0], 1);
    }

    #[test]
    fn a_subagents_transcript_is_billed_too() {
        // A delegated child is not a session the list shows, but its tokens were
        // paid for, and a total smaller than the invoice is not a total.
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("-Users-someone-project");
        std::fs::create_dir_all(session.join("subagents")).unwrap();
        std::fs::write(
            session.join("0f9c2b1a-1111-2222-3333-444455556666.jsonl"),
            [
                prompt(&iso_at(2026, 10, 27, 9, 0)),
                assistant(
                    &iso_at(2026, 10, 27, 9, 1),
                    json!([{"type": "text", "text": "hi"}]),
                    "claude-opus-5-5",
                    10,
                    1,
                ),
            ]
            .join("\n"),
        )
        .unwrap();
        std::fs::write(
            session.join("subagents/agent-a1.jsonl"),
            [
                prompt(&iso_at(2026, 10, 27, 9, 2)),
                assistant(
                    &iso_at(2026, 10, 27, 9, 3),
                    json!([{"type": "text", "text": "done"}]),
                    "claude-opus-5-5",
                    20,
                    2,
                ),
            ]
            .join("\n"),
        )
        .unwrap();

        let folded = days(dir.path());
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].messages, 4);
        assert_eq!(folded[0].input_tokens, 2 * 500 + 30);
        assert_eq!(folded[0].models[0].messages, 4);
    }

    #[test]
    fn a_machine_that_has_never_run_claude_code_has_no_days() {
        assert!(days(Path::new("/nonexistent/agi-mizar-claude")).is_empty());
    }
}
