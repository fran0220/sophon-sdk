use super::*;
use crate::session::config_candidate::{ConfigCandidate, MountedConfig};
use xai_grok_tools::registry::types::{FinalizedToolset, PreparedMcpTool};
use xai_grok_tools::types::resources::AvailableSkills;
use xai_grok_tools::types::skill_discovery_tracker::SkillManager;

struct PreparedCandidate {
    mounted: MountedConfig,
    model: crate::agent::config::ModelEntry,
    sampling: crate::sampling::SamplerConfig,
    auth_type: xai_chat_state::AuthType,
    auto_compact_threshold_percent: u8,
    config_version: xai_prompt_queue::QueueVersion,
    skill_revision: u64,
    plugin_revision: u64,
    plugin_registry: Option<Arc<xai_grok_agent::plugins::PluginRegistry>>,
    hook_registry: Option<Arc<xai_grok_hooks::discovery::HookRegistry>>,
    hook_load_errors: Vec<String>,
    client_hooks: crate::extensions::hooks::ClientHooks,
    skills: SkillManager,
    runtime_skills: AvailableSkills,
    skill_effects: xai_grok_tools::types::skill_discovery_tracker::SkillUpdateEffects,
    mcp_generation: crate::session::mcp_servers::Generation,
    mcp: McpState,
    tools: Vec<PreparedMcpTool>,
    catalog: super::mcp_snapshot::McpCatalog,
}

fn candidate_error(message: impl Into<String>) -> acp::Error {
    acp::Error::invalid_params().data(serde_json::json!({
        "code": "config_candidate_rejected", "message": message.into(),
    }))
}

impl SessionActor {
    pub(super) async fn seal_inherited_config(&self, mounted: MountedConfig) -> Result<crate::session::commands::SubagentParentSnapshot, acp::Error> {
        if !self.startup_hints.is_subagent {
            return Err(candidate_error("only a native child can inherit a mounted configuration"));
        }
        let generation = self.candidate_admission.generation();
        let cancelled = self.candidate_admission.begin(generation)
            .ok_or_else(|| candidate_error("child admission closed"))?;
        let ready = tokio::select! {
            biased;
            () = cancelled.cancelled() => Err(candidate_error("child inheritance cancelled")),
            () = self.ensure_prefix_ready() => Ok(()),
        };
        self.candidate_admission.finish(generation);
        ready?;
        let toolset = self.agent.borrow().tool_bridge().toolset();
        let mut mcp = self.mcp_state.lock().await;
        let mut resources = toolset.resources.lock().await;
        let hook_disabled = mounted.hook_disabled.clone();
        if !self.candidate_admission.inherit(generation, mounted, || {
            *self.hook_disabled.borrow_mut() = hook_disabled;
            mcp.freeze_mounted_snapshot();
            if let Some(skills) = resources.get_mut::<SkillManager>() {
                skills.set_baseline_frozen(true);
            }
        }) {
            return Err(candidate_error("child configuration already sealed or closed"));
        }
        drop(resources);
        drop(mcp);
        Ok(self.snapshot_subagent_parent().await)
    }

    pub(super) async fn snapshot_subagent_parent(
        &self,
    ) -> crate::session::commands::SubagentParentSnapshot {
        let definitions = self.prepare_tool_definitions_inner().await;
        let bridge = self.agent.borrow().tool_bridge().clone();
        let skills = bridge
            .read_resource::<AvailableSkills>()
            .await
            .map(|skills| skills.0);
        let mcp = self.mcp_state.lock().await;
        crate::session::commands::SubagentParentSnapshot {
            mounted: self.candidate_admission.mounted(),
            toolset: bridge.toolset(),
            tool_definitions: self.turn_base_tool_specs(&definitions),
            mcp_pool: if mcp.owned_clients.is_empty() && mcp.shared_clients.is_empty() {
                None
            } else {
                Some(crate::session::mcp_servers::SharedMcpPool::from_state(&mcp))
            },
            client_hooks: self.client_hooks.borrow().clone(),
            plugin_registry: self.plugin_registry.borrow().clone(),
            hook_registry: self.hook_registry.borrow().clone(),
            skills,
        }
    }

