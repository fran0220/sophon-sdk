//! In-process first-party registration against Grok Build's local tool registry.
//! No MCP server, loopback transport, or host-owned execution loop is involved.
use std::sync::Arc;

use serde_json::Value;
use xai_grok_tools::types::{
    tool::{ToolKind, ToolNamespace},
    tool_metadata::ToolMetadata,
};
use xai_tool_protocol::ToolId;
use xai_tool_runtime::{ListToolsContext, Tool, ToolCallContext, ToolError};
use xai_tool_types::ToolDescription;

use crate::{
    Error,
    protocol::{CallbackContext, ToolSpec},
};

/// Handlers run in the Runtime workspace. Dropping execute cancels its wait;
/// implementations must account for effects already issued before cancellation.
#[async_trait::async_trait]
pub trait NativeToolHandler: Send + Sync + 'static {
    async fn execute(
        &self,
        name: &str,
        args: Value,
        context: CallbackContext,
    ) -> Result<Value, Error>;
}

#[derive(Clone)]
pub struct NativeTool {
    pub spec: ToolSpec,
    pub handler: Arc<dyn NativeToolHandler>,
}

impl std::fmt::Debug for NativeTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeTool")
            .field("name", &self.spec.name)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RegisteredTool {
    pub tool: NativeTool,
    pub owner_session_id: String,
}

impl Tool for RegisteredTool {
    type Args = Value;
    type Output = Value;

    fn id(&self) -> ToolId {
        ToolId::new(format!("Sophon:{}", self.tool.spec.name)).expect("validated tool name")
    }
    fn description(&self, _: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            self.tool.spec.name.clone(),
            self.tool.spec.description.clone(),
        )
        .with_arguments_schema(self.tool.spec.input_schema.clone())
    }
    async fn run(&self, ctx: ToolCallContext, args: Value) -> Result<Value, ToolError> {
        // The native actor supplies this identity; never infer it from TS UI state.
        let invocation = ctx
            .get::<xai_grok_tools::registry::types::NativeInvocationContext>()
            .ok_or_else(|| ToolError::invalid_arguments("native invocation context is missing"))?;
        let context = CallbackContext {
            session_id: invocation.session_id.clone(),
            owner_session_id: self.owner_session_id.clone(),
            prompt_id: invocation.prompt_id.clone(),
            tool_call_id: ctx.call_id.to_string(),
            cwd: invocation.cwd.to_string_lossy().into_owned(),
            scheduled_invocation: invocation.scheduled_invocation.as_ref().map(|source| {
                crate::protocol::ScheduledInvocation {
                    session_id: source.session_id.clone(),
                    task_id: source.task_id.clone(),
                    occurrence: source.occurrence.to_rfc3339(),
                }
            }),
        };
        self.tool
            .handler
            .execute(&self.tool.spec.name, args, context)
            .await
            .map_err(|error| ToolError::invalid_arguments(error.to_string()))
    }
}

impl ToolMetadata for RegisteredTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }
    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }
    fn description_template(&self) -> &str {
        &self.tool.spec.description
    }
}

impl crate::Session {
    /// Register first-party handlers before admitting prompts. Names must not
    /// shadow any native tool. Registrations live with the native session.
    pub async fn register_tools(&self, tools: Vec<NativeTool>) -> Result<(), Error> {
        for tool in &tools {
            ToolId::new(format!("Sophon:{}", tool.spec.name))
                .map_err(|_| Error::invalid_config("invalid native tool name"))?;
        }
        self.agent.register_tools(self.id.clone(), tools).await
    }
}
