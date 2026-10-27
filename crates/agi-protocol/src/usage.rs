//! The usage frame a phone's details panel draws.
//!
//! Ported from the plugin that served the same phone before the desktop existed
//! (`dsh` v0.8.12, `src/bridge/stats.ts`). The shape is the phone's contract, not
//! this project's taste: every field is optional because each one comes from a
//! separate host projection, and a projection answers with what it has *right
//! now*. A frame carrying fewer fields than the last one is the ordinary case,
//! which is why the app merges one into the last rather than replacing it.
//!
//! A missing field means "this host cannot say", never zero.

use serde::{Deserialize, Serialize};

/// Everything the usage screen renders.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsage {
    /// Cumulative billed tokens, split the way providers bill them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
    /// How much of the input was served from the prompt cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hit: Option<CacheHit>,
    /// How full the context window is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<UsageContext>,
    /// Throughput and latency, folded from the durable log.
    #[serde(rename = "stats", skip_serializing_if = "Option::is_none")]
    pub throughput: Option<Throughput>,
    /// The route the last request used, and any pending change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelPair>,
    /// The session's permission preset, as the host names it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_preset: Option<String>,
    /// The session title, once one has been assigned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Cumulative billed tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    /// Input tokens billed at full rate.
    pub uncached_input_tokens: u64,
    /// Tokens produced by the model.
    pub output_tokens: u64,
    /// Input tokens served from the provider's prompt cache.
    pub cache_read_tokens: u64,
    /// Input tokens written into the provider's prompt cache.
    pub cache_write_tokens: u64,
    /// `uncached + cacheRead + cacheWrite`.
    pub billed_input_tokens: u64,
}

/// Cache effectiveness, formatted by the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheHit {
    /// Exact ratio in `[0, 1]`, or `None` when nothing has been billed yet.
    pub ratio: Option<f64>,
    /// Display-ready percentage that never overstates the saving.
    ///
    /// A string because "99.96 rounds to 100" is exactly the claim a cache report
    /// must not make; the host picks the precision and the app must not re-round.
    pub display_percent: Option<String>,
}

/// How much of the context window a session is using.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageContext {
    /// The model's context window, when the provider reported one.
    pub window: Option<u64>,
    /// Newest provider-reported prompt size.
    pub pressure_tokens: Option<u64>,
    /// What the next request's prompt would cost.
    pub projected_tokens: Option<u64>,
    /// Occupancy percentage, clamped to 100, or `None` when uncomputable.
    pub percent: Option<u64>,
    /// Approximate composition; never sums to `projectedTokens`.
    pub breakdown: Option<ContextBreakdown>,
}

/// Approximate composition of the context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextBreakdown {
    pub system_tokens: u64,
    pub tools_tokens: u64,
    pub message_tokens: u64,
}

/// Throughput and latency folded from the durable log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Throughput {
    pub turns: u64,
    pub steps: u64,
    /// Milliseconds spent waiting on the model, summed over steps.
    pub llm_ms: i64,
    /// Milliseconds spent in tool calls, summed.
    pub tool_ms: i64,
    /// Time to first token, averaged over the steps that reported one.
    pub ttft_ms: Option<f64>,
    /// Output tokens per second while decoding.
    pub decode_tokens_per_second: Option<f64>,
}

/// The route the last request actually used, and any pending change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPair {
    pub last_used: Option<ModelSelection>,
    pub next: Option<ModelSelection>,
}

/// One provider route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}