    pub(super) async fn admit_candidate_command(
        &self,
        command: SessionCommand,
    ) -> Option<SessionCommand> {
        let mounted = self.candidate_admission.mounted().is_some();
        if self.candidate_admission.required() && !mounted {
            match command {
                SessionCommand::Prompt { respond_to, .. } => {
                    let _ = respond_to.send(Err(candidate_error("session requires a configuration candidate before execution")));
                    return None;
                }
                // Dropping the reply rejects child admission at its existing
                // snapshot boundary, rather than supplying an unmounted parent.
                SessionCommand::SnapshotSubagentParent { .. } => return None,
                _ => {}
            }
        }
        if mounted || self.candidate_admission.required() {
            let error =
                || candidate_error("mounted configuration must be replaced by a prompt candidate");
            match command {
                SessionCommand::SetSessionModel { responds_to, .. }
                | SessionCommand::SetReasoningEffort { responds_to, .. } => {
                    let _ = responds_to.send(Err(error()));
                    return None;
                }
                SessionCommand::RebuildAgentForDefinition { responds_to, .. } => {
                    let _ = responds_to.send(Err(error()));
                    return None;
                }
                SessionCommand::UpdateMcpServers { respond_to, .. }
                | SessionCommand::ToggleMcpServer { respond_to, .. }
                | SessionCommand::ToggleMcpTool { respond_to, .. } => {
                    let _ = respond_to.send(Err(error()));
                    return None;
                }
                SessionCommand::SetClientHooks { hooks } if mounted => {
                    self.candidate_admission.defer_client_hooks_if_mounted(hooks);
                    return None;
                }
                SessionCommand::McpAuthTrigger { respond_to, .. } => {
                    let _ = respond_to.send(Err("MCP authentication changes require a new configuration candidate".into()));
                    return None;
                }
                SessionCommand::RetryAuthRequiredServers { .. } => return None,
                SessionCommand::HooksAction { respond_to, .. } => {
                    let _ = respond_to.send(xai_hooks_plugins_types::ActionOutcome {
                        status: xai_hooks_plugins_types::OutcomeStatus::ValidationError,
                        message: "Mounted hooks can only change through a configuration candidate".into(),
                        requires_reload: false,
                        requires_restart: false,
                    });
                    return None;
                }
                SessionCommand::OverrideModelName { .. }
                | SessionCommand::ReplaceSystemPrompt { .. } => {
                    tracing::warn!("ignored standalone configuration override for mounted session");
                    return None;
                }
                _ => {}
            }
        }
        let (mut prompt, candidate, generation) = match command {
            SessionCommand::PromptCandidate {
                prompt,
                candidate,
                generation,
            } => (*prompt, candidate, generation),
            command => return Some(command),
        };
        let SessionCommand::Prompt { ref respond_to, .. } = prompt else {
            return None;
        };
        if respond_to.is_closed() {
            return None;
        }
        {
            let state = self.state.lock().await;
            if state.running_task.is_some() || !state.pending_inputs.is_empty() {
                return Some(prompt);
            }
        }
        let result = if let Some(cancelled) = self.candidate_admission.begin(generation) {
            let prefix_ready = tokio::select! {
                biased;
                () = cancelled.cancelled() => Err(candidate_error("candidate cancelled before preparation")),
                () = self.tool_context.admission.wait_until_closed() => Err(candidate_error("agent admission closed")),
                () = self.ensure_prefix_ready() => Ok(()),
            };
            match prefix_ready {
                // mount_candidate cancels preparation itself. Once chat install
                // is submitted, it must finish the native publication tail;
                // dropping that future could expose a partial snapshot.
                Ok(()) => Box::pin(self.mount_candidate(candidate, cancelled)).await,
                Err(error) => Err(error),
            }
        } else {
            Err(candidate_error("candidate superseded by cancellation"))
        };
        self.candidate_admission.finish(generation);
        match result {
            Ok(permit) => {
                if let SessionCommand::Prompt {
                    ref mut agent_admission,
                    ..
                } = prompt
                {
                    *agent_admission = Some(permit);
                }
                Some(prompt)
            }
            Err(error) => {
                if let SessionCommand::Prompt { respond_to, .. } = prompt {
                    let _ = respond_to.send(Err(error));
                }
                None
            }
        }
    }

