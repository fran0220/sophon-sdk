//! Credential-free values actually installed in a native session.
//!
//! These are not catalog/requested overrides. In particular, idle timeout and
//! turn-level retry policy are session-spawn state and need not change when the
//! selected model changes. Model identity, context window, and reasoning effort
//! are reported by the route from the same actor snapshot.

/// Resolved sampling, active harness, and retry behavior for a session.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionModelFacts {
    /// `None` means omitted from the provider request, not a guessed provider default.
    pub max_completion_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stream_tool_calls: bool,
    /// Actual installed harness name, rather than the catalog's requested label.
    pub active_agent_type: String,
    pub auto_compact_threshold_percent: u8,
    /// Sampler transport retry budget, including the native environment override.
    pub max_retries: u32,
    /// Total-attempt ceiling for sampler-owned 429 retries.
    pub rate_limit_retry_threshold: u32,
    /// Active session's outer 429 wait loop; zero for main sessions or when disabled.
    pub subagent_rate_limit_max_attempts: u32,
    pub subagent_rate_limit_max_total_wait_secs: u64,
    /// Per-chunk idle timeout, not a whole-turn deadline.
    pub inference_idle_timeout_secs: u64,
    pub transient_retry_enabled: bool,
    pub transient_retries_per_step: u32,
    pub transient_retries_per_prompt: u32,
    pub transient_retry_window_secs: u64,
    pub retry_only_before_output: bool,
}

impl From<xai_grok_shell::session::commands::EffectiveModelFacts> for SessionModelFacts {
    fn from(value: xai_grok_shell::session::commands::EffectiveModelFacts) -> Self {
        Self {
            max_completion_tokens: value.max_completion_tokens,
            temperature: value.temperature,
            top_p: value.top_p,
            stream_tool_calls: value.stream_tool_calls,
            active_agent_type: value.active_agent_type,
            auto_compact_threshold_percent: value.auto_compact_threshold_percent,
            max_retries: value.max_retries,
            rate_limit_retry_threshold: value.rate_limit_retry_threshold,
            subagent_rate_limit_max_attempts: value.subagent_rate_limit_max_attempts,
            subagent_rate_limit_max_total_wait_secs: value.subagent_rate_limit_max_total_wait_secs,
            inference_idle_timeout_secs: value.inference_idle_timeout_secs,
            transient_retry_enabled: value.transient_retry_enabled,
            transient_retries_per_step: value.transient_retries_per_step,
            transient_retries_per_prompt: value.transient_retries_per_prompt,
            transient_retry_window_secs: value.transient_retry_window_secs,
            retry_only_before_output: value.retry_only_before_output,
        }
    }
}
