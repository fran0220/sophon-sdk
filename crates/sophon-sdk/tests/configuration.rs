use sophon_sdk::{
    AgentConfig, MemoryConfig, MemoryMode, ModelBehaviorConfig, ModelConfig, ModelRetryConfig,
    ProviderConfig,
};

fn config() -> AgentConfig {
    AgentConfig::new(ModelConfig::new(
        "main",
        ProviderConfig::openai_chat("https://example.invalid/v1", "explicit-key", "wire-model"),
    ))
}

#[test]
fn memory_selection_is_explicit_and_v2_by_default() {
    assert_eq!(config().memory.mode, MemoryMode::V2);
    assert!(config().memory.enabled());
    assert!(!config().memory_enabled(false).memory.enabled());
    assert_eq!(
        config()
            .memory_enabled(false)
            .memory_enabled(true)
            .memory
            .mode,
        MemoryMode::V2
    );
    let legacy = config().memory(MemoryConfig::new(MemoryMode::Legacy));
    assert_eq!(legacy.memory.mode, MemoryMode::Legacy);
    assert!(legacy.validate().is_ok());
}

#[test]
fn memory_flush_cannot_discover_an_unconfigured_provider() {
    let mut config = config();
    config.memory.flush.flush_model = Some("ambient-model".into());
    assert!(config.validate().is_err());
    config.memory.flush.flush_model = Some("main".into());
    assert!(config.validate().is_ok());
}

#[test]
fn memory_validation_checks_effective_chunk_defaults_and_finite_scores() {
    let mut config = config();
    // The omitted overlap still has an upstream default; this would produce
    // chunks smaller than their overlap without effective-value validation.
    config.memory.index.max_chunk_chars = Some(1);
    assert!(config.validate().is_err());
    config.memory.index.chunk_overlap_chars = Some(0);
    assert!(config.validate().is_ok());
    config.memory.search.min_score = Some(f32::NAN);
    assert!(config.validate().is_err());
}

#[test]
fn retry_counts_and_behavior_are_validated_per_model() {
    let model = ModelConfig::new(
        "second",
        ProviderConfig::anthropic("https://other.invalid/v1", "other-key", "other-model"),
    )
    .behavior(ModelBehaviorConfig {
        use_concise: Some(true),
        top_p: Some(0.5),
        ..Default::default()
    })
    .retry(ModelRetryConfig {
        max_retries: Some(0),
        rate_limit_retry_threshold: Some(1),
        inference_idle_timeout_secs: Some(10),
        ..Default::default()
    });
    let mut config = config().model(model);
    assert!(config.validate().is_ok());
    config.models[1].retry.rate_limit_retry_threshold = Some(0);
    assert!(config.validate().is_err());
    config.models[1].retry.rate_limit_retry_threshold = Some(1);
    config.models[1].behavior.temperature = Some(f32::INFINITY);
    assert!(config.validate().is_err());
    config.models[1].behavior.temperature = Some(0.5);
    config.models[1].behavior.auto_compact_threshold_percent = Some(101);
    assert!(config.validate().is_err());
}
