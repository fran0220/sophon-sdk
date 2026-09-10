//! SDK-to-native configuration boundaries. Never resolve model behavior here:
//! native runtime resolution remains the single authority for effective values.

use xai_grok_config_types::{MemoryEmbeddingSettings, MemorySettings};
use xai_grok_shell::agent::config::{Config, ModelEntry};

use crate::Error;
use crate::config::{AgentConfig, MemoryConfig, MemoryMode, ModelConfig};

fn memory_settings(config: &MemoryConfig) -> MemorySettings {
    MemorySettings {
        enabled: Some(config.enabled()),
        mode: Some(match config.mode {
            MemoryMode::Legacy => xai_grok_config_types::MemoryMode::Legacy,
            MemoryMode::Disabled | MemoryMode::V2 => xai_grok_config_types::MemoryMode::V2,
        }),
        // Empty model is the native explicit "no embeddings" value. In
        // particular, do not fall back to remote memory_embedding_model.
        embedding: Some(MemoryEmbeddingSettings {
            model: Some(String::new()),
            ..Default::default()
        }),
        index: Some(config.index.clone()),
        search: Some(config.search.clone()),
        initial_injection: Some(config.initial_injection.clone()),
        session: Some(config.session.clone()),
        watcher: Some(config.watcher.clone()),
        gc: Some(config.gc.clone()),
        dream: Some(config.dream.clone()),
    }
}

/// Call after hermetic loading and BEFORE `Config::new_from_toml_cfg`.
/// Only memory-owned tables are replaced; provider routes are never serialized
/// into a global fallback configuration.
pub(crate) fn apply_raw_config(config: &AgentConfig, raw: &mut toml::Value) -> Result<(), Error> {
    config.validate()?;
    let table = raw
        .as_table_mut()
        .ok_or_else(|| Error::invalid_config("native configuration must be a table"))?;
    let serialize = |value| {
        toml::Value::try_from(value)
            .map_err(|_| Error::invalid_config("unable to encode memory settings"))
    };
    table.insert("memory".into(), serialize(memory_settings(&config.memory))?);
    let compaction = table
        .entry("compaction")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| Error::invalid_config("native compaction configuration must be a table"))?;
    compaction.insert(
        "memory_flush".into(),
        toml::Value::try_from(&config.memory.flush)
            .map_err(|_| Error::invalid_config("unable to encode memory flush settings"))?,
    );
    compaction.insert(
        "pruning".into(),
        toml::Value::try_from(&config.memory.pruning)
            .map_err(|_| Error::invalid_config("unable to encode memory pruning settings"))?,
    );
    Ok(())
}

/// Call immediately AFTER `resolve_runtime_fields`. The enable bit is explicit
/// SDK policy, never an ambient environment flag. Tuning remains native-resolved.
/// Also pass `Some(config.memory.enabled())` as `memory_enabled_override` in the
/// resolution context so later native refreshes retain the explicit selection.
pub(crate) fn apply_runtime_config(config: &AgentConfig, native: &mut Config) {
    native.memory_enabled_override = Some(config.memory.enabled());
    if let Some(memory) = native.memory_config.as_mut() {
        memory.enabled = config.memory.enabled();
    }
}