    async fn prepare_candidate(
        &self,
        candidate: ConfigCandidate,
    ) -> Result<PreparedCandidate, acp::Error> {
        if !candidate.subject_options.is_empty() {
            return Err(candidate_error(
                "subjectOptions has no supported native options",
            ));
        }
        let config_version = self.tool_context.config_clock.snapshot();
        let (plugin_revision, plugin_registry) = self
            .candidate_admission
            .plugins_for_preparation(self.plugin_registry.borrow().clone());
        let client_hooks = self.candidate_admission
            .client_hooks_for_preparation(self.client_hooks.borrow().clone());
        let (model, sampling, auth_type) = self
            .models_manager
            .prepare_published_model(&candidate.model, candidate.reasoning_effort.as_deref())?;
        if model.info().agent_type != self.agent.borrow().definition().name {
            return Err(candidate_error(
                "candidate requires a different native agent harness",
            ));
        }
        let mut config = self.models_manager.native_config_snapshot();
        let auto_compact_threshold_percent = crate::util::config::resolve_auto_compact_threshold_percent(
            &config, &candidate.model, Some(model.info()),
        );
        let mut seen = std::collections::HashSet::new();
        for brief in &candidate.subagent_briefs {
            if !seen.insert(&brief.name) {
                return Err(candidate_error("duplicate subagent brief"));
            }
            let definition = config
                .registered_subagents
                .iter_mut()
                .chain(config.cli_agents.iter_mut())
                .find(|definition| definition.name == brief.name)
                .ok_or_else(|| {
                    candidate_error(format!(
                        "subagent brief requires a registered profile: {}",
                        brief.name
                    ))
                })?;
            if let Some(model) = &brief.model {
                self.models_manager.prepare_published_model(model, None)?;
                definition.model = xai_grok_agent::config::ModelOverride::Override(model.clone());
            }
            definition.description = brief.description.clone();
            definition.prompt_body = Some(brief.instructions.clone());
        }
        let cwd = std::path::Path::new(&self.session_info.cwd);
        for directory in &candidate.skill_directories {
            let path = std::path::Path::new(directory);
            if !path.is_absolute()
                || !tokio::fs::metadata(path)
                    .await
                    .is_ok_and(|metadata| metadata.is_dir())
            {
                return Err(candidate_error(format!(
                    "skillDirectories requires an existing absolute directory: {directory}"
                )));
            }
            if !config.skills.paths.contains(directory) {
                config.skills.paths.push(directory.clone());
            }
        }
        let bridge = self.agent.borrow().tool_bridge().clone();
        let mut skills = bridge
            .read_resource::<SkillManager>()
            .await
            .unwrap_or_default();
        let skill_revision = skills.baseline_revision();
        skills.set_baseline_frozen(false);
        let project_trusted = crate::agent::folder_trust::resolve_and_record(
            cwd,
            config.remote_settings.as_ref(),
            false,
        );
        let git_root = xai_grok_workspace::session::git::find_git_root_from_path(cwd).ok();
        let (native_hooks, hook_errors) = crate::util::hooks::discover_hooks(
            git_root.as_deref(), &self.rebuild_spec.compat, project_trusted,
        );
        let (hook_registry, _) = self.prepare_plugin_hook_registry(
            plugin_registry.as_deref(), Some(Arc::new(native_hooks)),
        );
        let hook_load_errors = hook_errors.iter().map(ToString::to_string).collect();
        let hook_disabled = Arc::new(xai_grok_hooks::trust::DisabledHooks::load());
        let mut baseline = xai_grok_agent::prompt::skills::list_skills_with_plugins(
            Some(&self.session_info.cwd),
            &config.skills,
            plugin_registry.as_deref(),
            self.rebuild_spec.compat,
            project_trusted,
        )
        .await;
        for skill in &mut baseline {
            *skill = xai_grok_tools::implementations::skills::skill::load_skill_with_body(skill)
                .await
                .map_err(candidate_error)?;
        }
        skills.update_startup_baseline(baseline);
        let (runtime_skills, skill_effects) = match skills.take_pending() {
            Some((skills, effects)) => (AvailableSkills(skills), effects),
            None => (
                bridge
                    .read_resource::<AvailableSkills>()
                    .await
                    .unwrap_or_else(|| AvailableSkills(vec![])),
                Default::default(),
            ),
        };
        skills.set_baseline_frozen(true);

        let configs = crate::session::managed_mcp::merge_managed_mcp_servers(
            candidate.external_mcp_servers.clone(),
            cwd,
            plugin_registry.as_deref(),
            &self.rebuild_spec.compat,
        );
        let (mcp_generation, meta) = {
            let live = self.mcp_state.lock().await;
            (
                live.current_generation(),
                live.meta_config_map.clone(),
            )
        };
        let mut mcp = McpState::new_with_meta(configs.clone(), meta.clone());
        // A mounted baseline stays frozen; the next candidate reads fresh
        // native policy instead of retaining a stale prior-mount copy.
        mcp.disabled_tools = crate::util::config::get_all_mcp_disabled_tools(cwd);
        let oauth = self.spawn_oauth_config_map(cwd);
        let events = self.events.writer();
        let spawn_context = crate::session::mcp_servers::McpSpawnCtx::for_session(
            self.session_info.id.0.as_ref(),
            &events,
            crate::session::mcp_servers::OauthInteractivity::from_non_interactive(
                self.attach_non_interactive.get(),
            ),
            self.tool_context.process_scope.as_ref(),
        );
        let clients = crate::session::mcp_servers::start_mcp_servers(
            configs,
            Some(cwd),
            &meta,
            &oauth,
            &spawn_context,
        )
        .await;
        let mut tools = vec![];
        for client in clients {
            let client = Arc::new(client.map_err(|error| candidate_error(error.to_string()))?);
            let name = client.server_name().to_string();
            let registrations = client
                .get_tool_registrations(self.mcp_state.clone())
                .await
                .map_err(|error| candidate_error(error.to_string()))?;
            for registration in registrations {
                let unqualified = registration
                    .name
                    .strip_prefix(&format!("{name}__"))
                    .unwrap_or(&registration.name);
                mcp.record_tool_icons(registration.name.clone(), registration.icons.clone());
                if let Some(meta) = registration.meta.clone() {
                    mcp.mcp_tool_meta.insert(registration.name.clone(), meta);
                }
                if mcp.is_tool_disabled(&name, unqualified) {
                    mcp.disabled_tool_registrations
                        .insert(registration.name.clone(), registration);
                } else if registration.model_visible {
                    tools.push(FinalizedToolset::prepare_mcp_tool(
                        registration.name,
                        registration.tool,
                        registration.input_schema,
                    ));
                }
            }
            mcp.owned_clients.insert(name, client);
        }
        let gateway_catalog = {
            let state = self.managed_mcp_handle.lock().await;
            if state.gateway_tools_active {
                match &state.gateway_tool_cache {
                    crate::session::managed_mcp::GatewayToolCatalogCache::Ready(catalog) => {
                        Some(catalog.clone())
                    }
                    _ => return Err(candidate_error("managed MCP catalog is not ready")),
                }
            } else {
                None
            }
        };
        let disabled_gateway_tools = crate::util::config::get_all_mcp_disabled_tools(cwd);
        let catalog = super::mcp_snapshot::prepare_mcp_catalog(
            bridge.toolset().prepared_mcp_definitions(&tools),
            mcp.all_clients()
                .map(|(name, client)| (name.clone(), client.clone()))
                .collect(),
            gateway_catalog.as_ref(),
            &disabled_gateway_tools,
        )
        .await;
        Ok(PreparedCandidate {
            mounted: MountedConfig {
                candidate,
                config,
                sampling: sampling.clone(),
                mcp_servers: mcp.configs.clone(),
                hook_disabled,
            },
            model,
            sampling,
            auth_type,
            auto_compact_threshold_percent,
            config_version,
            skill_revision,
            plugin_revision,
            plugin_registry,
            hook_registry,
            hook_load_errors,
            client_hooks,
            skills,
            runtime_skills,
            skill_effects,
            mcp_generation,
            mcp,
            tools,
            catalog,
        })
    }

