use super::*;
use crate::remote::DEFAULT_CONTEXT_WINDOW;
use xai_chat_state::conversation_util::replace_or_insert_system_head;
impl SessionActor {
    /// Credential-free facts from the same chat-state snapshot used for the
    /// route. Do not resolve the catalog again: model switches and effort
    /// routing can leave session-local values different from catalog defaults.
    pub(super) fn effective_model_facts(
        &self,
        sampling: &xai_grok_sampling_types::SamplingConfig,
    ) -> crate::session::commands::EffectiveModelFacts {
        let configured_max_retries = sampling.max_retries.unwrap_or(self.max_retries);
        let max_retries = if configured_max_retries == 0 {
            0
        } else {
            xai_grok_sampler::resolve_max_retries(Some(configured_max_retries))
        };
        let subagent_budget = self.rate_limit_wait_budget(sampling.rate_limit_retry_threshold);
        crate::session::commands::EffectiveModelFacts {
            max_completion_tokens: sampling.max_completion_tokens,
            temperature: sampling.temperature,
            top_p: sampling.top_p,
            stream_tool_calls: sampling.stream_tool_calls.unwrap_or(false),
            active_agent_type: self.agent.borrow().definition().name.clone(),
            auto_compact_threshold_percent: self.compaction.threshold_percent.get(),
            max_retries,
            rate_limit_retry_threshold: sampling.rate_limit_retry_threshold.unwrap_or_else(|| {
                super::spawn::subagent_sampler_rate_limit_threshold(
                    self.startup_hints.is_subagent,
                    self.rate_limit_waits.max_attempts,
                )
            }),
            subagent_rate_limit_max_attempts: subagent_budget.max_attempts(),
            subagent_rate_limit_max_total_wait_secs: if subagent_budget.can_wait() {
                self.rate_limit_waits.max_total_wait.as_secs()
            } else {
                0
            },
            inference_idle_timeout_secs: self.inference_idle_timeout.as_secs(),
            transient_retry_enabled: self.transient_retry_enabled,
            transient_retries_per_step: if self.transient_retry_enabled {
                MAX_TRANSIENT_TURN_RETRIES
            } else {
                0
            },
            transient_retries_per_prompt: if self.transient_retry_enabled {
                MAX_TRANSIENT_RETRIES_PER_PROMPT
            } else {
                0
            },
            transient_retry_window_secs: if self.transient_retry_enabled {
                MAX_TRANSIENT_RETRY_WINDOW.as_secs()
            } else {
                0
            },
            retry_only_before_output: self.tool_context.task_output_token_budget.is_some()
                || self.tool_context.sampler_retry_only_before_output,
        }
    }

