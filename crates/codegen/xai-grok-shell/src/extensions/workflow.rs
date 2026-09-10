//! Typed workflow management; never routes through the slash resolver.
use agent_client_protocol as acp;
use serde::Deserialize;
use xai_grok_tools::implementations::grok_build::workflow::{WorkflowSource, WorkflowToolInput};

use super::agent_runtime::AgentRuntime;
use crate::session::SessionCommand;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    session_id: acp::SessionId,
    action: Action,
}

/// Deliberately does not deserialize WorkflowToolInput's legacy aliases.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    List,
    Start {
        source: StartSource,
        args: Option<serde_json::Value>,
        agent_budget: Option<u64>,
    },
    Pause {
        run_id: String,
    },
    Resume {
        run_id: String,
        agent_budget: Option<u64>,
    },
    Stop {
        run_id: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StartSource {
    Name { name: String },
    Script { script: String },
    ScriptPath { script_path: String },
}

impl Action {
    pub(crate) fn into_input(self) -> Option<WorkflowToolInput> {
        let (source, args, agent_budget) = match self {
            Self::List => return None,
            Self::Start {
                source,
                args,
                agent_budget,
            } => (
                match source {
                    StartSource::Name { name } => WorkflowSource::Name { name },
                    StartSource::Script { script } => WorkflowSource::Script { script },
                    StartSource::ScriptPath { script_path } => {
                        WorkflowSource::ScriptPath { script_path }
                    }
                },
                args,
                agent_budget,
            ),
            Self::Pause { run_id } => (WorkflowSource::Pause { run_id }, None, None),
            Self::Stop { run_id } => (WorkflowSource::Stop { run_id }, None, None),
            Self::Resume {
                run_id,
                agent_budget,
            } => (
                WorkflowSource::Resume {
                    resume_from_run_id: run_id,
                },
                None,
                agent_budget,
            ),
        };
        Some(WorkflowToolInput {
            source,
            args,
            agent_budget,
            validate_only: false,
        })
    }
}

pub(crate) async fn handle(agent: &dyn AgentRuntime, args: &acp::ExtRequest) -> super::ExtResult {
    let req: Request = super::parse_params(args)?;
    let handle = agent
        .session_handle_waiting_for_load(&req.session_id)
        .await
        .ok_or_else(|| acp::Error::invalid_params().data("unknown session"))?;
    let (respond_to, response) = tokio::sync::oneshot::channel();
    handle
        .cmd_tx
        .send(SessionCommand::ManageWorkflow {
            action: req.action,
            respond_to,
        })
        .map_err(|_| acp::Error::internal_error().data("session actor stopped"))?;
    super::to_ext_response(
        response
            .await
            .map_err(|_| acp::Error::internal_error().data("workflow response dropped"))?,
    )
}

#[cfg(test)]
mod tests {
    use super::Action;

    #[test]
    fn strict_actions_reject_aliases_and_implicit_selectors() {
        for value in [
            serde_json::json!({"type": "start", "name": "legacy"}),
            serde_json::json!({"type": "pause"}),
            serde_json::json!({"type": "resume", "run_id": "wf_1", "args": {}}),
            serde_json::json!({"type": "runs"}),
        ] {
            assert!(serde_json::from_value::<Action>(value).is_err());
        }
        let action: Action =
            serde_json::from_value(serde_json::json!({"type": "stop", "run_id": ""})).unwrap();
        assert!(action.into_input().unwrap().validate().is_err());
    }
}