    /// Only called by native admission while running_task and pending_inputs are empty.
    /// The actor does not read another command until this decision completes.
    pub(super) async fn mount_candidate(
        &self,
        candidate: serde_json::Value,
        cancelled: tokio_util::sync::CancellationToken,
    ) -> Result<xai_grok_tools::management::admission::AdmissionPermit, acp::Error> {
        let candidate: ConfigCandidate = serde_json::from_value(candidate)
            .map_err(|error| candidate_error(error.to_string()))?;
        let prepared = tokio::select! {
            biased;
            () = cancelled.cancelled() => return Err(candidate_error("candidate cancelled")),
            () = self.tool_context.admission.wait_until_closed() => return Err(candidate_error("agent admission closed during preparation")),
            // Keep the detached discovery state off every caller's actor/test future.
            prepared = Box::pin(self.prepare_candidate(candidate)) => prepared?,
        };
        let toolset = self.agent.borrow().tool_bridge().toolset();
        // Lock producers before checking generations. No old MCP completion or
        // pending skill publication can pass this boundary during chat install.
        let mut live_mcp = self.mcp_state.clone().lock_owned().await;
        let mut resources = toolset.resources.clone().lock_owned().await;
        if prepared.mcp_generation.is_cancelled()
            || self.tool_context.config_clock.snapshot() != prepared.config_version
            || resources
                .get::<SkillManager>()
                .map_or(0, SkillManager::baseline_revision)
                != prepared.skill_revision
        {
            return Err(candidate_error("candidate superseded during preparation"));
        }
        let (current_model, current_sampling, current_auth_type) = self.models_manager.prepare_published_model(
            &prepared.mounted.candidate.model,
            prepared.mounted.candidate.reasoning_effort.as_deref(),
        )?;
        if serde_json::to_value(current_model).ok() != serde_json::to_value(&prepared.model).ok()
            || current_sampling.api_key != prepared.sampling.api_key
            || current_auth_type != prepared.auth_type
        {
            return Err(candidate_error(
                "candidate model changed during preparation",
            ));
        }
        if cancelled.is_cancelled() {
            return Err(candidate_error("candidate cancelled"));
        }
        let old_sampling = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .ok_or_else(|| candidate_error("chat state unavailable"))?;
        let old_credentials = self.chat_state_handle.get_credentials().await;
        let sampling = prepared.sampling.clone();
        let context_window = self.compaction.context_window_override
            .or_else(|| std::num::NonZeroU64::new(sampling.context_window))
            .ok_or_else(|| candidate_error("candidate has invalid context window"))?;
        let chat_sampling = xai_grok_sampling_types::SamplingConfig {
            base_url: sampling.base_url.clone(),
            mtls_cert_dir: sampling.mtls_cert_dir.clone(),
            model: sampling.model.clone(),
            max_completion_tokens: sampling.max_completion_tokens,
            temperature: sampling.temperature,
            top_p: sampling.top_p,
            max_retries: Some(xai_grok_sampler::resolve_max_retries(sampling.max_retries)),
            rate_limit_retry_threshold: sampling.rate_limit_retry_threshold,
            api_backend: sampling.api_backend.clone(),
            extra_headers: sampling.extra_headers.clone(),
            conversation_group_id: old_sampling.conversation_group_id,
            query_params: sampling.query_params.clone(),
            env_http_headers: sampling.env_http_headers.clone(),
            context_window,
            reasoning_effort: sampling.reasoning_effort,
            reasoning_summary: sampling.reasoning_summary,
            stream_tool_calls: Some(sampling.stream_tool_calls),
        };
        let credentials = xai_chat_state::Credentials {
            api_key: sampling.api_key.clone(),
            client_version: sampling.client_version.clone(),
            alpha_test_key: old_credentials.alpha_test_key,
            auth_type: prepared.auth_type,
        };
        let model_id = acp::ModelId::new(prepared.mounted.candidate.model.clone());
        let auto_compact_threshold_percent = prepared.auto_compact_threshold_percent;
        let instructions = prepared.mounted.candidate.instructions.clone();
        let skill_effects = prepared.skill_effects.clone();
        let plugin_registry = prepared.plugin_registry.clone();
        let hook_registry = prepared.hook_registry.clone();
        let hook_disabled = prepared.mounted.hook_disabled.clone();
        let hook_load_errors = prepared.hook_load_errors.clone();
        let client_hooks = prepared.client_hooks.clone();
        let committed = Arc::new(parking_lot::Mutex::new(None));
        let commit_result = committed.clone();
        let candidate_admission = self.candidate_admission.clone();
        let admission = self.tool_context.admission.clone();
        let config_clock = self.tool_context.config_clock.clone();
        let commit_cancelled = cancelled.clone();
        let tool_metadata_snapshot = self.tool_metadata_snapshot.clone();
        let mcp_reminder_dirty = self.mcp_reminder_dirty.clone();
        let installed = self
            .chat_state_handle
            .install_prepared_config_with_commit(
                instructions.clone(),
                chat_sampling,
                credentials,
                cancelled,
                move || {
                    candidate_admission.publish(
                        &commit_cancelled,
                        prepared.plugin_revision,
                        prepared.mounted,
                        || {
                            if config_clock.snapshot() != prepared.config_version {
                                return false;
                            }
                            // Acquire the synchronous registry lock only inside this
                            // no-await commit, never while waiting on the chat actor.
                            let Ok(install) = toolset.prepare_mcp_install(prepared.tools) else {
                                return false;
                            };
                            let Ok(permit) = admission.try_admit(
                                xai_grok_tools::management::admission::AdmissionSource::Human,
                            ) else {
                                return false;
                            };
                            let _claim = live_mcp.restart_init();
                            live_mcp.update_configs(prepared.mcp.configs);
                            live_mcp.owned_clients = prepared.mcp.owned_clients;
                            live_mcp.mcp_tool_meta = prepared.mcp.mcp_tool_meta;
                            live_mcp.mcp_tool_icons = prepared.mcp.mcp_tool_icons;
                            live_mcp.disabled_tools = prepared.mcp.disabled_tools;
                            live_mcp.disabled_tool_registrations =
                                prepared.mcp.disabled_tool_registrations;
                            let event_tx = live_mcp.client_event_tx();
                            live_mcp.set_client_event_tx(event_tx);
                            live_mcp.complete_init();
                            live_mcp.freeze_mounted_snapshot();
                            install.commit();
                            resources.insert(prepared.skills);
                            resources.insert(prepared.runtime_skills);
                            resources.insert(
                                xai_grok_tools::types::resources::ManagedGatewayToolCatalog(
                                    prepared
                                        .catalog
                                        .gateway_resource_entries
                                        .into_iter()
                                        .collect(),
                                ),
                            );
                            *tool_metadata_snapshot.lock().unwrap() =
                                crate::session::tool_index::ToolMetadataSnapshot {
                                    tools: prepared.catalog.tools,
                                    servers: prepared.catalog.servers,
                                    mcp_initialized: true,
                                };
                            mcp_reminder_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                            *commit_result.lock() = Some(permit);
                            true
                        },
                    )
                },
            )
            .await;
        if installed != Some(true) {
            return Err(candidate_error(
                "candidate cancelled, superseded, or publication refused",
            ));
        }
        let permit = committed.lock().take().expect("acknowledged native commit");
        *self.plugin_registry.borrow_mut() = plugin_registry;
        *self.hook_registry.borrow_mut() = hook_registry;
        *self.hook_disabled.borrow_mut() = hook_disabled;
        *self.hook_load_errors.borrow_mut() = hook_load_errors;
        *self.client_hooks.borrow_mut() = client_hooks;
        *self.explicit_system_prompt.borrow_mut() = Some(instructions);
        self.supports_backend_search
            .set(sampling.supports_backend_search);
        self.compactions_remaining
            .set(sampling.compactions_remaining);
        self.compaction_at_tokens.set(sampling.compaction_at_tokens);
        self.compaction.threshold_percent.set(auto_compact_threshold_percent);
        self.invalidate_model_auth_memo();
        self.signals_handle().record_model_usage(&sampling.model);
        let _ = self.notifications.persistence_tx.send(PersistenceMsg::CurrentModel {
            model_id,
            agent_name: Some(self.agent.borrow().definition().name.clone()),
            reasoning_effort: Some(sampling.reasoning_effort),
        });
        self.apply_skill_update_effects(skill_effects).await;
        let version = self.tool_context.config_clock.bump();
        self.broadcast_effective_config_changed(version);
        // Publication, actor-local hook/skill adoption and config-clock advance
        // have all completed. Close serializes with this activation and is terminal.
        self.candidate_admission.activate_scheduler();
        Ok(permit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugins_with_hook(
        name: &str,
        root: &std::path::Path,
    ) -> Arc<xai_grok_agent::plugins::PluginRegistry> {
        use xai_grok_agent::plugins::discovery::{
            DiscoveredPlugin, PluginId, PluginOrigin, PluginScope,
        };
        let discovered = DiscoveredPlugin {
            manifest: serde_json::from_value(serde_json::json!({
                "name": name, "hooks": {"hooks": {"SessionStart": [{"hooks": [{"type":"command", "command":"true"}]}]}}
            })).unwrap(),
            id: PluginId(name.into()), root: root.into(), canonical_root: root.into(),
            scope: PluginScope::CliOverride, origin: PluginOrigin::CliOverride, trusted: true,
            skill_dirs: vec![], command_dirs: vec![], agent_dirs: vec![],
            hooks_path: None, mcp_config_path: None, lsp_config_path: None, conflict: None,
        };
        Arc::new(xai_grok_agent::plugins::PluginRegistry::from_discovered(
            vec![discovered],
            &[],
            &[name.into()],
        ))
    }

    fn candidate_prompt(
        id: &str,
        candidate: serde_json::Value,
        generation: u64,
    ) -> (
        SessionCommand,
        tokio::sync::oneshot::Receiver<crate::session::PromptTurnResult>,
    ) {
        let (respond_to, response) = tokio::sync::oneshot::channel();
        let prompt = SessionCommand::Prompt {
            prompt_id: id.into(),
            prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(id))],
            prompt_mode: Default::default(),
            artifact_upload_ctx: None,
            client_identifier: None,
            screen_mode: None,
            verbatim: false,
            traceparent: None,
            json_schema: None,
            send_now: false,
            admission: None,
            agent_admission: None,
            tool_overrides_update: None,
            respond_to,
            prompt_admitted: None,
            persist_ack: None,
            parsed_prompt_tx: None,
        };
        (
            SessionCommand::PromptCandidate {
                candidate,
                generation,
                prompt: Box::new(prompt),
            },
            response,
        )
    }

