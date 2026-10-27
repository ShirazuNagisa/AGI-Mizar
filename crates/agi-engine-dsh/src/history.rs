//! Folding DSH's records into the days the home page charts.
//!
//! ### Where the tokens come from
//!
//! Every committed `assistant/message` record carries the provider's own usage
//! object in `data.usage`:
//!
//! ```text
//! { "inputTokens": 4407, "outputTokens": 185,
//!   "cacheReadTokens": 7808, "cacheWriteTokens": 0, "totalTokens": 12400 }
//! ```
//!
//! (`tokenUsageValue` in `dsh-session-format-v0-to-v1` requires the first two and
//! allows the four rest; measured on this machine, 91 of 91 assistant records in a
//! real session carried it.) A request's tokens therefore belong to the moment
//! that request was committed, which is what a daily chart needs. The per-session
//! cumulative `tokenUsage` projection is the host's own fold of these same
//! samples, and [`fold_raw`] reproduces its rules so the two cannot disagree.
//!
//! ### Why this reads raw records
//!
//! [`crate::projection`] drops `data.usage` on purpose — the conversation view has
//! no use for it — so the usage fold is handed the records the plugin sent,
//! untouched, and projects only what it needs (which messages count).
//!
//! ### Which record shapes are read
//!
//! - **Messages** — through [`crate::projection::project`], so a runtime-context
//!   injection is not a prompt here either, and a tool call is never a message.
//! - **Tokens** — `assistant/message` → `data.usage`. The billed input is
//!   `inputTokens + cacheReadTokens + cacheWriteTokens`: the cache counters are
//!   real tokens that were paid for at a different rate, and leaving them out is
//!   how a cached session once looked free. That is the same sum the host reports
//!   as `billedInputTokens`, which is what the reconciliation against
//!   `session/projections` compares.
//! - **Replaced samples** — a step can settle more than one sample (a retry, or a
//!   second message in the same step). The host's `tokenUsage` unit keeps the
//!   *last* sample per `(turn, step)` and replaces the earlier one rather than
//!   adding both, so this does the same; a chart that added both would overstate
//!   the day by exactly the retried tokens.
//! - **`assistant/attempt`** — a stream that committed no message. Its `usage` is
//!   read the same way when it has one; the per-chunk samples inside
//!   `data.stream` are not walked, because on this machine attempts never commit.

use std::collections::HashMap;

use agi_engine::usage::UNKNOWN_MODEL;
use agi_engine::UsageDays;
use agi_protocol::domain::RecordKind;
use serde_json::Value;

use crate::projection;

/// One provider-reported sample, waiting to be placed on a day.
#[derive(Debug, Clone)]
struct Sample {
    at: i64,
    model: String,
    input: u64,
    output: u64,
}

/// Which sample slot a record fills.
///
/// A slot is `(turn, step)` when the record names one, because that is the unit
/// the host replaces within; a record without them cannot be superseded by
/// anything and gets a slot of its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Slot {
    Step(i64, i64),
    Record(i64),
}

/// One session's samples, while its pages arrive.
///
/// `session/page` is a *page*: a long session is several reads, and a step's
/// samples can be split across two of them. Keeping the slots here rather than
/// inside one call means the replacement rule still holds across the seam, and
/// keeps only the samples — never the records, which carry whole streamed
/// responses.
#[derive(Debug, Default)]
pub struct SessionUsage {
    samples: Vec<Sample>,
    slots: HashMap<Slot, usize>,
}

impl SessionUsage {
    /// Count one page of raw records: messages straight into `days`, samples
    /// held until [`SessionUsage::finish`].
    ///
    /// `session_model` is the model the session list reports, used only when a
    /// record names none of its own.
    pub fn absorb(&mut self, days: &mut UsageDays, records: &[Value], session_model: Option<&str>) {
        for record in records {
            let model = model_of(record).or(session_model).unwrap_or(UNKNOWN_MODEL);

            // Messages: the same rule the conversation view uses, which is also
            // why an injected `runtime-context` user message is not a prompt here.
            if let Some(projected) = projection::project(record) {
                if matches!(
                    projected.kind,
                    RecordKind::UserMessage | RecordKind::AssistantMessage
                ) {
                    days.message(projected.time, model, 0, 0);
                }
            }

            let Some((slot, sample)) = usage_of(record, model) else {
                continue;
            };
            match self.slots.get(&slot) {
                Some(&index) => self.samples[index] = sample,
                None => {
                    self.slots.insert(slot, self.samples.len());
                    self.samples.push(sample);
                }
            }
        }
    }

