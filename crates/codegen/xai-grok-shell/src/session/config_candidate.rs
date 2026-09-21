//! Native FIFO configuration admission. Candidate data is not a sequence of setters.

use serde::{Deserialize, Serialize};

pub const CONFIG_CANDIDATE_META_KEY: &str = "x.sophon/configCandidate";

pub(crate) fn required_from_meta(
    meta: Option<&agent_client_protocol::Meta>,
) -> Result<bool, agent_client_protocol::Error> {
    match meta.and_then(|meta| meta.get("x.ai/requireConfigCandidate")) {
        None => Ok(false),
        Some(serde_json::Value::Bool(required)) => Ok(*required),
        Some(_) => Err(agent_client_protocol::Error::invalid_params()
            .data("x.ai/requireConfigCandidate must be a boolean")),
    }
}

pub(crate) fn wrap_prompt(
    prompt: super::SessionCommand,
    candidate: Option<(u64, serde_json::Value)>,
) -> super::SessionCommand {
    match candidate {
        Some((generation, candidate)) => super::SessionCommand::PromptCandidate {
            candidate,
            generation,
            prompt: Box::new(prompt),
        },
        None => prompt,
    }
}

/// A complete external-owner snapshot. Local/global configuration remains native-owned.
/// Decode only after the actor observes an empty FIFO; busy candidates are ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigCandidate {
    pub instructions: String,
    pub skill_directories: Vec<String>,
    pub external_mcp_servers: Vec<agent_client_protocol::McpServer>,
    pub model: String,
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub subject_options: serde_json::Map<String, serde_json::Value>,
    pub subagent_briefs: Vec<SubagentBrief>,
    pub revision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubagentBrief {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub model: Option<String>,
}

/// Out-of-band cancellation only; the actor mailbox remains the sole input FIFO.
/// Capturing a ticket at native ingress also fences candidates waiting behind a
/// preparation when cancellation arrives before the actor reaches them.
#[derive(Clone, Default)]
pub(crate) struct CandidateAdmission {
    state: std::sync::Arc<parking_lot::Mutex<CandidateAdmissionState>>,
}

#[derive(Default)]
struct CandidateAdmissionState {
    generation: u64,
    required: bool,
    closed: bool,
    scheduler_activation: Option<
        xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerActivationGate,
    >,
    preparing: Option<tokio_util::sync::CancellationToken>,
    mounted: Option<std::sync::Arc<MountedConfig>>,
    plugin_revision: u64,
    pending_plugins: Option<Option<std::sync::Arc<xai_grok_agent::plugins::PluginRegistry>>>,
    pending_client_hooks: Option<crate::extensions::hooks::ClientHooks>,
}

#[derive(Clone)]
pub struct MountedConfig {
    pub(crate) candidate: ConfigCandidate,
    pub(crate) config: crate::agent::config::Config,
    pub(crate) sampling: crate::sampling::SamplerConfig,
    pub(crate) mcp_servers: Vec<agent_client_protocol::McpServer>,
    pub(crate) hook_disabled: std::sync::Arc<xai_grok_hooks::trust::DisabledHooks>,
}

impl CandidateAdmission {
    pub(crate) fn new(
        required: bool,
        scheduler_activation: Option<
            xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerActivationGate,
        >,
    ) -> Self {
        Self {
            state: std::sync::Arc::new(parking_lot::Mutex::new(CandidateAdmissionState {
                required,
                scheduler_activation,
                ..Default::default()
            })),
        }
    }

    pub(crate) fn required(&self) -> bool {
        self.state.lock().required
    }

    pub(crate) fn inherit(&self, generation: u64, mounted: MountedConfig, commit: impl FnOnce()) -> bool {
        let mut state = self.state.lock();
        if state.closed || state.generation != generation || state.required || state.mounted.is_some() {
            return false;
        }
        commit();
        state.mounted = Some(std::sync::Arc::new(mounted));
        true
    }

    pub(crate) fn activate_scheduler(&self) {
        let state = self.state.lock();
        if !state.closed && state.mounted.is_some()
            && let Some(gate) = &state.scheduler_activation
        {
            gate.activate();
        }
    }

    /// Permanent process-local fence. A retry may drain/flush again but cannot
    /// admit a candidate or reopen this scheduler. Reopen builds a fresh latch.
    pub(crate) fn close(&self) {
        let mut state = self.state.lock();
        state.closed = true;
        state.generation += 1;
        if let Some(token) = state.preparing.take() {
            token.cancel();
        }
        if let Some(gate) = &state.scheduler_activation {
            gate.close();
        }
    }