    // Allocate the real loop separately so its large construction temporary is
    // not retained in the fixture's async state machine.
    fn spawn_fifo_loop(
        actor: Arc<SessionActor>,
        commands: tokio::sync::mpsc::UnboundedReceiver<SessionCommand>,
        chat: tokio::sync::mpsc::UnboundedReceiver<xai_chat_state::ChatStateEvent>,
        events: tokio::sync::mpsc::UnboundedReceiver<crate::session::replay_events::SessionEvent>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::task::spawn_local(super::super::run_session(
            actor,
            commands,
            chat,
            events,
            None,
            Arc::new(parking_lot::Mutex::new(
                xai_grok_workspace::file_system::CodebaseIndexManager::new(),
            )),
            std::path::PathBuf::from("/tmp"),
            crate::session::fs_watch::FsWatchCapabilities::none(),
        ))
    }

    #[tokio::test]
    async fn native_fifo_mount_keeps_queued_route_and_skills_then_switches_idle_protocol() {
        tokio::task::LocalSet::new().run_until(Box::pin(async {
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                let server = xai_grok_test_support::MockInferenceServer::start().await.unwrap();
                server.set_response("done");
                server.hold_agent_completions();
                let (gateway_tx, mut gateway_rx) = tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
                tokio::task::spawn_local(async move {
                    while let Some(message) = gateway_rx.recv().await {
                        if let xai_acp_lib::AcpClientMessage::SessionNotification(args) = message {
                            let _ = args.response_tx.send(Ok(()));
                        }
                    }
                });
                let (persistence_tx, mut persistence_rx) = tokio::sync::mpsc::unbounded_channel::<crate::session::persistence::PersistenceMsg>();
                let (model_tx, mut model_rx) = tokio::sync::mpsc::unbounded_channel();
                tokio::task::spawn_local(async move {
                    while let Some(message) = persistence_rx.recv().await {
                        if let crate::session::persistence::PersistenceMsg::FlushAndAck { respond_to }
                        | crate::session::persistence::PersistenceMsg::FlushForExit { respond_to } = message {
                            let _ = respond_to.send(Ok(()));
                        } else if let PersistenceMsg::CurrentModel { model_id, .. } = message {
                            let _ = model_tx.send(model_id);
                        }
                    }
                });
                let (mut actor, events) = crate::session::acp_session::support::create_test_actor_ex(
                    0, 256_000, 85, gateway_tx, persistence_tx,
                ).await;
                let (sampler_tx, mut sampler_rx) = tokio::sync::mpsc::unbounded_channel();
                actor.sampler_handle = xai_grok_sampler::SamplerActor::spawn(Default::default(), Default::default(), sampler_tx);
                actor.compaction.context_window_override = std::num::NonZeroU64::new(123_456);
                for (key, wire, backend) in [
                    ("route-a", "wire-A", xai_grok_sampling_types::ApiBackend::Responses),
                    ("route-c", "wire-C", xai_grok_sampling_types::ApiBackend::ChatCompletions),
                ] {
                    let mut entry = crate::agent::config::ModelEntry::fallback(wire, &Default::default());
                    entry.info.agent_type = actor.agent.borrow().definition().name.clone();
                    entry.info.base_url = server.url();
                    entry.info.api_backend = backend;
                    entry.info.max_retries = Some(0);
                    entry.info.auto_compact_threshold_percent = Some(if key == "route-a" { 61 } else { 73 });
                    entry.api_key = Some("test-fifo-key".into());
                    actor.models_manager.insert_test_entry(key, entry);
                }
                let actor = Arc::new(actor);
                assert_eq!(actor.effective_config_snapshot().await.unwrap().mounted_revision, None);
                let sampler_actor = actor.clone();
                tokio::task::spawn_local(async move {
                    while let Some(event) = sampler_rx.recv().await { sampler_actor.handle_sampling_event(event).await; }
                });
                let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
                let (_chat_tx, chat_rx) = tokio::sync::mpsc::unbounded_channel();
                let run = spawn_fifo_loop(actor.clone(), cmd_rx, chat_rx, events);
                let skills = tempfile::tempdir().unwrap();
                let skill_path = skills.path().join("SKILL.md");
                std::fs::write(&skill_path, "---\nname: frozen\ndescription: frozen skill\n---\nSKILL_A").unwrap();
                let candidate = serde_json::json!({
                    "instructions":"INSTRUCTIONS_A", "skillDirectories":[skills.path()],
                    "externalMcpServers":[], "model":"route-a", "subjectOptions":{}, "subagentBriefs":[], "revision":"A"
                });
                let (a, response_a) = candidate_prompt("prompt-A", candidate.clone(), actor.candidate_admission.generation());
                cmd_tx.send(a).unwrap();
                while server.request_count() == 0 { tokio::task::yield_now().await; }
                // A busy candidate is ignored, not decoded or partially applied.
                let (b, response_b) = candidate_prompt("prompt-B", serde_json::json!({"invalid":true}), actor.candidate_admission.generation());
                cmd_tx.send(b).unwrap();
                cmd_tx.send(SessionCommand::ReplaceSystemPrompt { system_prompt: "MUST_NOT_INSTALL".into() }).unwrap();
                let (responds_to, barrier) = tokio::sync::oneshot::channel();
                cmd_tx.send(SessionCommand::GetCurrentModel { responds_to }).unwrap();
                barrier.await.unwrap();
                assert_eq!(actor.candidate_admission.mounted().unwrap().candidate.revision, "A");
                assert_eq!(actor.effective_config_snapshot().await.unwrap().mounted_revision.as_deref(), Some("A"));
                assert_eq!(actor.compaction.threshold_percent.get(), 61);
                assert_eq!(actor.chat_state_handle.get_credentials().await.auth_type, xai_chat_state::AuthType::ApiKey);
                assert_eq!(actor.tool_context.admission.snapshot().accepted, 2);
                std::fs::write(&skill_path, "---\nname: frozen\ndescription: changed skill\n---\nSKILL_C").unwrap();
                actor.reload_skills_from_disk().await;
                let old = actor.snapshot_subagent_parent().await;
                assert_eq!(old.skills.unwrap().iter().find(|skill| skill.name == "frozen").unwrap().body.as_deref(), Some("SKILL_A"));
                server.release_agent_completions();
                response_a.await.unwrap().unwrap();
                response_b.await.unwrap().unwrap();
                loop {
                    let state = actor.state.lock().await;
                    if state.running_task.is_none() && state.pending_inputs.is_empty() { break; }
                    drop(state);
                    tokio::task::yield_now().await;
                }
                let mut next = candidate;
                next["model"] = serde_json::json!("route-c");
                next["instructions"] = serde_json::json!("INSTRUCTIONS_C");
                next["revision"] = serde_json::json!("C");
                let (c, response_c) = candidate_prompt("prompt-C", next, actor.candidate_admission.generation());
                cmd_tx.send(c).unwrap();
                response_c.await.unwrap().unwrap();
                assert_eq!(actor.effective_config_snapshot().await.unwrap().mounted_revision.as_deref(), Some("C"));
                assert_eq!(actor.compaction.threshold_percent.get(), 73);
                assert_eq!(actor.effective_config_snapshot().await.unwrap().route.context_window, 123_456);
                assert_eq!(model_rx.recv().await.unwrap().0.as_ref(), "route-a");
                assert_eq!(model_rx.recv().await.unwrap().0.as_ref(), "route-c");
                assert!(model_rx.try_recv().is_err());
                let requests = server.requests();
                let inference: Vec<_> = requests.iter().filter(|request| request.path == "/v1/responses" || request.path == "/v1/chat/completions").collect();
                assert_eq!(inference.len(), 3);
                assert_eq!(inference.iter().map(|request| request.body.as_ref().unwrap()["model"].as_str().unwrap()).collect::<Vec<_>>(), vec!["wire-A", "wire-A", "wire-C"]);
                assert_eq!(inference.iter().map(|request| request.path.as_str()).collect::<Vec<_>>(), vec!["/v1/responses", "/v1/responses", "/v1/chat/completions"]);
                assert!(inference[1].body.as_ref().unwrap().to_string().contains("INSTRUCTIONS_A"));
                assert!(!inference[1].body.as_ref().unwrap().to_string().contains("MUST_NOT_INSTALL"));
                assert!(inference[2].body.as_ref().unwrap().to_string().contains("INSTRUCTIONS_C"));
                let current = actor.snapshot_subagent_parent().await;
                assert_eq!(current.skills.unwrap().iter().find(|skill| skill.name == "frozen").unwrap().body.as_deref(), Some("SKILL_C"));
                let (respond_to, shutdown) = tokio::sync::oneshot::channel();
                cmd_tx.send(SessionCommand::ShutdownChecked { respond_to }).unwrap();
                shutdown.await.unwrap().unwrap();
                run.await.unwrap();
            }).await.expect("native FIFO mount test timed out");
        })).await;
    }

    #[tokio::test]
    async fn native_candidate_mount_failure_and_cancel_preserve_complete_old_snapshot() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut actor =
                    crate::session::acp_session::support::actor_with_persistence_drain().await;
                let activation = xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerActivationGate::blocked();
                Arc::get_mut(&mut actor).unwrap().candidate_admission =
                    crate::session::config_candidate::CandidateAdmission::new(true, Some(activation.clone()));
                let mut model = crate::agent::config::ModelEntry::fallback(
                    "candidate-wire",
                    &crate::agent::config::EndpointsConfig::default(),
                );
                model.info.agent_type = actor.agent.borrow().definition().name.clone();
                model.api_key = Some("test-only-candidate-key".into());
                actor.models_manager.insert_test_entry("candidate", model);
                let skill_dir = tempfile::tempdir().unwrap();
                let plugins_a = plugins_with_hook("mount-a", skill_dir.path());
                let plugins_b = plugins_with_hook("mount-b", skill_dir.path());
                *actor.plugin_registry.borrow_mut() = Some(plugins_a.clone());
                std::fs::write(skill_dir.path().join("SKILL.md"), "---\nname: mounted-skill-a\ndescription: First mounted skill\n---\nA instructions\n").unwrap();
                let candidate = serde_json::json!({
                    "instructions":"mounted A", "skillDirectories":[skill_dir.path()], "externalMcpServers":[],
                    "model":"candidate", "subjectOptions":{}, "subagentBriefs":[], "revision":"A",
                });
                let toolset = actor.agent.borrow().tool_bridge().toolset();
                let cancelled = tokio_util::sync::CancellationToken::new();
                cancelled.cancel();
                assert!(actor.mount_candidate(candidate.clone(), cancelled).await.is_err());
                assert!(!activation.is_active());
                let (command, response) = candidate_prompt("unmounted", candidate.clone(), 0);
                let SessionCommand::PromptCandidate { prompt, .. } = command else { unreachable!() };
                assert!(actor.admit_candidate_command(*prompt).await.is_none());
                assert!(response.await.unwrap().is_err());
                let (respond_to, response) = tokio::sync::oneshot::channel();
                assert!(actor.admit_candidate_command(SessionCommand::SnapshotSubagentParent { respond_to }).await.is_none());
                assert!(response.await.is_err());
                assert_eq!(actor.tool_context.admission.snapshot().accepted, 0);
                let permit = actor
                    .mount_candidate(candidate.clone(), Default::default())
                    .await
                    .unwrap();
                drop(permit);
                assert!(activation.is_active());
                let before = actor.chat_state_handle.snapshot().await.unwrap();
                assert_eq!(before.sampling_config.model, "candidate-wire");
                assert!(actor.tool_metadata_snapshot.lock().unwrap().mcp_initialized);
                assert_eq!(
                    actor
                        .candidate_admission
                        .mounted()
                        .unwrap()
                        .candidate
                        .revision,
                    "A"
                );
                assert!(Arc::ptr_eq(
                    &toolset,
                    &actor.agent.borrow().tool_bridge().toolset()
                ));
                let definitions = serde_json::to_value(toolset.tool_definitions()).unwrap();
                let generation = actor.mcp_state.lock().await.current_generation();
                let (respond_to, response) = tokio::sync::oneshot::channel();
                assert!(actor.admit_candidate_command(SessionCommand::ToggleMcpTool {
                    server_name: "native".into(), tool_name: "native".into(),
                    enabled: false, is_managed_gateway: false, respond_to,
                }).await.is_none());
                assert!(response.await.unwrap().is_err());
                let (responds_to, response) = tokio::sync::oneshot::channel();
                assert!(actor.admit_candidate_command(SessionCommand::SetReasoningEffort {
                    effort: xai_grok_sampling_types::ReasoningEffort::High, responds_to,
                }).await.is_none());
                assert!(response.await.unwrap().is_err());
                // Even producers capturing the new generation cannot change a mounted baseline.
                use crate::session::mcp_servers::SharedMcpState;
                assert!(actor.mcp_state.write_if_current(&generation, |_| panic!("mounted writer ran")).await.is_err());
                assert!(actor.mcp_state.write_if_slot_is("new-server", None, &generation, |_| panic!("mounted slot writer ran")).await.is_err());
                actor.mcp_reminder_dirty.store(false, std::sync::atomic::Ordering::Relaxed);
                actor.refresh_mcp_snapshot_and_schedule_reminder().await;
                assert!(!actor.mcp_reminder_dirty.load(std::sync::atomic::Ordering::Relaxed));
                let inherited = actor.snapshot_subagent_parent().await;
                assert!(inherited.skills.as_ref().unwrap().iter().any(|skill| skill.name == "mounted-skill-a"));
                assert!(Arc::ptr_eq(&inherited.toolset, &toolset));
                assert!(inherited.hook_registry.as_ref().unwrap().all_hooks().iter().any(|hook| hook.name.starts_with("plugin/mount-a/")));
                // Cancellation between preparation and the seal lock must not
                // publish; close and required cold actors must also refuse it.
                let captured = inherited.mounted.as_ref().unwrap();
                let child_admission = crate::session::config_candidate::CandidateAdmission::default();
                let ticket = child_admission.generation();
                child_admission.cancel();
                assert!(!child_admission.inherit(ticket, (**captured).clone(), || panic!("stale seal published")));
                assert!(child_admission.inherit(child_admission.generation(), (**captured).clone(), || {}));
                assert!(!child_admission.inherit(child_admission.generation(), (**captured).clone(), || panic!("duplicate seal published")));
                child_admission.close();
                assert!(!child_admission.inherit(child_admission.generation(), (**captured).clone(), || panic!("closed seal published")));
                let required = crate::session::config_candidate::CandidateAdmission::new(true, None);
                assert!(!required.inherit(required.generation(), (**captured).clone(), || panic!("required actor inherited")));
                let hook_event = xai_grok_hooks::event::HookEventName::PreToolUse;
                assert!(!actor.client_hooks.borrow().contains_key(&hook_event));
                let hooks = std::collections::HashMap::from([(hook_event, vec![])]);
                assert!(actor.admit_candidate_command(SessionCommand::SetClientHooks { hooks }).await.is_none());
                assert!(!actor.client_hooks.borrow().contains_key(&hook_event));
                assert_eq!(actor.apply_plugin_registry_snapshot(Some(plugins_b.clone())).await, (0, false, 0));
                for failure in ["cancel", "unknown-option", "invalid-directory"] {
                    let mut next = candidate.clone();
                    next["instructions"] = serde_json::json!("must never publish");
                    next["revision"] = serde_json::json!("B");
                    let cancelled = tokio_util::sync::CancellationToken::new();
                    match failure {
                        "cancel" => cancelled.cancel(),
                        "unknown-option" => {
                            next["subjectOptions"] = serde_json::json!({"unknown":true})
                        }
                        "invalid-directory" => {
                            next["skillDirectories"] = serde_json::json!(["relative-path"])
                        }
                        _ => unreachable!(),
                    }
                    assert!(actor.mount_candidate(next, cancelled).await.is_err());
                    assert!(Arc::ptr_eq(actor.plugin_registry.borrow().as_ref().unwrap(), &plugins_a));
                    assert!(!actor.client_hooks.borrow().contains_key(&hook_event));
                    assert!(actor.hook_registry.borrow().as_ref().unwrap().all_hooks().iter().any(|hook| hook.name.starts_with("plugin/mount-a/")));
                    assert_eq!(
                        serde_json::to_value(actor.chat_state_handle.snapshot().await.unwrap())
                            .unwrap(),
                        serde_json::to_value(&before).unwrap()
                    );
                    assert_eq!(
                        actor
                            .candidate_admission
                            .mounted()
                            .unwrap()
                            .candidate
                            .revision,
                        "A"
                    );
                    assert_eq!(
                        serde_json::to_value(toolset.tool_definitions()).unwrap(),
                        definitions
                    );
                    assert!(!generation.is_cancelled());
                    assert_eq!(actor.tool_context.admission.snapshot().active, 0);
                    assert_eq!(actor.tool_context.admission.snapshot().accepted, 1);
                }
                std::fs::write(skill_dir.path().join("SKILL.md"), "---\nname: mounted-skill-b\ndescription: Replacement mounted skill\n---\nB instructions\n").unwrap();
                let captured_skill = inherited.skills.as_ref().unwrap().iter().find(|skill| skill.name == "mounted-skill-a").unwrap();
                assert_eq!(xai_grok_tools::implementations::skills::skill::load_skill_content(captured_skill).await.unwrap(), "A instructions\n");
                let mut next = candidate;
                next["instructions"] = serde_json::json!("mounted B");
                next["revision"] = serde_json::json!("B");
                drop(actor.mount_candidate(next, Default::default()).await.unwrap());
                let replacement = actor.snapshot_subagent_parent().await;
                assert!(replacement.client_hooks.contains_key(&hook_event));
                assert!(Arc::ptr_eq(replacement.plugin_registry.as_ref().unwrap(), &plugins_b));
                let replacement_hooks = replacement.hook_registry.as_ref().unwrap().all_hooks();
                assert!(replacement_hooks.iter().any(|hook| hook.name.starts_with("plugin/mount-b/")));
                assert!(!replacement_hooks.iter().any(|hook| hook.name.starts_with("plugin/mount-a/")));
                assert!(Arc::ptr_eq(inherited.plugin_registry.as_ref().unwrap(), &plugins_a));
                assert_eq!(replacement.mounted.unwrap().candidate.revision, "B");
                let replacement_skills = replacement.skills.unwrap();
                assert_eq!(replacement_skills.iter().find(|skill| skill.name == "mounted-skill-b").unwrap().body.as_deref(), Some("B instructions\n"));
                assert!(replacement_skills.iter().any(|skill| skill.name == "mounted-skill-b"));
                assert!(!replacement_skills.iter().any(|skill| skill.name == "mounted-skill-a"));
                assert_eq!(inherited.mounted.unwrap().candidate.revision, "A");
                let inherited_skills = inherited.skills.unwrap();
                assert!(inherited_skills.iter().any(|skill| skill.name == "mounted-skill-a"));
                assert!(!inherited_skills.iter().any(|skill| skill.name == "mounted-skill-b"));
                actor.candidate_admission.close();
                assert!(!activation.is_active());
                let closed_snapshot = actor.chat_state_handle.snapshot().await.unwrap();
                assert!(actor.mount_candidate(serde_json::json!({
                    "instructions":"closed", "skillDirectories":[], "externalMcpServers":[],
                    "model":"candidate", "subjectOptions":{}, "subagentBriefs":[], "revision":"closed"
                }), Default::default()).await.is_err());
                activation.activate();
                assert!(!activation.is_active());
                assert_eq!(serde_json::to_value(actor.chat_state_handle.snapshot().await.unwrap()).unwrap(), serde_json::to_value(closed_snapshot).unwrap());
            })
            .await;
    }
}