    pub(super) async fn handle_set_session_model(
        self: &std::sync::Arc<Self>,
        sampling_config: xai_grok_sampler::SamplerConfig,
        use_concise: bool,
        is_family_switch: bool,
        apply_prompt_override: bool,
        skip_prompt_rewrite: bool,
        auto_compact_threshold_percent: u8,
    ) -> Result<acp::ModelId, acp::Error> {
        let mut sampling_config = sampling_config;
        if let Some(current) = self.chat_state_handle.get_sampling_config().await
            && let Some(id) = current.conversation_group_id
        {
            sampling_config.conversation_group_id = Some(id);
        }
        let model_id = acp::ModelId::new(sampling_config.model.clone());
        let new_context_window = self.compaction.context_window_override.unwrap_or_else(|| {
            std::num::NonZeroU64::new(sampling_config.context_window).unwrap_or_else(|| {
                std::num::NonZeroU64::new(DEFAULT_CONTEXT_WINDOW)
                    .expect("DEFAULT_CONTEXT_WINDOW is non-zero")
            })
        });
        let prev_threshold = self.compaction.threshold_percent.get();
        if prev_threshold != auto_compact_threshold_percent {
            tracing::info!(
                session_id = %self.session_info.id.0,
                new_model = %sampling_config.model,
                old_threshold = prev_threshold,
                new_threshold = auto_compact_threshold_percent,
                "auto_compact_threshold_percent updated for model switch"
            );
        }
        self.compaction
            .threshold_percent
            .set(auto_compact_threshold_percent);
        self.supports_backend_search
            .set(sampling_config.supports_backend_search);
        self.compactions_remaining
            .set(sampling_config.compactions_remaining);
        self.compaction_at_tokens
            .set(sampling_config.compaction_at_tokens);
        xai_grok_telemetry::unified_log::info(
            "backend_search: model switch",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "new_model": &sampling_config.model,
                "api_backend": format!("{:?}", sampling_config.api_backend),
                "supports_backend_search": sampling_config.supports_backend_search,
            })),
        );
        self.chat_state_handle
            .update_sampling_config(xai_grok_sampling_types::SamplingConfig {
                base_url: sampling_config.base_url.clone(),
                mtls_cert_dir: sampling_config.mtls_cert_dir.clone(),
                model: sampling_config.model.clone(),
                max_completion_tokens: sampling_config.max_completion_tokens,
                temperature: sampling_config.temperature,
                top_p: sampling_config.top_p,
                max_retries: Some(xai_grok_sampler::resolve_max_retries(
                    sampling_config.max_retries,
                )),
                rate_limit_retry_threshold: sampling_config.rate_limit_retry_threshold,
                api_backend: sampling_config.api_backend.clone(),
                extra_headers: sampling_config.extra_headers.clone(),
                conversation_group_id: sampling_config.conversation_group_id.clone(),
                query_params: sampling_config.query_params.clone(),
                env_http_headers: sampling_config.env_http_headers.clone(),
                context_window: new_context_window,
                reasoning_effort: sampling_config.reasoning_effort,
                stream_tool_calls: Some(sampling_config.stream_tool_calls),
            });
        let config_version = self.tool_context.config_clock.bump();
        self.broadcast_effective_config_changed(config_version);
        let existing = self.chat_state_handle.get_credentials().await;
        let session_key = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current_or_expired().map(|a| a.key));
        self.chat_state_handle
            .update_credentials(xai_chat_state::Credentials {
                api_key: sampling_config.api_key.clone(),
                auth_type: crate::agent::config::resolve_chat_state_auth_type(
                    sampling_config.model.as_str(),
                    session_key.as_deref(),
                    existing.auth_type,
                ),
                alpha_test_key: existing.alpha_test_key,
                client_version: sampling_config.client_version.clone(),
            });
        self.invalidate_model_auth_memo();
        self.signals_handle()
            .record_model_usage(&sampling_config.model);
        if apply_prompt_override && !skip_prompt_rewrite {
            let mut conversation = self.chat_state_handle.get_conversation().await;
            for item in conversation.iter_mut() {
                if let ConversationItem::System(sys) = item {
                    if let Some(prompt) = self.explicit_system_prompt.borrow().as_deref() {
                        sys.content = std::sync::Arc::<str>::from(prompt);
                    } else if use_concise {
                        sys.content = std::sync::Arc::<str>::from(
                            xai_grok_agent::prompt::template::COMPACT_SYSTEM_PROMPT,
                        );
                    } else {
                        sys.content =
                            std::sync::Arc::<str>::from(self.agent.borrow().system_prompt());
                    }
                    break;
                }
            }
            self.chat_state_handle.replace_conversation(conversation);
        } else if !apply_prompt_override {
            tracing::info!(
                session_id = %self.session_info.id.0,
                model_id = %model_id.0,
                "handle_set_session_model: skipping prompt override (apply_prompt_override=false)"
            );
        } else {
            tracing::info!(
                session_id = %self.session_info.id.0,
                model_id = %model_id.0,
                "handle_set_session_model: skipping prompt rewrite (just rebuilt harness)"
            );
        }
        let agent_name = self.agent.borrow().definition().name.clone();
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::CurrentModel {
                model_id: model_id.clone(),
                agent_name: Some(agent_name),
                reasoning_effort: Some(sampling_config.reasoning_effort),
            });
        self.emit_status_snapshot_detached();
        let turn_in_flight = self.state.lock().await.running_task.is_some();
        if turn_in_flight && is_family_switch {
            tracing::warn!("Family-switch compact skipped: turn in flight");
        }
        if is_family_switch && !turn_in_flight && self.history_has_model_minted_items().await {
            self.abort_and_clear_prefire().await;
            let estimated_total_tokens = self.chat_state_handle.get_estimated_total_tokens().await;
            let context_window = new_context_window.get();
            let trigger_info = compaction::AutoCompactTriggerInfo {
                tokens_used: estimated_total_tokens,
                context_window,
                percentage: xai_token_estimation::usage_percentage_u8(
                    estimated_total_tokens,
                    context_window,
                ),
            };
            tracing::info!("Family-switch compact: -> {}", sampling_config.model);
            if let Err(e) = self.run_compact_only(trigger_info, true).await {
                tracing::error!(error = %e, "Family-switch compaction failed; switching anyway");
            }
        }
        Ok(model_id)
    }
    /// Set the reasoning effort on the live sampling config, applying the same
    /// support check and per-effort model routing as `apply_supported_effort`.
    pub(super) async fn handle_set_reasoning_effort(
        self: &std::sync::Arc<Self>,
        effort: xai_grok_sampling_types::ReasoningEffort,
    ) -> Result<acp::ModelId, acp::Error> {
        let Some(mut cfg) = self.chat_state_handle.get_sampling_config().await else {
            return Err(acp::Error::internal_error().data("session has no sampling config"));
        };
        if !self
            .models_manager
            .model_supports_reasoning_effort(&cfg.model)
        {
            return Err(acp::Error::invalid_params()
                .data("the session's current model does not support reasoning effort"));
        }
        if let Some(routed) = self.models_manager.model_for_effort(&cfg.model, effort) {
            cfg.model = routed;
        }
        cfg.reasoning_effort = Some(effort);
        let model_id = acp::ModelId::new(cfg.model.clone());
        self.chat_state_handle.update_sampling_config(cfg);
        self.invalidate_model_auth_memo();
        let config_version = self.tool_context.config_clock.bump();
        self.broadcast_effective_config_changed(config_version);
        let agent_name = self.agent.borrow().definition().name.clone();
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::CurrentModel {
                model_id: model_id.clone(),
                agent_name: Some(agent_name),
                reasoning_effort: Some(Some(effort)),
            });
        self.emit_status_snapshot_detached();
        Ok(model_id)
    }
    /// Handle [`SessionCommand::RebuildAgentForDefinition`].
    /// Builds a fresh [`xai_grok_agent::Agent`] from the cached [`crate::session::agent_rebuild::AgentRebuildSpec`] and the supplied definition.
    /// Triggered from `MvpAgent::set_session_model` only when the new model's `agent_type` differs from the session's `active_agent_type`.
    pub(super) async fn handle_rebuild_agent_for_definition(
        &self,
        definition: xai_grok_agent::AgentDefinition,
    ) -> Result<(), acp::Error> {
        {
            let state = self.state.lock().await;
            if state.running_task.is_some() {
                tracing::warn!(
                    session_id = %self.session_info.id.0,
                    new_agent_type = %definition.name,
                    "handle_rebuild_agent_for_definition: turn in flight, rejecting rebuild"
                );
                return Err(acp::Error::internal_error()
                    .data("rebuild_agent: turn in flight, refusing to rebuild harness"));
            }
        }
        let new_agent_name = definition.name.clone();
        tracing::info!(
            session_id = %self.session_info.id.0,
            new_agent_type = %new_agent_name,
            "handle_rebuild_agent_for_definition: rebuilding harness"
        );
        let new_agent = self
            .rebuild_spec
            .build_agent(definition)
            .await
            .map_err(|e| {
                tracing::error!(
                    session_id = %self.session_info.id.0,
                    new_agent_type = %new_agent_name,
                    error = %e,
                    "handle_rebuild_agent_for_definition: AgentBuilder::build failed"
                );
                acp::Error::internal_error().data(format!(
                    "rebuild_agent: build failed for agent_type={new_agent_name}: {e}"
                ))
            })?;
        let new_system_prompt = self
            .explicit_system_prompt
            .borrow()
            .clone()
            .unwrap_or_else(|| new_agent.system_prompt().to_string());
        let mut new_prompt_context = new_agent.prompt_context().clone();
        new_prompt_context.normalize_for_persistence();
        self.abort_and_clear_prefire().await;
        *self.agent.borrow_mut() = new_agent;
        *self.active_agent_type.lock() = Some(new_agent_name.clone());
        self.emit_resolved_tool_overrides();
        self.queue_exit_reminder_on_approved_exit.store(
            self.is_cursor_harness(),
            std::sync::atomic::Ordering::Relaxed,
        );
        if let Err(e) = self.workspace_ops.bind_local_session(
            &self.session_id_string(),
            self.tool_context.cwd.as_path().to_path_buf(),
            self.tool_context.hunk_tracker_handle.clone(),
            self.agent.borrow().tool_bridge().toolset(),
            None,
        ) {
            tracing::warn!(error = %e, "failed to rebind local session toolset after agent rebuild");
        }
        {
            let bridge = self.agent.borrow().tool_bridge().clone();
            let snapshot = self.tool_metadata_snapshot.clone();
            let tool_index = crate::session::tool_index::Bm25ToolSearchIndex::new(snapshot);
            bridge
                .update_resource(xai_grok_tools::types::tool_index::ToolIndex(
                    std::sync::Arc::new(tool_index),
                ))
                .await;
            if let Some(client) = self.rebuild_spec.managed_gateway_tool_client.clone() {
                bridge.update_resource(client).await;
            }
            let plan_path = self.plan_mode.lock().plan_file_path().to_path_buf();
            bridge
                .update_resource(xai_grok_tools::types::resources::PlanFilePath(plan_path))
                .await;
            if let Some(display_cwd) = self.display_cwd.get() {
                bridge
                    .set_display_cwd(std::path::PathBuf::from(display_cwd))
                    .await;
            }
            bridge
                .update_resource(
                    xai_grok_tools::implementations::grok_build::workflow::WorkflowLaunchHandle(
                        self.workflow_launch_tx.clone(),
                    ),
                )
                .await;
            if !self.goal_runs_on_workflow_engine() {
                bridge
                    .update_resource(
                        xai_grok_tools::implementations::grok_build::update_goal::GoalUpdateHandle(
                            self.goal_update_tx.clone(),
                        ),
                    )
                    .await;
            }
            if let Some(reservations) = self.tool_context.task_completion_reservations.clone() {
                bridge.update_resource(reservations).await;
            }
            if let Some(gate) = self.tool_context.task_wake_suppressed.clone() {
                bridge.update_resource(gate).await;
            }
            self.inject_deny_read_globs().await;
        }
        let claim = self.restart_mcp_init(&mut *self.mcp_state.lock().await);
        self.re_register_mcp_tools_on_rebuilt_bridge().await;
        self.run_mcp_init_with_claim(claim).await;
        self.deferred_prefix.cancel();
        let new_user_prefix = self
            .build_prefix_after_mcp_wait(self.requires_full_mcp_wait())
            .await;
        {
            let mut conversation = self.chat_state_handle.get_conversation().await;
            let _ = replace_or_insert_system_head(&mut conversation, &new_system_prompt);
            let drop_startup_skill_reminder = false;
            Self::rewrite_zero_turn_prefix(
                &mut conversation,
                new_user_prefix,
                drop_startup_skill_reminder,
            );
            if !conversation_has_project_instructions(&conversation)
                && let Some(agents_md_reminder) = self.agent.borrow().agents_md_user_reminder()
            {
                let agents_md_at = conversation.len().min(2);
                conversation.insert(
                    agents_md_at,
                    ConversationItem::project_instructions(agents_md_reminder),
                );
            }
            self.inject_baseline_skill_reminder(&mut conversation).await;
            self.chat_state_handle.replace_conversation(conversation);
        }
        save_prompt_context(&self.session_info, &new_prompt_context);
        save_system_prompt(&self.session_info, &new_system_prompt);
        let snapshot = self.chat_state_handle.get_conversation().await;
        persist_chat_history_jsonl_sync(&self.session_info, &snapshot);
        self.mcp_reminder_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.send_available_commands_update(AdvertiseTrigger::HarnessRebuild)
            .await;
        tracing::info!(
            session_id = %self.session_info.id.0,
            new_agent_type = %new_agent_name,
            "handle_rebuild_agent_for_definition: harness rebuild complete"
        );
        Ok(())
    }
    /// Apply a client-supplied `systemPromptOverride` on attach without wiping user/assistant history: swap only the leading `System` message.
    /// The swap happens atomically inside the `ChatStateActor`.
    /// `system_prompt.txt` (not owned by the persistence actor) is saved directly, even on a head no-op, so a diverged secondary artifact self-heals.
    pub(super) async fn handle_replace_system_prompt(&self, system_prompt: String) {
        if self.startup_hints.preserve_inherited_system {
            tracing::debug!(
                session_id = %self.session_info.id.0,
                "handle_replace_system_prompt: skipped (preserve_inherited_system)"
            );
            return;
        }
        let Some(changed) = self
            .chat_state_handle
            .replace_system_head(&system_prompt)
            .await
        else {
            tracing::error!(
                session_id = %self.session_info.id.0,
                "handle_replace_system_prompt: chat-state actor unavailable; override not applied"
            );
            return;
        };
        save_system_prompt(&self.session_info, &system_prompt);
        *self.explicit_system_prompt.borrow_mut() = Some(system_prompt.clone());
        if changed {
            tracing::info!(
                session_id = %self.session_info.id.0,
                prompt_len = system_prompt.len(),
                "handle_replace_system_prompt: client override applied"
            );
        } else {
            tracing::debug!(
                session_id = %self.session_info.id.0,
                "handle_replace_system_prompt: head already matches, no-op"
            );
        }
    }
    /// Whether the conversation has anything a family switch must compact away.
    async fn history_has_model_minted_items(&self) -> bool {
        self.chat_state_handle
            .get_conversation()
            .await
            .iter()
            .any(|item| {
                matches!(
                    item,
                    xai_grok_sampling_types::ConversationItem::Assistant(_)
                        | xai_grok_sampling_types::ConversationItem::Reasoning(_)
                        | xai_grok_sampling_types::ConversationItem::BackendToolCall(_)
                )
            })
    }
    /// Abort and join an in-flight prefire pass-1 and drop its NOTE1 cache.
    pub(super) async fn abort_and_clear_prefire(&self) {
        if let Some(handle) = self.compaction.prefire.take_handle() {
            handle.abort();
            let _ = handle.await;
            self.compaction.prefire.finish();
        }
        self.compaction.prefire.clear();
    }
}

