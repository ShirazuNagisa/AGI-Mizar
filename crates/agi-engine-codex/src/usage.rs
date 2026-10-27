//! Folding Codex's rollout logs into the days the home page charts.
//!
//! Codex keeps a per-thread rollout under `~/.codex/sessions/YYYY/MM/DD/` and
//! appends one JSON object per event. Two of those events carry everything the
//! charts need:
//!
//! - **`event_msg` / `token_count`** — `payload.info.last_token_usage` is what
//!   the request that just finished cost, and `total_token_usage` is the
//!   thread's running total. The per-request figure is the one to bucket: the
//!   running total re-counted on every event would bill a long session once per
//!   turn. Older rollouts carry the same numbers as a top-level
//!   `token_usage_record` with `turn_token_usage`, which is read only when a file
//!   has no `token_count` at all.
//! - **`response_item` / `message`** — the conversation, with a `role` of
//!   `user`, `assistant` or `developer`. `developer` is the harness's own
//!   instructions and is not a message. A file that predates these carries its
//!   messages as `event_msg` `user_message`/`agent_message` instead, which is
//!   why the two sources are counted separately and only one of them is used:
//!   the newest format writes both for the same message.
//!
//! Codex's `input_tokens` already includes the cached prefix (`total_tokens` is
//! `input + output`), so nothing is added to it, unlike Claude Code where the
//! cache counters are separate.

use std::path::{Path, PathBuf};

use agi_engine::usage::{parse_instant_ms, UNKNOWN_MODEL};
use agi_engine::UsageDays;
use agi_protocol::api::UsageDay;
use serde_json::Value;

/// How deep the session tree is walked.
///
/// The real layout is `sessions/<year>/<month>/<day>/rollout-*.jsonl`, and the
/// cap only exists so a hostile symlink cannot make this loop forever.
const MAX_DEPTH: usize = 8;

/// Codex's configuration directory: `CODEX_HOME` when it is set.
pub fn codex_home() -> PathBuf {
    if let Ok(configured) = std::env::var("CODEX_HOME") {
        let trimmed = configured.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".codex")
}

/// The directory holding one subtree per day of rollouts.
pub fn sessions_dir() -> PathBuf {
    codex_home().join("sessions")
}

/// Every day of activity in every rollout under `sessions`.
///
/// A missing tree is no days rather than an error: a machine that has never run
/// Codex has no rollouts, which is a fact the window draws as an empty graph.
pub fn days(sessions: &Path) -> Vec<UsageDay> {
    let mut days = UsageDays::new();
    for path in rollout_files(sessions) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        fold_rollout(&mut days, &text);
    }
    days.finish()
}

/// A set of days plus how many rows went into it, so one of two sources can win.
#[derive(Debug, Default)]
struct Counted {
    days: UsageDays,
    count: u64,
}

impl Counted {
    fn message(&mut self, at: i64, model: &str) {
        self.days.message(at, model, 0, 0);
        self.count += 1;
    }

    fn tokens(&mut self, at: i64, model: &str, input: u64, output: u64) {
        self.days.tokens(at, model, input, output);
        self.count += 1;
    }
}

/// Fold one rollout's text into `days`.
pub fn fold_rollout(days: &mut UsageDays, text: &str) {
    // Tokens are counted once per file, whichever event carried them.
    let mut reported_tokens = Counted::default();
    let mut recorded_tokens = Counted::default();
    // Messages are counted from one format or the other, never both.
    let mut response_messages = Counted::default();
    let mut event_messages = Counted::default();

    // The model a line belongs to: Codex states it in the thread's settings, not
    // on each message, so the latest one seen is what a token was billed at.
    let mut model: Option<String> = None;
    // The running total last seen, for a rollout that only reports totals.
    let mut last_total: Option<(u64, u64)> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // A half-written last line is the normal state of a live rollout.
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(at) = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_instant_ms)
        else {
            continue;
        };
        let payload = value.get("payload");
        let name = model.as_deref().unwrap_or(UNKNOWN_MODEL);

        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "world_state" => {
                model = model_of(payload.and_then(|payload| {
                    payload
                        .get("state")
                        .and_then(|state| state.get("collaboration_mode"))
                }))
                .or(model);
            }
            "token_usage_record" => {
                let usage = payload
                    .and_then(|payload| payload.get("turn_token_usage"))
                    .or_else(|| payload.and_then(|payload| payload.get("usage")));
                let (input, output) = token_pair(usage);
                if input > 0 || output > 0 {
                    recorded_tokens.tokens(at, name, input, output);
                }
            }
            "event_msg" => {
                let kind = payload
                    .and_then(|payload| payload.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match kind {
                    "token_count" => {
                        let info = payload.and_then(|payload| payload.get("info"));
                        let last = token_pair(info.and_then(|info| info.get("last_token_usage")));
                        let total = token_pair(info.and_then(|info| info.get("total_token_usage")));
                        let (input, output) = if last.0 > 0 || last.1 > 0 {
                            last
                        } else {
                            // No per-request figure: the difference between two
                            // running totals is the same number, and the first
                            // one is the difference from nothing.
                            let previous = last_total.unwrap_or((0, 0));
                            (
                                total.0.saturating_sub(previous.0),
                                total.1.saturating_sub(previous.1),
                            )
                        };
                        if total.0 > 0 || total.1 > 0 {
                            last_total = Some(total);
                        }
                        if input > 0 || output > 0 {
                            reported_tokens.tokens(at, name, input, output);
                        }
                    }
                    "user_message" | "agent_message" => {
                        // A message costs nothing of its own: the request that
                        // read or wrote it is the `token_count` above.
                        event_messages.message(at, name);
                    }
                    "thread_settings_applied" => {
                        model =
                            model_of(payload.and_then(|payload| payload.get("thread_settings")))
                                .or(model);
                    }
                    _ => {}
                }
            }
            "response_item" => {
                let is_message = payload
                    .and_then(|payload| payload.get("type"))
                    .and_then(Value::as_str)
                    == Some("message");
                let role = payload
                    .and_then(|payload| payload.get("role"))
                    .and_then(Value::as_str);
                if is_message && matches!(role, Some("user" | "assistant")) {
                    response_messages.message(at, name);
                }
            }
            _ => {}
        }
    }

    // One source per file, and the newer format wins when both are present: the
    // rollouts that carry `event_msg` messages also carry the same words as
    // `response_item` messages, and counting both doubles the day.
    let tokens = if reported_tokens.count > 0 {
        reported_tokens
    } else {
        recorded_tokens
    };
    let messages = if response_messages.count > 0 {
        response_messages
    } else {
        event_messages
    };

    days.merge(tokens.days.finish());
    days.merge(messages.days.finish());
}

