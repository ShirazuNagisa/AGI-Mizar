//! Turning DSH's usage projections into the frame a phone draws.
//!
//! A direct port of `dsh` v0.8.12 `src/bridge/stats.ts`, which served the same
//! phone before this desktop existed. The rules it encodes are the host's, and
//! the app relies on them:
//!
//! 1. **A missing key means the capability is absent, not zero.** A projection
//!    key disappears when its owning package is not composed, so an absent key
//!    must render as "unknown". Reporting zero would be a lie an operator could
//!    act on.
//! 2. **The derived figures are computed here, not in the app.** Cache hit rate
//!    and context occupancy have specific rounding rules — notably that a
//!    partial hit must never round up to a flat `100` — and computing them twice
//!    means they will eventually disagree.

use agi_protocol::usage::{
    CacheHit, ContextBreakdown, ModelPair, ModelSelection, SessionUsage, Throughput, TokenUsage,
    UsageContext,
};
use serde_json::Value;

/// Read a non-negative integer field.
fn num(source: Option<&Value>, key: &str) -> Option<u64> {
    source?.get(key)?.as_u64()
}

/// Read a signed integer field.
fn signed(source: Option<&Value>, key: &str) -> Option<i64> {
    source?.get(key)?.as_i64()
}

/// Read a string field.
fn text<'a>(source: Option<&'a Value>, key: &str) -> Option<&'a str> {
    source?.get(key)?.as_str()
}