    pub(crate) fn mounted(&self) -> Option<std::sync::Arc<MountedConfig>> {
        self.state.lock().mounted.clone()
    }

    pub(crate) fn defer_client_hooks_if_mounted(&self, hooks: crate::extensions::hooks::ClientHooks) -> bool {
        let mut state = self.state.lock();
        if state.mounted.is_none() {
            return false;
        }
        state.pending_client_hooks = Some(hooks);
        state.plugin_revision += 1;
        if let Some(token) = &state.preparing { token.cancel(); }
        true
    }

    pub(crate) fn client_hooks_for_preparation(&self, current: crate::extensions::hooks::ClientHooks) -> crate::extensions::hooks::ClientHooks {
        self.state.lock().pending_client_hooks.clone().unwrap_or(current)
    }

    pub(crate) fn defer_plugins_if_mounted(
        &self,
        plugins: Option<std::sync::Arc<xai_grok_agent::plugins::PluginRegistry>>,
    ) -> bool {
        let mut state = self.state.lock();
        if state.mounted.is_none() {
            return false;
        }
        state.pending_plugins = Some(plugins);
        state.plugin_revision += 1;
        if let Some(token) = &state.preparing {
            token.cancel();
        }
        true
    }

    pub(crate) fn plugins_for_preparation(
        &self,
        current: Option<std::sync::Arc<xai_grok_agent::plugins::PluginRegistry>>,
    ) -> (
        u64,
        Option<std::sync::Arc<xai_grok_agent::plugins::PluginRegistry>>,
    ) {
        let state = self.state.lock();
        (
            state.plugin_revision,
            state.pending_plugins.clone().unwrap_or(current),
        )
    }

    /// Serialize synchronous legacy writers with the first mounted publication.
    pub(crate) fn while_unmounted(&self, write: impl FnOnce()) {
        let state = self.state.lock();
        if state.mounted.is_none() {
            write();
        }
    }

    pub(crate) fn publish(
        &self,
        cancelled: &tokio_util::sync::CancellationToken,
        plugin_revision: u64,
        mounted: MountedConfig,
        commit: impl FnOnce() -> bool,
    ) -> bool {
        let mut state = self.state.lock();
        if state.closed || cancelled.is_cancelled() || state.plugin_revision != plugin_revision || !commit() {
            return false;
        }
        state.pending_plugins = None;
        state.pending_client_hooks = None;
        state.mounted = Some(std::sync::Arc::new(mounted));
        true
    }

    pub(crate) fn generation(&self) -> u64 {
        self.state.lock().generation
    }

    pub(crate) fn begin(&self, generation: u64) -> Option<tokio_util::sync::CancellationToken> {
        let mut state = self.state.lock();
        if state.closed || state.generation != generation {
            return None;
        }
        let token = tokio_util::sync::CancellationToken::new();
        state.preparing = Some(token.clone());
        Some(token)
    }

    pub(crate) fn cancel(&self) {
        let mut state = self.state.lock();
        state.generation += 1;
        if let Some(token) = state.preparing.take() {
            token.cancel();
        }
    }

    pub(crate) fn finish(&self, generation: u64) {
        let mut state = self.state.lock();
        if state.generation == generation {
            state.preparing = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_candidate_metadata_is_explicit_and_typed() {
        assert!(!required_from_meta(None).unwrap());
        for required in [false, true] {
            let meta = serde_json::json!({"x.ai/requireConfigCandidate":required});
            assert_eq!(required_from_meta(meta.as_object()).unwrap(), required);
        }
        for invalid in [serde_json::Value::Null, serde_json::json!("true"), serde_json::json!(1)] {
            let meta = serde_json::json!({"x.ai/requireConfigCandidate":invalid});
            assert!(required_from_meta(meta.as_object()).is_err());
        }
    }

    #[test]
    fn candidate_close_is_terminal_across_clones_and_finish() {
        use xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerActivationGate;
        let gate = SchedulerActivationGate::blocked();
        let admission = CandidateAdmission::new(true, Some(gate.clone()));
        let ticket = admission.generation();
        let token = admission.begin(ticket).unwrap();
        admission.activate_scheduler();
        assert!(!gate.is_active());
        admission.clone().close();
        assert!(token.is_cancelled());
        admission.finish(ticket);
        admission.cancel();
        assert!(admission.begin(admission.generation()).is_none());
        gate.activate();
        assert!(!gate.is_active());
    }
}