/// The `{model}` of a settings block, when it names a real one.
fn model_of(settings: Option<&Value>) -> Option<String> {
    settings?
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
}

/// Input and output tokens of a usage object.
fn token_pair(usage: Option<&Value>) -> (u64, u64) {
    let Some(usage) = usage else {
        return (0, 0);
    };
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    (field("input_tokens"), field("output_tokens"))
}

/// Every rollout file under `root`, in a stable order.
fn rollout_files(root: &Path) -> Vec<PathBuf> {
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
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("rollout-") && name.ends_with(".jsonl") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    /// A local instant, written the way Codex writes it.
    fn iso_at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> String {
        chrono::Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("an unambiguous local instant")
            .with_timezone(&chrono::Utc)
            .format("%Y-%m-%dT%H:%M:%S.000Z")
            .to_string()
    }

    fn settings(at: &str, model: &str) -> String {
        json!({
            "timestamp": at,
            "type": "event_msg",
            "payload": {
                "type": "thread_settings_applied",
                "thread_settings": {"model": model}
            }
        })
        .to_string()
    }

    fn token_count(at: &str, total: (u64, u64), last: (u64, u64)) -> String {
        json!({
            "timestamp": at,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": total.0, "output_tokens": total.1,
                        "total_tokens": total.0 + total.1
                    },
                    "last_token_usage": {
                        "input_tokens": last.0, "output_tokens": last.1,
                        "total_tokens": last.0 + last.1
                    }
                }
            }
        })
        .to_string()
    }

    fn message(at: &str, role: &str) -> String {
        json!({
            "timestamp": at,
            "type": "response_item",
            "payload": {"type": "message", "role": role, "content": []}
        })
        .to_string()
    }

    fn function_call(at: &str) -> String {
        json!({
            "timestamp": at,
            "type": "response_item",
            "payload": {"type": "function_call", "name": "exec_command", "call_id": "c1"}
        })
        .to_string()
    }

    #[test]
    fn counts_the_per_request_figure_rather_than_the_running_total() {
        // Two requests in one thread: the totals are cumulative, so summing the
        // totals would bill the first request twice.
        let text = [
            settings(&iso_at(2026, 10, 19, 16, 37), "deepseek-flash"),
            message(&iso_at(2026, 10, 19, 16, 37), "user"),
            message(&iso_at(2026, 10, 19, 16, 38), "assistant"),
            token_count(&iso_at(2026, 10, 19, 16, 38), (1_000, 100), (1_000, 100)),
            message(&iso_at(2026, 10, 19, 16, 39), "user"),
            token_count(&iso_at(2026, 10, 19, 16, 40), (3_000, 400), (2_000, 300)),
        ]
        .join("\n");

        let mut days = UsageDays::new();
        fold_rollout(&mut days, &text);
        let folded = days.finish();

        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].date, "2026-10-19");
        assert_eq!(folded[0].messages, 3);
        assert_eq!(folded[0].input_tokens, 3_000);
        assert_eq!(folded[0].output_tokens, 400);
        assert_eq!(folded[0].hours[16], 3);
        assert_eq!(folded[0].models[0].model, "deepseek-flash");
        assert_eq!(folded[0].models[0].input_tokens, 3_000);
    }

    #[test]
    fn a_developer_message_is_instructions_not_conversation() {
        let text = [
            message(&iso_at(2026, 10, 19, 9, 0), "developer"),
            message(&iso_at(2026, 10, 19, 9, 1), "user"),
            function_call(&iso_at(2026, 10, 19, 9, 2)),
            message(&iso_at(2026, 10, 19, 9, 3), "assistant"),
        ]
        .join("\n");

        let mut days = UsageDays::new();
        fold_rollout(&mut days, &text);
        let folded = days.finish();
        assert_eq!(folded[0].messages, 2);
        // No `token_count` in this rollout at all, so there is nothing to bill.
        assert_eq!(folded[0].input_tokens, 0);
        // Nothing named a model.
        assert_eq!(folded[0].models[0].model, UNKNOWN_MODEL);
    }

    #[test]
    fn an_older_rollout_reports_messages_as_event_msg() {
        let text = [
            json!({"timestamp": iso_at(2026, 10, 19, 9, 0), "type": "event_msg",
                   "payload": {"type": "user_message", "message": "hi"}})
            .to_string(),
            json!({"timestamp": iso_at(2026, 10, 19, 9, 1), "type": "event_msg",
                   "payload": {"type": "agent_message", "message": "hello"}})
            .to_string(),
            json!({"timestamp": iso_at(2026, 10, 19, 9, 2), "type": "token_usage_record",
                   "payload": {"turn_token_usage": {"input_tokens": 40, "output_tokens": 4}}})
            .to_string(),
        ]
        .join("\n");

        let mut days = UsageDays::new();
        fold_rollout(&mut days, &text);
        let folded = days.finish();
        assert_eq!(folded[0].messages, 2);
        assert_eq!(folded[0].input_tokens, 40);
        assert_eq!(folded[0].output_tokens, 4);
    }

    #[test]
    fn a_rollout_that_carries_both_formats_counts_the_messages_once() {
        let text = [
            message(&iso_at(2026, 10, 19, 9, 0), "user"),
            json!({"timestamp": iso_at(2026, 10, 19, 9, 0), "type": "event_msg",
                   "payload": {"type": "user_message", "message": "hi"}})
            .to_string(),
            message(&iso_at(2026, 10, 19, 9, 1), "assistant"),
            json!({"timestamp": iso_at(2026, 10, 19, 9, 1), "type": "event_msg",
                   "payload": {"type": "agent_message", "message": "hello"}})
            .to_string(),
            token_count(&iso_at(2026, 10, 19, 9, 1), (10, 2), (10, 2)),
        ]
        .join("\n");

        let mut days = UsageDays::new();
        fold_rollout(&mut days, &text);
        assert_eq!(days.finish()[0].messages, 2);
    }

    #[test]
    fn a_total_without_a_per_request_figure_is_read_as_a_difference() {
        let text = [
            json!({"timestamp": iso_at(2026, 10, 19, 9, 0), "type": "event_msg",
                   "payload": {"type": "token_count", "info": {
                       "total_token_usage": {"input_tokens": 100, "output_tokens": 10}}}})
            .to_string(),
            json!({"timestamp": iso_at(2026, 10, 19, 9, 5), "type": "event_msg",
                   "payload": {"type": "token_count", "info": {
                       "total_token_usage": {"input_tokens": 250, "output_tokens": 40}}}})
            .to_string(),
        ]
        .join("\n");

        let mut days = UsageDays::new();
        fold_rollout(&mut days, &text);
        let folded = days.finish();
        assert_eq!(folded[0].input_tokens, 250);
        assert_eq!(folded[0].output_tokens, 40);
        // Two usage events, no messages: a step that only ran tools.
        assert_eq!(folded[0].messages, 0);
        assert_eq!(folded[0].hours.iter().sum::<u64>(), 0);
    }

    #[test]
    fn scans_the_year_month_day_tree_and_skips_other_files() {
        let home = tempfile::tempdir().unwrap();
        let day_dir = home.path().join("sessions/2026/10/19");
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(
            day_dir.join("rollout-2026-10-19T16-37-20-abc.jsonl"),
            [
                message(&iso_at(2026, 10, 19, 23, 58), "user"),
                token_count(&iso_at(2026, 10, 19, 23, 58), (7, 3), (7, 3)),
                message(&iso_at(2026, 10, 20, 0, 2), "assistant"),
            ]
            .join("\n"),
        )
        .unwrap();
        // Not a rollout: a lock file, a settings snapshot, a journal.
        std::fs::write(day_dir.join("history.jsonl"), "{}").unwrap();

        let folded = days(&home.path().join("sessions"));
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0].date, "2026-10-19");
        assert_eq!(folded[0].messages, 1);
        assert_eq!(folded[0].input_tokens, 7);
        assert_eq!(folded[0].hours[23], 1);
        assert_eq!(folded[1].date, "2026-10-20");
        assert_eq!(folded[1].messages, 1);
        assert_eq!(folded[1].hours[0], 1);
    }

    #[test]
    fn a_machine_that_has_never_run_codex_has_no_days() {
        assert!(days(&sessions_dir().join("nonexistent-agi-mizar")).is_empty());
    }
}