/// Build the whole frame from one projection snapshot.
pub fn from_projections(values: &Value) -> SessionUsage {
    let usage = token_usage(values.get("tokenUsage"));
    let permissions = values.get("permissions");
    SessionUsage {
        cache_hit: usage.as_ref().and_then(cache_hit),
        token_usage: usage,
        context: context(
            values.get("contextPressure"),
            values.get("contextBreakdown"),
        ),
        throughput: throughput(values.get("sessionStats")),
        model: model(values.get("modelSelection")),
        permission_preset: text(permissions, "currentValue").map(str::to_string),
        title: values
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// Build the token usage view.
pub fn token_usage(value: Option<&Value>) -> Option<TokenUsage> {
    let parts = crate::projection::token_parts(value?)?;
    Some(TokenUsage {
        uncached_input_tokens: parts.uncached,
        output_tokens: parts.output,
        cache_read_tokens: parts.cache_read,
        cache_write_tokens: parts.cache_write,
        billed_input_tokens: parts.billed_input(),
    })
}

/// Build the cache hit view from a usage view.
pub fn cache_hit(usage: &TokenUsage) -> Option<CacheHit> {
    let billed = usage.billed_input_tokens;
    Some(CacheHit {
        ratio: (billed > 0).then(|| usage.cache_read_tokens as f64 / billed as f64),
        display_percent: format_cache_hit_percent(usage.cache_read_tokens, billed),
    })
}

/// Format a cache hit ratio so it never claims a saving that did not happen.
///
/// Whole percentages are used while they are honest. A ratio that would round up
/// to `100` gets enough decimals to stay below it — `99.9`, `99.99`, and so on —
/// and a ratio that still rounds to `100` at the precision cap is floored.
pub fn format_cache_hit_percent(cache_read: u64, billed: u64) -> Option<String> {
    if billed == 0 {
        return None;
    }
    if cache_read == 0 {
        return Some("0".to_string());
    }
    if cache_read >= billed {
        return Some("100".to_string());
    }

    let ratio = cache_read as f64 / billed as f64;
    let whole = (ratio * 100.0).round();
    if whole < 100.0 {
        return Some(format!("{}", whole as u64));
    }

    for places in 1..=4 {
        let text = format!("{:.*}", places, ratio * 100.0);
        if text.parse::<f64>().is_ok_and(|value| value < 100.0) {
            return Some(text);
        }
    }
    // At four decimals a genuine partial hit still reads as 100, so step down to
    // the largest value below 100 rather than rounding the difference away.
    Some("99.9999".to_string())
}

/// Build the context pressure view.
///
/// Occupancy follows DSH's own client: prefer `projectedTokens`, fall back to
/// `pressureTokens`, and clamp to 100. A window without a pressure sample (or
/// the reverse) yields no percentage rather than a guess.
pub fn context(pressure: Option<&Value>, breakdown: Option<&Value>) -> Option<UsageContext> {
    let window = num(pressure, "contextWindow");
    let pressure_tokens = num(pressure, "pressureTokens");
    let projected = num(pressure, "projectedTokens");
    if window.is_none() && pressure_tokens.is_none() && projected.is_none() {
        return None;
    }

    let used = projected.or(pressure_tokens);
    let percent = match (used, window) {
        (Some(used), Some(window)) if window > 0 => {
            Some(((used as f64 / window as f64) * 100.0).round().min(100.0) as u64)
        }
        _ => None,
    };

    let system = num(breakdown, "systemTokens");
    let tools = num(breakdown, "toolsTokens");
    let messages = num(breakdown, "messageTokens");

    Some(UsageContext {
        window,
        pressure_tokens,
        projected_tokens: projected,
        percent,
        breakdown: match (system, tools, messages) {
            (None, None, None) => None,
            _ => Some(ContextBreakdown {
                system_tokens: system.unwrap_or(0),
                tools_tokens: tools.unwrap_or(0),
                message_tokens: messages.unwrap_or(0),
            }),
        },
    })
}

/// Build the throughput view.
pub fn throughput(value: Option<&Value>) -> Option<Throughput> {
    let turns = num(value, "turns")?;
    let steps = num(value, "steps")?;

    let ttft = signed(value, "ttftMs");
    let ttft_steps = num(value, "ttftSteps");
    let decode_ms = signed(value, "decodeMs");
    let decode_tokens = num(value, "decodeTokens");

    Some(Throughput {
        turns,
        steps,
        llm_ms: signed(value, "llmMs").unwrap_or(0),
        tool_ms: signed(value, "toolMs").unwrap_or(0),
        ttft_ms: match (ttft, ttft_steps) {
            (Some(total), Some(count)) if count > 0 => Some(total as f64 / count as f64),
            _ => None,
        },
        decode_tokens_per_second: match (decode_ms, decode_tokens) {
            (Some(ms), Some(tokens)) if ms > 0 => Some(tokens as f64 / (ms as f64 / 1000.0)),
            _ => None,
        },
    })
}

/// Build the model selection view.
pub fn model(value: Option<&Value>) -> Option<ModelPair> {
    let value = value?;
    if !value.is_object() {
        return None;
    }
    Some(ModelPair {
        last_used: selection(value.get("lastUsed")),
        next: selection(value.get("next")),
    })
}

/// Read one route.
fn selection(value: Option<&Value>) -> Option<ModelSelection> {
    let value = value?;
    let provider = value.get("provider")?.as_str()?;
    let model = value.get("model")?.as_str()?;
    Some(ModelSelection {
        provider: provider.to_string(),
        model: model.to_string(),
        reasoning_effort: value
            .get("reasoningEffort")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn adds_the_three_buckets_the_provider_bills() {
        let usage = token_usage(Some(&json!({
            "uncachedInputTokens": 1200,
            "outputTokens": 18100,
            "cacheReadTokens": 900000,
            "cacheWriteTokens": 40,
        })))
        .unwrap();
        assert_eq!(usage.billed_input_tokens, 901_240);
        assert_eq!(usage.output_tokens, 18_100);
    }

    #[test]
    fn a_usage_frame_without_an_output_count_is_not_a_usage_frame() {
        assert!(token_usage(Some(&json!({ "uncachedInputTokens": 1 }))).is_none());
        assert!(token_usage(None).is_none());
    }

    #[test]
    fn a_partial_cache_hit_never_rounds_up_to_one_hundred() {
        assert_eq!(format_cache_hit_percent(0, 100), Some("0".to_string()));
        assert_eq!(format_cache_hit_percent(1, 2), Some("50".to_string()));
        assert_eq!(format_cache_hit_percent(100, 100), Some("100".to_string()));
        // 9996/10000 is 99.96%: rounding to a whole number would say 100.
        let text = format_cache_hit_percent(9996, 10000).unwrap();
        assert!(text.starts_with("99."), "got {text}");
        assert!(text.parse::<f64>().unwrap() < 100.0);
        // A ratio closer to a full hit than four decimals can express.
        assert_eq!(
            format_cache_hit_percent(99_999_999, 100_000_000),
            Some("99.9999".to_string())
        );
        assert_eq!(format_cache_hit_percent(0, 0), None);
    }

    #[test]
    fn context_prefers_the_projected_size_and_clamps() {
        let view = context(
            Some(&json!({
                "contextWindow": 1000,
                "pressureTokens": 500,
                "projectedTokens": 600,
            })),
            None,
        )
        .unwrap();
        assert_eq!(view.window, Some(1000));
        assert_eq!(view.pressure_tokens, Some(500));
        assert_eq!(view.projected_tokens, Some(600));
        assert_eq!(view.percent, Some(60));

        let full = context(
            Some(&json!({"contextWindow": 10, "projectedTokens": 40})),
            None,
        )
        .unwrap();
        assert_eq!(full.percent, Some(100));

        // A window with no sample is not a percentage.
        let empty = context(Some(&json!({"contextWindow": 1000})), None).unwrap();
        assert_eq!(empty.percent, None);

        assert!(context(None, None).is_none());
    }

    #[test]
    fn breakdown_is_only_reported_when_a_part_of_it_exists() {
        let view = context(
            Some(&json!({"contextWindow": 10, "pressureTokens": 1})),
            Some(&json!({"systemTokens": 3})),
        )
        .unwrap();
        assert_eq!(
            view.breakdown,
            Some(ContextBreakdown {
                system_tokens: 3,
                tools_tokens: 0,
                message_tokens: 0
            })
        );
    }

    #[test]
    fn throughput_averages_what_it_can() {
        let view = throughput(Some(&json!({
            "turns": 4, "steps": 46, "llmMs": 302000, "toolMs": 501000,
            "ttftMs": 2400, "ttftSteps": 4, "decodeMs": 2000, "decodeTokens": 1000,
        })))
        .unwrap();
        assert_eq!(view.turns, 4);
        assert_eq!(view.steps, 46);
        assert_eq!(view.llm_ms, 302_000);
        assert_eq!(view.ttft_ms, Some(600.0));
        assert_eq!(view.decode_tokens_per_second, Some(500.0));

        // Missing timings are absent, not zero.
        let bare = throughput(Some(&json!({"turns": 1, "steps": 0}))).unwrap();
        assert_eq!(bare.ttft_ms, None);
        assert_eq!(bare.decode_tokens_per_second, None);
        assert!(throughput(Some(&json!({"turns": 1}))).is_none());
    }

    #[test]
    fn a_route_needs_both_a_provider_and_a_model() {
        let value = json!({
            "lastUsed": {"provider": "deepseek", "model": "v4", "reasoningEffort": "high"},
            "next": {"provider": "deepseek"},
        });
        let pair = model(Some(&value)).unwrap();
        let last = pair.last_used.unwrap();
        assert_eq!(last.provider, "deepseek");
        assert_eq!(last.model, "v4");
        assert_eq!(last.reasoning_effort.as_deref(), Some("high"));
        assert!(pair.next.is_none());
    }

    #[test]
    fn the_frame_reads_what_the_host_actually_composes() {
        // Shaped like a real `session/projections` answer.
        let values = json!({
            "title": "Drawer swipe jank",
            "permissions": {"currentValue": "workspace-write"},
            "tokenUsage": {"uncachedInputTokens": 10, "outputTokens": 5, "cacheReadTokens": 90},
            "contextPressure": {"contextWindow": 1000, "pressureTokens": 250},
            "contextBreakdown": {"systemTokens": 1, "toolsTokens": 2, "messageTokens": 3},
            "sessionStats": {"turns": 2, "steps": 3, "llmMs": 1000, "toolMs": 2000},
            "modelSelection": {"lastUsed": {"provider": "p", "model": "m"}, "next": null},
        });
        let frame = from_projections(&values);
        assert_eq!(frame.title.as_deref(), Some("Drawer swipe jank"));
        assert_eq!(frame.permission_preset.as_deref(), Some("workspace-write"));
        assert_eq!(frame.token_usage.as_ref().unwrap().billed_input_tokens, 100);
        assert_eq!(frame.context.as_ref().unwrap().percent, Some(25));
        assert_eq!(frame.throughput.as_ref().unwrap().turns, 2);
        assert_eq!(
            frame
                .model
                .as_ref()
                .unwrap()
                .last_used
                .as_ref()
                .unwrap()
                .model,
            "m"
        );
        assert!(frame.model.as_ref().unwrap().next.is_none());
    }

    #[test]
    fn the_frame_serialises_with_the_keys_the_app_decodes() {
        // The app decodes this by key name; a rename here is a silent blank
        // panel there, so the wire names are asserted rather than assumed.
        let frame = from_projections(&json!({
            "title": "t",
            "permissions": {"currentValue": "workspace-write"},
            "tokenUsage": {"uncachedInputTokens": 10, "outputTokens": 5, "cacheReadTokens": 90},
            "contextPressure": {"contextWindow": 1000, "pressureTokens": 250},
            "sessionStats": {"turns": 2, "steps": 3, "llmMs": 1000, "toolMs": 2000},
            "modelSelection": {"lastUsed": {"provider": "p", "model": "m"}},
        }));
        let wire = serde_json::to_value(&frame).unwrap();
        for key in [
            "tokenUsage",
            "cacheHit",
            "context",
            "stats",
            "model",
            "permissionPreset",
            "title",
        ] {
            assert!(wire.get(key).is_some(), "missing {key} in {wire}");
        }
        assert_eq!(wire["tokenUsage"]["billedInputTokens"], 100);
        assert_eq!(wire["context"]["pressureTokens"], 250);
        assert_eq!(wire["stats"]["turns"], 2);
        assert!(wire.get("throughput").is_none());
    }
}
