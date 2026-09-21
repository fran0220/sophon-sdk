//! Native FIFO configuration admission. Candidate data is not a sequence of setters.

use serde::{Deserialize, Serialize};

pub const CONFIG_CANDIDATE_META_KEY: &str = "x.sophon/configCandidate";

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
    preparing: Option<tokio_util::sync::CancellationToken>,
    mounted: Option<std::sync::Arc<MountedConfig>>,
}

pub(crate) struct MountedConfig {
    pub candidate: ConfigCandidate,
    pub config: crate::agent::config::Config,
    pub sampling: crate::sampling::SamplerConfig,
    pub mcp_servers: Vec<agent_client_protocol::McpServer>,
}

impl CandidateAdmission {
    pub(crate) fn mounted(&self) -> Option<std::sync::Arc<MountedConfig>> {
        self.state.lock().mounted.clone()
    }

    pub(crate) fn publish(
        &self,
        cancelled: &tokio_util::sync::CancellationToken,
        mounted: MountedConfig,
        commit: impl FnOnce() -> bool,
    ) -> bool {
        let mut state = self.state.lock();
        if cancelled.is_cancelled() || !commit() {
            return false;
        }
        state.mounted = Some(std::sync::Arc::new(mounted));
        true
    }

    pub(crate) fn generation(&self) -> u64 {
        self.state.lock().generation
    }

    pub(crate) fn begin(&self, generation: u64) -> Option<tokio_util::sync::CancellationToken> {
        let mut state = self.state.lock();
        if state.generation != generation {
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
