//! Folding messages into the per-day usage the desktop's home page charts.
//!
//! All three adapters answer `usage_history` with the same shape, and all three
//! have to make the same two decisions: which local day and hour an instant
//! falls in, and how a day's models are added up. Doing that three times would
//! produce three answers that drift — one engine counting a late-evening message
//! on the previous day is exactly the kind of bug nobody notices until two
//! charts disagree.
//!
//! ### Local days, because that is the unit a person works in
//!
//! Every timestamp an engine stores is UTC (epoch milliseconds, or an ISO string
//! ending in `Z`). "What did I do on Tuesday" is not a UTC question, so the
//! bucketing happens here, once, through [`chrono::Local`].
//!
//! ### What `hours` counts
//!
//! Messages, not tokens: the home page computes a peak hour from it, and an hour
//! that was active but cheap is still an hour the operator was working. Tokens
//! are the per-day totals, which are what a cost line needs.

use std::collections::BTreeMap;

use agi_protocol::api::{ModelUsage, UsageDay};

/// The model an engine could not name.
///
/// A row is better than no row: a chart that silently drops the sessions it
/// could not attribute reads as a quieter week than the operator had.
pub const UNKNOWN_MODEL: &str = "unknown";

/// The local calendar day and hour an instant falls in.
///
/// `None` only for an instant outside the representable range, which is a
/// corrupt record rather than a date.
pub fn local_bucket(at_ms: i64) -> Option<(String, usize)> {
    use chrono::Timelike;

    let utc = chrono::DateTime::from_timestamp_millis(at_ms)?;
    let local = utc.with_timezone(&chrono::Local);
    Some((local.format("%Y-%m-%d").to_string(), local.hour() as usize))
}

/// The instant an RFC 3339 timestamp names, in epoch milliseconds.
///
/// Codex writes `2026-10-19T08:37:20.447Z` on every line, with the offset
/// spelled `Z`; a record with a numeric offset is read the same way.
pub fn parse_instant_ms(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|at| at.timestamp_millis())
}

/// One day, while it is still being added up.
#[derive(Debug, Default)]
struct DayBuilder {
    messages: u64,
    input_tokens: u64,
    output_tokens: u64,
    hours: [u64; 24],
    models: BTreeMap<String, ModelUsage>,
}

impl DayBuilder {
    fn add(&mut self, hour: usize, model: &str, messages: u64, input: u64, output: u64) {
        self.messages += messages;
        self.input_tokens += input;
        self.output_tokens += output;
        // `local_bucket` answers 0..=23; the mask is belt and braces so a
        // corrupt record cannot panic on a chart refresh.
        self.hours[hour.min(23)] += messages;

        let name = if model.trim().is_empty() {
            UNKNOWN_MODEL
        } else {
            model
        };
        let entry = self
            .models
            .entry(name.to_string())
            .or_insert_with(|| ModelUsage {
                model: name.to_string(),
                ..Default::default()
            });
        entry.messages += messages;
        entry.input_tokens += input;
        entry.output_tokens += output;
    }

    fn finish(self, date: String) -> UsageDay {
        let mut models: Vec<ModelUsage> = self.models.into_values().collect();
        // Most-used first: the home page shows the top few, and an order that
        // depends on hash iteration would reshuffle between refreshes.
        models.sort_by(|left, right| {
            right
                .messages
                .cmp(&left.messages)
                .then_with(|| right.input_tokens.cmp(&left.input_tokens))
                .then_with(|| left.model.cmp(&right.model))
        });
        UsageDay {
            date,
            messages: self.messages,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            hours: self.hours,
            models,
        }
    }
}

/// Every day of activity, accumulated.
///
/// An adapter feeds it one event at a time from whatever its engine stores; the
/// kernel feeds it whole days from three adapters. Both end up in the same type,
/// so merging cannot be a different sum from counting.
#[derive(Debug, Default)]
pub struct UsageDays {
    days: BTreeMap<String, DayBuilder>,
}

impl UsageDays {
    /// Nothing counted yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one message, and the tokens it cost, at the instant `at_ms`.
    ///
    /// A message that cost nothing — an engine that does not record usage — is
    /// still a message, which is why the two counters are separate arguments.
    pub fn message(&mut self, at_ms: i64, model: &str, input: u64, output: u64) {
        self.add(at_ms, model, 1, input, output);
    }

    /// Add tokens that are not a message.
    ///
    /// Providers bill per request, not per message: a step that only called
    /// tools cost tokens and produced no message, and one assistant reply can
    /// arrive as several billed requests.
    pub fn tokens(&mut self, at_ms: i64, model: &str, input: u64, output: u64) {
        self.add(at_ms, model, 0, input, output);
    }

    fn add(&mut self, at_ms: i64, model: &str, messages: u64, input: u64, output: u64) {
        let Some((date, hour)) = local_bucket(at_ms) else {
            return;
        };
        self.days
            .entry(date)
            .or_default()
            .add(hour, model, messages, input, output);
    }

