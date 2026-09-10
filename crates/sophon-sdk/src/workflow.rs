//! Workflow run management backed exclusively by the native session manager.
use serde::{Deserialize, Serialize};

use crate::{Error, Session};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowStartSource {
    Name { name: String },
    Script { script: String },
    ScriptPath { script_path: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowStartRequest {
    pub source: WorkflowStartSource,
    pub args: Option<serde_json::Value>,
    /// Absolute cumulative native child-agent cap, not a per-resume allowance.
    pub agent_budget: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkflowAcknowledgement {
    pub run_id: String,
    pub name: String,
    pub script_path: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Active,
    UserPaused,
    BackOffPaused,
    NoProgressPaused,
    InfraPaused,
    Blocked,
    BudgetLimited,
    Interrupted,
    Complete,
    Failed,
    Cancelled,
}

impl WorkflowRunStatus {
    /// Native terminal classification. Failed/cancelled same-process runs may still resume.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Interrupted | Self::Complete | Self::Failed | Self::Cancelled
        )
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkflowRun {
    pub run_id: String,
    pub name: String,
    pub objective: String,
    pub status: WorkflowRunStatus,
    pub agent_budget: Option<u64>,
    pub agents_used: u64,
    pub elapsed_ms_floor: u64,
    pub result_summary: Option<String>,
    pub pause_message: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkflowRunsSnapshot {
    pub runs: Vec<WorkflowRun>,
}

impl Session {
    async fn workflow_request<T: serde::de::DeserializeOwned>(
        &self,
        action: serde_json::Value,
    ) -> Result<T, Error> {
        let response = self
            .extension(
                "x.ai/workflow/manage",
                serde_json::json!({"action": action}),
            )
            .await?;
        crate::management::deserialize_extension(response)
            .map_err(|error| Error::Operation(error.to_string()))
    }

    pub async fn workflow_runs(&self) -> Result<WorkflowRunsSnapshot, Error> {
        self.workflow_request(serde_json::json!({"type": "list"}))
            .await
    }

    pub async fn start_workflow(
        &self,
        request: WorkflowStartRequest,
    ) -> Result<WorkflowAcknowledgement, Error> {
        self.workflow_request(
            serde_json::json!({"type": "start", "source": request.source,
            "args": request.args, "agent_budget": request.agent_budget}),
        )
        .await
    }

    /// Requires the exact ID from a launch acknowledgement or run snapshot.
    pub async fn pause_workflow(&self, run_id: &str) -> Result<WorkflowAcknowledgement, Error> {
        self.workflow_request(serde_json::json!({"type": "pause", "run_id": run_id}))
            .await
    }

    /// Continues immutable native source/args; an exhausted budget needs a higher absolute cap.
    pub async fn resume_workflow(
        &self,
        run_id: &str,
        agent_budget: Option<u64>,
    ) -> Result<WorkflowAcknowledgement, Error> {
        self.workflow_request(serde_json::json!({"type": "resume", "run_id": run_id,
            "agent_budget": agent_budget}))
            .await
    }

    pub async fn stop_workflow(&self, run_id: &str) -> Result<WorkflowAcknowledgement, Error> {
        self.workflow_request(serde_json::json!({"type": "stop", "run_id": run_id}))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::WorkflowAcknowledgement;

    #[test]
    fn native_envelope_is_unwrapped_not_deserialized_as_an_ack() {
        let ack: WorkflowAcknowledgement = crate::management::deserialize_extension(
            serde_json::json!({"result": {"run_id": "wf_1", "name": "flow"}}),
        )
        .unwrap();
        assert_eq!(ack.run_id, "wf_1");
        assert!(ack.script_path.is_none());
        let error = crate::management::deserialize_extension::<WorkflowAcknowledgement>(
            serde_json::json!({"result": null, "error": "workflow_launch_failed: native refusal"}),
        )
        .unwrap_err();
        assert!(error.to_string().contains("native refusal"));
    }
}