    /// Place the samples that survived replacement on their days.
    pub fn finish(self, days: &mut UsageDays) {
        for sample in self.samples {
            days.tokens(sample.at, &sample.model, sample.input, sample.output);
        }
    }
}

/// Count one page of a session's raw records into `days`.
///
/// The one-call form; a caller reading several pages uses [`SessionUsage`].
///
/// Samples are collected before any is counted so a replaced one is never added
/// at all: the alternative is subtracting it back out of a day it may already
/// have been split across.
pub fn fold_raw(days: &mut UsageDays, records: &[Value], session_model: Option<&str>) {
    let mut session = SessionUsage::default();
    session.absorb(days, records, session_model);
    session.finish(days);
}

/// The provider's usage for one record, and the slot it fills.
///
/// A page answers `{type: "event", event: {seq, time, type, data}}`; the same
/// unwrapping the projection uses is applied first, so a bare record works too.
fn usage_of(record: &Value, model: &str) -> Option<(Slot, Sample)> {
    let event = projection::event_of(record)?;
    let kind = event.get("type").and_then(Value::as_str)?;
    if kind != "assistant/message" && kind != "assistant/attempt" {
        return None;
    }
    let data = event.get("data")?;
    let usage = data.get("usage")?;
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let input = field("inputTokens") + field("cacheReadTokens") + field("cacheWriteTokens");
    let output = field("outputTokens");
    if input == 0 && output == 0 {
        // An all-zero sample is what a provider that reports nothing leaves
        // behind; counting it would be a zero-shaped claim about a real request.
        return None;
    }

    let at = event.get("time").and_then(Value::as_i64)?;
    let turn = data.get("turn").and_then(Value::as_i64);
    let step = data.get("step").and_then(Value::as_i64);
    let seq = event.get("seq").and_then(Value::as_i64).unwrap_or(0);

    let slot = match (turn, step) {
        (Some(turn), Some(step)) => Slot::Step(turn, step),
        _ => Slot::Record(seq),
    };
    Some((
        slot,
        Sample {
            at,
            model: model.to_string(),
            input,
            output,
        },
    ))
}