    /// Add days another engine already folded.
    pub fn merge(&mut self, days: impl IntoIterator<Item = UsageDay>) {
        for day in days {
            let entry = self.days.entry(day.date.clone()).or_default();
            entry.messages += day.messages;
            entry.input_tokens += day.input_tokens;
            entry.output_tokens += day.output_tokens;
            for (hour, count) in day.hours.iter().enumerate().take(24) {
                entry.hours[hour] += count;
            }
            for model in day.models {
                let target =
                    entry
                        .models
                        .entry(model.model.clone())
                        .or_insert_with(|| ModelUsage {
                            model: model.model.clone(),
                            ..Default::default()
                        });
                target.messages += model.messages;
                target.input_tokens += model.input_tokens;
                target.output_tokens += model.output_tokens;
            }
        }
    }

    /// The days, oldest first, with only the dates that had activity.
    pub fn finish(self) -> Vec<UsageDay> {
        self.days
            .into_iter()
            .map(|(date, builder)| builder.finish(date))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A local instant, as epoch milliseconds.
    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        chrono::Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("an unambiguous local instant")
            .timestamp_millis()
    }

    #[test]
    fn a_message_lands_on_its_local_day_and_hour() {
        let mut days = UsageDays::new();
        days.message(at(2026, 10, 27, 14, 30), "deepseek-v4", 100, 20);

        let folded = days.finish();
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].date, "2026-10-27");
        assert_eq!(folded[0].messages, 1);
        assert_eq!(folded[0].input_tokens, 100);
        assert_eq!(folded[0].output_tokens, 20);
        assert_eq!(folded[0].hours[14], 1);
        assert_eq!(folded[0].hours.iter().sum::<u64>(), 1);
        assert_eq!(folded[0].models.len(), 1);
        assert_eq!(folded[0].models[0].model, "deepseek-v4");
    }

    #[test]
    fn an_empty_local_day_boundary_still_splits_the_days() {
        // Two minutes apart, and two different local days: the whole point of
        // bucketing locally rather than by UTC.
        let mut days = UsageDays::new();
        days.message(at(2026, 10, 27, 23, 58), "m", 0, 0);
        days.message(at(2026, 10, 28, 0, 1), "m", 0, 0);

        let folded = days.finish();
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0].date, "2026-10-27");
        assert_eq!(folded[0].hours[23], 1);
        assert_eq!(folded[1].date, "2026-10-28");
        assert_eq!(folded[1].hours[0], 1);
    }

    #[test]
    fn one_day_carries_one_row_per_model_most_used_first() {
        let mut days = UsageDays::new();
        days.message(at(2026, 10, 27, 9, 0), "small", 10, 1);
        days.message(at(2026, 10, 27, 10, 0), "big", 900, 90);
        days.message(at(2026, 10, 27, 11, 0), "big", 900, 90);
        days.tokens(at(2026, 10, 27, 12, 0), "big", 5, 5);

        let folded = days.finish();
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].messages, 3);
        // Tokens from a step that was not a message still count.
        assert_eq!(folded[0].input_tokens, 10 + 900 + 900 + 5);
        assert_eq!(folded[0].output_tokens, 1 + 90 + 90 + 5);
        let models: Vec<&str> = folded[0]
            .models
            .iter()
            .map(|entry| entry.model.as_str())
            .collect();
        assert_eq!(models, vec!["big", "small"]);
        assert_eq!(folded[0].models[0].messages, 2);
        assert_eq!(folded[0].models[0].input_tokens, 1805);
    }

    #[test]
    fn a_nameless_model_is_still_a_row() {
        let mut days = UsageDays::new();
        days.message(at(2026, 10, 27, 9, 0), "", 1, 1);
        let folded = days.finish();
        assert_eq!(folded[0].models[0].model, UNKNOWN_MODEL);
    }

    #[test]
    fn merging_two_engines_adds_the_same_date_instead_of_listing_it_twice() {
        let mut mine = UsageDays::new();
        mine.message(at(2026, 10, 27, 9, 0), "a", 10, 1);

        let mut theirs = UsageDays::new();
        theirs.message(at(2026, 10, 27, 9, 30), "b", 20, 2);
        theirs.message(at(2026, 10, 26, 9, 30), "b", 30, 3);

        mine.merge(theirs.finish());
        let folded = mine.finish();
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0].date, "2026-10-26");
        assert_eq!(folded[1].date, "2026-10-27");
        assert_eq!(folded[1].messages, 2);
        assert_eq!(folded[1].input_tokens, 30);
        assert_eq!(folded[1].hours[9], 2);
        assert_eq!(folded[1].models.len(), 2);
    }

    #[test]
    fn a_game_that_has_never_run_is_no_days_at_all() {
        assert!(UsageDays::new().finish().is_empty());
    }
}