#[cfg(test)]
mod effective_model_facts_tests {
    use super::*;

    #[test]
    fn asymmetric_sampling_values_follow_model_switch_not_catalog_defaults() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(tokio::task::LocalSet::new().run_until(async {
                    let actor = super::super::support::actor_with_persistence_drain().await;
                    let idle_timeout = actor.inference_idle_timeout.as_secs();
                    let mut config = actor.reconstruct_full_config().await;
                    for (model, temperature, top_p, tokens, context, threshold) in [
                        ("model-a", Some(0.25), None, Some(1234), 100_000, 3),
                        ("model-b", None, Some(0.8), None, 200_000, 7),
                    ] {
                        config.model = model.to_string();
                        config.temperature = temperature;
                        config.top_p = top_p;
                        config.max_completion_tokens = tokens;
                        config.context_window = context;
                        config.rate_limit_retry_threshold = Some(threshold);
                        config.reasoning_effort = None;
                        actor
                            .handle_set_session_model(config.clone(), false, false, false, true, 81)
                            .await
                            .unwrap();
                        let sampling = actor.chat_state_handle.get_sampling_config().await.unwrap();
                        let facts = actor.effective_model_facts(&sampling);
                        assert_eq!(sampling.model, model);
                        assert_eq!(sampling.context_window.get(), context);
                        assert_eq!(facts.temperature, temperature);
                        assert_eq!(facts.top_p, top_p);
                        assert_eq!(facts.max_completion_tokens, tokens);
                        assert_eq!(facts.rate_limit_retry_threshold, threshold);
                        assert_eq!(facts.auto_compact_threshold_percent, 81);
                        // These are actor-spawn policy, not the new model's inputs.
                        assert_eq!(facts.inference_idle_timeout_secs, idle_timeout);
                        assert_eq!(facts.subagent_rate_limit_max_attempts, 0);
                        let reconstructed = actor.reconstruct_full_config().await;
                        assert_eq!(facts.temperature, reconstructed.temperature);
                        assert_eq!(facts.top_p, reconstructed.top_p);
                        assert_eq!(
                            facts.max_completion_tokens,
                            reconstructed.max_completion_tokens
                        );
                    }
                    let mut sampling = actor.chat_state_handle.get_sampling_config().await.unwrap();
                    sampling.max_retries = Some(0);
                    assert_eq!(actor.effective_model_facts(&sampling).max_retries, 0);
                    sampling.max_retries = Some(4);
                    assert_eq!(
                        actor.effective_model_facts(&sampling).max_retries,
                        xai_grok_sampler::resolve_max_retries(Some(4)),
                    );
                }));
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