/// The model a record says answered it.
///
/// Measured shape: `data.message.source = {kind: "model", provider: …,
/// model: "deepseek-flash"}`. A session that switches models therefore lands its
/// tokens on the right row of the chart instead of on whatever model it ended on.
fn model_of(record: &Value) -> Option<&str> {
    projection::event_of(record)?
        .get("data")?
        .get("message")?
        .get("source")?
        .get("model")?
        .as_str()
        .map(str::trim)
        .filter(|model| !model.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    /// A local instant, as epoch milliseconds (what a DSH record carries).
    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        chrono::Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("an unambiguous local instant")
            .timestamp_millis()
    }

    fn assistant(time: i64, turn: i64, step: i64, model: &str, usage: Value) -> Value {
        json!({
            "type": "assistant/message",
            "seq": step + 100,
            "time": time,
            "data": {
                "turn": turn,
                "step": step,
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "done" }],
                    "source": { "kind": "model", "provider": "deepseek-account", "model": model }
                },
                "usage": usage
            }
        })
    }

    fn user(time: i64, turn: i64, text: &str) -> Value {
        json!({
            "type": "user/message",
            "seq": 1,
            "time": time,
            "data": { "turn": turn, "content": [{ "type": "text", "text": text }] }
        })
    }

    #[test]
    fn a_records_own_usage_lands_on_its_day_hour_and_model() {
        let records = vec![
            user(at(2026, 10, 27, 9, 0), 1, "hello"),
            assistant(
                at(2026, 10, 27, 9, 1),
                1,
                1,
                "deepseek-flash",
                json!({"inputTokens": 4407, "outputTokens": 185,
                       "cacheReadTokens": 7808, "cacheWriteTokens": 3, "totalTokens": 12400}),
            ),
        ];

        let mut days = UsageDays::new();
        fold_raw(&mut days, &records, Some("session-model"));
        let folded = days.finish();

        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].date, "2026-10-27");
        assert_eq!(folded[0].messages, 2);
        assert_eq!(folded[0].hours[9], 2);
        // Billed input is uncached + written + read, the same sum the host's
        // `billedInputTokens` reports.
        assert_eq!(folded[0].input_tokens, 4407 + 7808 + 3);
        assert_eq!(folded[0].output_tokens, 185);
        // The record names its own model, which wins over the session's.
        assert_eq!(folded[0].models[0].model, "deepseek-flash");
    }

    #[test]
    fn a_page_wraps_each_record_in_an_event() {
        // `session/page` answers `{type: "event", event: {seq, time, type, data}}`,
        // and a build that forwarded the record itself is read the same way.
        let wrapped = json!({
            "type": "event",
            "event": {
                "type": "assistant/message",
                "seq": 9,
                "time": at(2026, 10, 27, 9, 1),
                "data": {
                    "turn": 1,
                    "step": 1,
                    "message": {
                        "content": [{ "type": "text", "text": "done" }],
                        "source": { "kind": "model", "model": "deepseek-flash" }
                    },
                    "usage": { "inputTokens": 20, "outputTokens": 2 }
                }
            }
        });

        let mut days = UsageDays::new();
        fold_raw(&mut days, &[wrapped], None);
        let folded = days.finish();

        assert_eq!(folded[0].messages, 1);
        assert_eq!(folded[0].input_tokens, 20);
        assert_eq!(folded[0].output_tokens, 2);
        assert_eq!(folded[0].models[0].model, "deepseek-flash");
    }

    #[test]
    fn a_retried_step_counts_once_the_way_the_host_counts_it() {
        // Two samples for the same (turn, step): the second settles the step, so
        // counting both would overstate the day by exactly the retried tokens.
        let records = vec![
            assistant(
                at(2026, 10, 27, 9, 1),
                1,
                1,
                "m",
                json!({"inputTokens": 100, "outputTokens": 10}),
            ),
            assistant(
                at(2026, 10, 27, 9, 2),
                1,
                1,
                "m",
                json!({"inputTokens": 120, "outputTokens": 12}),
            ),
            assistant(
                at(2026, 10, 27, 9, 3),
                1,
                2,
                "m",
                json!({"inputTokens": 50, "outputTokens": 5}),
            ),
        ];

        let mut days = UsageDays::new();
        fold_raw(&mut days, &records, None);
        let folded = days.finish();

        assert_eq!(folded[0].input_tokens, 120 + 50);
        assert_eq!(folded[0].output_tokens, 12 + 5);
        // All three were messages, though: a retry still said something.
        assert_eq!(folded[0].messages, 3);
    }

    #[test]
    fn a_step_split_across_two_pages_still_counts_once() {
        // A page boundary can land inside a step. The replacement rule is what a
        // retried step needs, and it must hold across the seam rather than
        // counting the first page's sample as if it were a finished step.
        let first = vec![assistant(
            at(2026, 10, 27, 9, 1),
            1,
            1,
            "m",
            json!({"inputTokens": 100, "outputTokens": 10}),
        )];
        let second = vec![assistant(
            at(2026, 10, 27, 9, 2),
            1,
            1,
            "m",
            json!({"inputTokens": 120, "outputTokens": 12}),
        )];

        let mut days = UsageDays::new();
        let mut session = SessionUsage::default();
        session.absorb(&mut days, &first, None);
        session.absorb(&mut days, &second, None);
        session.finish(&mut days);

        let folded = days.finish();
        assert_eq!(folded[0].input_tokens, 120);
        assert_eq!(folded[0].output_tokens, 12);
        assert_eq!(folded[0].messages, 2);
    }

    #[test]
    fn a_step_that_switched_model_lands_its_tokens_on_the_later_model() {
        let records = vec![
            assistant(
                at(2026, 10, 27, 9, 0),
                1,
                1,
                "small",
                json!({"inputTokens": 10, "outputTokens": 1}),
            ),
            assistant(
                at(2026, 10, 27, 10, 0),
                1,
                2,
                "big",
                json!({"inputTokens": 900, "outputTokens": 90}),
            ),
        ];

        let mut days = UsageDays::new();
        fold_raw(&mut days, &records, None);
        let folded = days.finish();
        let models: Vec<(&str, u64)> = folded[0]
            .models
            .iter()
            .map(|entry| (entry.model.as_str(), entry.input_tokens))
            .collect();
        assert_eq!(models, vec![("big", 900), ("small", 10)]);
    }

    #[test]
    fn a_record_without_usage_or_a_model_is_still_counted_as_a_message() {
        let records = vec![
            user(at(2026, 10, 27, 9, 0), 1, "hello"),
            // No `usage`: an older record, or a provider that reported none. The
            // message counts; no token is invented for it.
            json!({
                "type": "assistant/message", "seq": 2, "time": at(2026, 10, 27, 9, 1),
                "data": { "turn": 1, "step": 1,
                          "message": { "content": [{ "type": "text", "text": "hi" }] } }
            }),
        ];

        let mut days = UsageDays::new();
        fold_raw(&mut days, &records, None);
        let folded = days.finish();

        assert_eq!(folded[0].messages, 2);
        assert_eq!(folded[0].input_tokens, 0);
        assert_eq!(folded[0].output_tokens, 0);
        assert_eq!(folded[0].models[0].model, UNKNOWN_MODEL);
    }

    #[test]
    fn only_messages_are_messages_and_only_settlements_carry_tokens() {
        let time = at(2026, 10, 27, 9, 0);
        let mut days = UsageDays::new();
        fold_raw(
            &mut days,
            &[
                json!({"type":"turn/start","seq":1,"time":time,"data":{"turn":1}}),
                // A runtime-context injection is not something a person typed.
                json!({"type":"user/message","seq":2,"time":time,
                       "data":{"turn":1,"source":{"kind":"runtime-context"},
                               "content":[{"type":"text","text":"injected"}]}}),
                json!({"type":"tool/call","seq":3,"time":time,
                       "data":{"turn":1,"step":1,"callId":"c1","name":"bash"}}),
                json!({"type":"tool/result","seq":4,"time":time,
                       "data":{"turn":1,"step":1,"callId":"c1","content":[{"type":"text","text":"out"}]}}),
                json!({"type":"request/header","seq":5,"time":time,
                       "data":{"usage":{"inputTokens":999,"outputTokens":9}}}),
            ],
            Some("deepseek-flash"),
        );
        let folded = days.finish();

        // The tool call/result and the injected context are not messages, and a
        // `request/header` is not a settlement: nothing to count.
        assert!(folded.is_empty(), "{folded:?}");
    }

    #[test]
    fn two_local_days_keep_their_own_totals() {
        let mut days = UsageDays::new();
        fold_raw(
            &mut days,
            &[
                assistant(
                    at(2026, 10, 27, 23, 59),
                    1,
                    1,
                    "m",
                    json!({"inputTokens": 100, "outputTokens": 10}),
                ),
                assistant(
                    at(2026, 10, 28, 0, 1),
                    2,
                    1,
                    "m",
                    json!({"inputTokens": 7, "outputTokens": 1}),
                ),
            ],
            None,
        );
        let folded = days.finish();
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0].input_tokens, 100);
        assert_eq!(folded[0].hours[23], 1);
        assert_eq!(folded[1].input_tokens, 7);
        assert_eq!(folded[1].hours[0], 1);
    }
}
