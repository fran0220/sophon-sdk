use super::*;
use crate::session::config_candidate::{ConfigCandidate, MountedConfig};
use xai_grok_tools::registry::types::{FinalizedToolset, PreparedMcpTool};
use xai_grok_tools::types::resources::AvailableSkills;
use xai_grok_tools::types::skill_discovery_tracker::SkillManager;

struct PreparedCandidate {
    mounted: MountedConfig,
    model: crate::agent::config::ModelEntry,
    sampling: crate::sampling::SamplerConfig,
    config_version: xai_prompt_queue::QueueVersion,
    skill_revision: u64,
    plugin_revision: u64,
    plugin_registry: Option<Arc<xai_grok_agent::plugins::PluginRegistry>>,
    hook_registry: Option<Arc<xai_grok_hooks::discovery::HookRegistry>>,
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
        if self.candidate_admission.mounted().is_some() {
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
                SessionCommand::OverrideModelName { .. } => {
                    tracing::warn!("ignored standalone model override for mounted session");
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
        let (hook_registry, _) = self.prepare_plugin_hook_registry(plugin_registry.as_deref());
        let (model, sampling) = self
            .models_manager
            .prepare_published_model(&candidate.model, candidate.reasoning_effort.as_deref())?;
        if model.info().agent_type != self.agent.borrow().definition().name {
            return Err(candidate_error(
                "candidate requires a different native agent harness",
            ));
        }
        let mut config = self.models_manager.native_config_snapshot();
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
        let baseline = xai_grok_agent::prompt::skills::list_skills_with_plugins(
            Some(&self.session_info.cwd),
            &config.skills,
            plugin_registry.as_deref(),
            self.rebuild_spec.compat,
            project_trusted,
        )
        .await;
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
        let (mcp_generation, meta, disabled_tools) = {
            let live = self.mcp_state.lock().await;
            (
                live.current_generation(),
                live.meta_config_map.clone(),
                live.disabled_tools.clone(),
            )
        };
        let mut mcp = McpState::new_with_meta(configs.clone(), meta.clone());
        mcp.disabled_tools = disabled_tools;
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
            },
            model,
            sampling,
            config_version,
            skill_revision,
            plugin_revision,
            plugin_registry,
            hook_registry,
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
        let (current_model, current_sampling) = self.models_manager.prepare_published_model(
            &prepared.mounted.candidate.model,
            prepared.mounted.candidate.reasoning_effort.as_deref(),
        )?;
        if serde_json::to_value(current_model).ok() != serde_json::to_value(&prepared.model).ok()
            || current_sampling.api_key != prepared.sampling.api_key
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
        let context_window = std::num::NonZeroU64::new(sampling.context_window)
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
            auth_type: crate::agent::config::resolve_chat_state_auth_type(
                sampling.model.as_str(),
                self.auth_manager
                    .as_ref()
                    .and_then(|manager| manager.current_or_expired())
                    .as_ref()
                    .map(|auth| auth.key.as_str()),
                old_credentials.auth_type,
            ),
        };
        let instructions = prepared.mounted.candidate.instructions.clone();
        let skill_effects = prepared.skill_effects.clone();
        let plugin_registry = prepared.plugin_registry.clone();
        let hook_registry = prepared.hook_registry.clone();
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
        *self.explicit_system_prompt.borrow_mut() = Some(instructions);
        self.supports_backend_search
            .set(sampling.supports_backend_search);
        self.compactions_remaining
            .set(sampling.compactions_remaining);
        self.compaction_at_tokens.set(sampling.compaction_at_tokens);
        self.invalidate_model_auth_memo();
        self.apply_skill_update_effects(skill_effects).await;
        let version = self.tool_context.config_clock.bump();
        self.broadcast_effective_config_changed(version);
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

    #[tokio::test]
    async fn native_candidate_mount_failure_and_cancel_preserve_complete_old_snapshot() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let actor =
                    crate::session::acp_session::support::actor_with_persistence_drain().await;
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
                let permit = actor
                    .mount_candidate(candidate.clone(), Default::default())
                    .await
                    .unwrap();
                drop(permit);
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
                let mut next = candidate;
                next["instructions"] = serde_json::json!("mounted B");
                next["revision"] = serde_json::json!("B");
                drop(actor.mount_candidate(next, Default::default()).await.unwrap());
                let replacement = actor.snapshot_subagent_parent().await;
                assert!(Arc::ptr_eq(replacement.plugin_registry.as_ref().unwrap(), &plugins_b));
                let replacement_hooks = replacement.hook_registry.as_ref().unwrap().all_hooks();
                assert!(replacement_hooks.iter().any(|hook| hook.name.starts_with("plugin/mount-b/")));
                assert!(!replacement_hooks.iter().any(|hook| hook.name.starts_with("plugin/mount-a/")));
                assert!(Arc::ptr_eq(inherited.plugin_registry.as_ref().unwrap(), &plugins_a));
                assert_eq!(replacement.mounted.unwrap().candidate.revision, "B");
                let replacement_skills = replacement.skills.unwrap();
                assert!(replacement_skills.iter().any(|skill| skill.name == "mounted-skill-b"));
                assert!(!replacement_skills.iter().any(|skill| skill.name == "mounted-skill-a"));
                assert_eq!(inherited.mounted.unwrap().candidate.revision, "A");
                let inherited_skills = inherited.skills.unwrap();
                assert!(inherited_skills.iter().any(|skill| skill.name == "mounted-skill-a"));
                assert!(!inherited_skills.iter().any(|skill| skill.name == "mounted-skill-b"));
            })
            .await;
    }
}
