//! Normalized per-request token accounting, shared by every provider
//! parser, the conversation/session layers, and the event vocabulary.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Full prompt-side context of the request, including cache reads and
    /// cache writes. Every provider parser normalizes to this convention —
    /// `cached_tokens` and `cache_creation_tokens` are subsets.
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// Tokens served from cache (subset of prompt_tokens, cheaper pricing).
    #[serde(default)]
    pub cached_tokens: u64,
    /// Tokens written to cache this request (subset of prompt_tokens;
    /// Anthropic and GPT-5.6+ bill these at a premium — providers without a
    /// write concept leave 0).
    #[serde(default)]
    pub cache_creation_tokens: u64,
    /// Prompt-cache TTL implied by this response's cache writes (Anthropic
    /// 300s default, 3600s with the extended-TTL beta; GPT-5.6 1800s).
    /// `None` when the response makes no flavor statement — consumers keep
    /// the last known value or fall back to a provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl_seconds: Option<u32>,
    /// Provider rate-limit windows read from this response's headers
    /// (Anthropic `anthropic-ratelimit-*`; empty for providers or
    /// transports that expose none — the credential-egress relay strips
    /// headers).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rate_limit_windows: Vec<crate::vitals::SessionLimitWindow>,
}