/// Call for EACH explicitly routed model, before inserting it into the catalog.
/// This copies overrides only; do not substitute SDK defaults or pre-resolve
/// retries/timeouts here. Inference and switching must use the native resolver.
pub(crate) fn apply_model_config(config: &ModelConfig, entry: &mut ModelEntry) {
    let behavior = &config.behavior;
    let info = &mut entry.info;
    if let Some(value) = behavior.use_concise {
        info.use_concise = value;
    }
    if let Some(value) = &behavior.agent_type {
        info.agent_type.clone_from(value);
    }
    info.system_prompt_label
        .clone_from(&behavior.system_prompt_label);
    info.auto_compact_threshold_percent = behavior.auto_compact_threshold_percent;
    info.max_completion_tokens = behavior.max_completion_tokens;
    info.temperature = behavior.temperature;
    info.top_p = behavior.top_p;
    info.stream_tool_calls = behavior.stream_tool_calls;
    info.max_retries = config.retry.max_retries;
    info.rate_limit_retry_threshold = config.retry.rate_limit_retry_threshold;
    info.subagent_rate_limit_max_attempts = config.retry.subagent_rate_limit_max_attempts;
    info.inference_idle_timeout_secs = config.retry.inference_idle_timeout_secs;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelBehaviorConfig, ModelRetryConfig, ProviderConfig};
    use xai_grok_shell::agent::config::RuntimeResolutionContext;

    fn config() -> AgentConfig {
        AgentConfig::new(ModelConfig::new(
            "test",
            ProviderConfig::openai_chat("https://example.invalid/v1", "explicit", "wire-model"),
        ))
    }

    fn resolve(config: &AgentConfig) -> Config {
        assert!(xai_grok_config::set_hermetic_discovery(true));
        let mut raw = toml::Value::Table(Default::default());
        apply_raw_config(config, &mut raw).unwrap();
        let mut native = Config::new_from_toml_cfg(&raw).unwrap();
        native.resolve_runtime_fields(&RuntimeResolutionContext {
            raw_config: &raw,
            remote_settings: None,
            is_headless: true,
            cli_subagents: None,
            cli_web_search_model: None,
            cli_session_summary_model: None,
            memory_enabled_override: Some(config.memory.enabled()),
            disable_web_search: false,
            todo_gate: false,
            laziness_debug_log: None,
            storage_mode: None,
        });
        apply_runtime_config(config, &mut native);
        native
    }

    #[test]
    fn effective_memory_defaults_and_explicit_modes() {
        let default = resolve(&config()).memory_config.unwrap();
        assert!(default.enabled);
        assert!(default.mode.is_v2());
        assert!(default.embedding.model.is_none());
        for (mode, enabled, legacy) in [
            (MemoryMode::Disabled, false, false),
            (MemoryMode::Legacy, true, true),
            (MemoryMode::V2, true, false),
        ] {
            let effective = resolve(&config().memory_mode(mode)).memory_config.unwrap();
            assert_eq!(effective.enabled, enabled);
            assert_eq!(effective.mode.is_legacy(), legacy);
            assert_eq!(effective.index, default.index);
            assert!(effective.embedding.model.is_none());
        }
        let mut config = config();
        config.memory.index.max_chunk_chars = Some(4096);
        config.memory.search.max_results = Some(11);
        config.memory.flush.enabled = Some(false);
        let effective = resolve(&config).memory_config.unwrap();
        assert_eq!(effective.index.max_chunk_chars, 4096);
        assert_eq!(effective.search.max_results, 11);
        assert!(!effective.flush.enabled);
    }

    #[test]
    fn models_keep_distinct_native_overrides_without_changing_routes() {
        let native = resolve(&config());
        let mut first = ModelEntry::fallback("first", &native.endpoints);
        let mut second = ModelEntry::fallback("second", &native.endpoints);
        first.api_key = Some("first-key".into());
        second.api_key = Some("second-key".into());
        let first_route = first.base_url.clone();
        let mut model = config().models.remove(0);
        model.behavior = ModelBehaviorConfig {
            use_concise: Some(true),
            agent_type: Some("codex".into()),
            temperature: Some(0.25),
            ..Default::default()
        };
        model.retry = ModelRetryConfig {
            max_retries: Some(0),
            rate_limit_retry_threshold: Some(1),
            subagent_rate_limit_max_attempts: Some(2),
            inference_idle_timeout_secs: Some(20),
        };
        apply_model_config(&model, &mut first);
        model.behavior.use_concise = Some(false);
        model.retry.max_retries = Some(7);
        model.retry.inference_idle_timeout_secs = Some(90);
        apply_model_config(&model, &mut second);
        assert!(first.use_concise);
        assert!(!second.use_concise);
        assert_eq!(first.max_retries, Some(0));
        assert_eq!(second.max_retries, Some(7));
        // Exercise the actual inference retry resolver, not just catalog inputs.
        // Upstream still permits a process-wide override; do not mutate the
        // environment in this parallel test process to hide that precedence.
        let effective_first = xai_grok_sampler::resolve_max_retries(first.max_retries);
        let effective_second = xai_grok_sampler::resolve_max_retries(second.max_retries);
        if let Some(env) = std::env::var("GROK_MAX_RETRIES")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
        {
            assert_eq!((effective_first, effective_second), (env, env));
        } else {
            assert_eq!((effective_first, effective_second), (0, 7));
        }
        assert_eq!(first.inference_idle_timeout_secs, Some(20));
        assert_eq!(second.inference_idle_timeout_secs, Some(90));
        assert_eq!(first.base_url, first_route);
        assert_eq!(first.api_key.as_deref(), Some("first-key"));
        assert_eq!(second.api_key.as_deref(), Some("second-key"));
        assert_eq!(first.agent_type, "codex");
        let mut untouched = ModelEntry::fallback("untouched", &native.endpoints);
        let default_agent_type = untouched.agent_type.clone();
        apply_model_config(&config().models[0], &mut untouched);
        assert_eq!(untouched.agent_type, default_agent_type);
        assert_eq!(untouched.max_retries, None);
        assert_eq!(untouched.inference_idle_timeout_secs, None);
    }

    #[test]
    fn raw_memory_overlay_preserves_other_tables_and_blocks_embedding_fallback() {
        let mut raw: toml::Value = toml::from_str("[provider]\nmarker = 'untouched'\n[compaction]\nmarker = 42\n[memory.embedding]\nmodel = 'ambient'").unwrap();
        apply_raw_config(&config(), &mut raw).unwrap();
        assert_eq!(raw["provider"]["marker"].as_str(), Some("untouched"));
        assert_eq!(raw["compaction"]["marker"].as_integer(), Some(42));
        let memory: MemorySettings = raw["memory"].clone().try_into().unwrap();
        let remote = xai_grok_config_types::RemoteSettings {
            memory_embedding_model: Some("remote-embedding".into()),
            ..Default::default()
        };
        let effective = xai_grok_config_types::MemoryConfig::resolve_settings(
            Some(true),
            &memory,
            &Default::default(),
            &Default::default(),
            Some(&remote),
        );
        assert!(effective.embedding.model.is_none());
    }
}
