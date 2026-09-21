//! Private Runtime transport, version 1. Rust is the source of truth for the
//! generated TypeScript declarations. Neither credentials nor request bodies
//! belong in diagnostic logs or event journals.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderRoute {
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub query_params: BTreeMap<String, String>,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    OpenaiChat,
    OpenaiResponses,
    Anthropic,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeModel {
    pub id: String,
    pub provider: ProviderRoute,
    /// Authoritative canonical effort capabilities. Omission grants none.
    #[serde(default)]
    pub supported_reasoning: Vec<crate::ReasoningEffort>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_completion_tokens: Option<u32>,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeConfig {
    pub models: Vec<RuntimeModel>,
    pub default_model: String,
    #[serde(default)]
    pub web_search_model: Option<String>,
    #[serde(default)]
    pub session_summary_model: Option<String>,
    #[serde(default)]
    pub compaction_model: Option<String>,
    #[serde(default)]
    pub image_description_model: Option<String>,
    #[serde(default)]
    pub browser: Option<BrowserConfig>,
    #[serde(default)]
    pub media: Option<crate::native_media::NativeMediaConfig>,
    #[serde(default)]
    pub subagents: Vec<crate::SubagentDefinition>,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserConfig {
    pub executable: String,
    /// Account/runtime-scoped persistent profile, never per product thread.
    pub data_dir: String,
    pub artifact_dir: String,
    pub headless: bool,
    #[serde(default)]
    pub no_sandbox: bool,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Workspace {
    /// Stable host-owned identity; paths always belong to the runtime machine.
    pub id: String,
    pub cwd: String,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionOptions {
    pub workspace: Workspace,
    /// Keep scheduler execution and mutations blocked until native candidate publication.
    /// Set on every create/load/resume requiring a product-owned configuration.
    #[serde(default)]
    #[ts(optional)]
    pub require_config_candidate: Option<bool>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub mcp_servers: Vec<crate::McpServer>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
}

/// Explicit product-service or client-device callbacks. Generic OS tools are
/// already native and must not be registered through this interface.
#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionDescriptor {
    pub id: String,
    pub workspace: Workspace,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Prompt {
    /// Passed unchanged as native promptId; never synthesized from UI state.
    pub turn_id: String,
    pub blocks: Vec<PromptBlock>,
    #[serde(default)]
    #[ts(optional)]
    pub config_candidate: Option<crate::ConfigCandidate>,
    #[serde(default)]
    #[ts(optional)]
    pub send_now: Option<bool>,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PromptBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        mime_type: String,
    },
    Audio {
        data: String,
        mime_type: String,
    },
    ResourceLink {
        name: String,
        uri: String,
    },
    EmbeddedText {
        uri: String,
        text: String,
        mime_type: Option<String>,
    },
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PromptReceipt {
    pub stop_reason: String,
    pub prompt_id: Option<String>,
    #[ts(type = "number | null")]
    pub prompt_index: Option<u64>,
    pub usage: Option<crate::TurnUsage>,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalOpenResult {
    pub terminal_id: String,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalCloseResult {
    pub terminal_id: String,
    pub exit_code: i32,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Update {
    UserText(String),
    AssistantText(String),
    ThoughtText(String),
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCall),
    Plan(Vec<PlanEntry>),
    TurnCompleted(crate::TurnCompletion),
    /// A native status category without a protocol envelope.
    Status {
        kind: String,
    },
    Compaction(CompactionUpdate),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[serde(
    tag = "phase",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum CompactionUpdate {
    Started {
        tokens_used: u64,
        context_window: u64,
        percentage: u8,
        reason: String,
    },
    Completed {
        tokens_before: Option<u64>,
        tokens_after: u64,
        elapsed_ms: Option<i64>,
        summary_preview: Option<String>,
    },
    Failed {
        error: String,
    },
    Cancelled {
        reason: String,
    },
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<Value>,
}

#[derive(Clone, Serialize, Deserialize, TS)]
pub struct PlanEntry {
    pub content: String,
    pub priority: String,
    pub status: String,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRecord {
    pub session_id: String,
    pub event_id: Option<String>,
    pub prompt_id: Option<String>,
    #[ts(type = "number | null")]
    pub prompt_index: Option<u64>,
    pub hide_from_scrollback: bool,
    pub model: Option<String>,
    pub is_replay: bool,
    pub update: Update,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct HistorySnapshot {
    pub session_id: String,
    pub revision: String,
    pub boundary_id: String,
    pub records: Vec<HistoryRecord>,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum RuntimeEvent {
    Queue {
        snapshot: crate::management::QueueSnapshot,
    },
    Scheduler {
        session_id: String,
        task_id: crate::management::ScheduledTaskId,
        version: crate::management::Version,
        occurrence: crate::management::ScheduledTaskEvent,
        snapshot_required: bool,
    },
    Subagent {
        event: crate::subagent::SubagentEvent,
    },
    HistoryRecord {
        record: HistoryRecord,
    },
    HistoryBoundary {
        session_id: String,
        boundary_id: String,
    },
    Session {
        session_id: String,
        update: Update,
        prompt_id: Option<String>,
    },
    /// The stream is no longer a valid display handoff. Resnapshot all sessions.
    Gap {
        dropped: u32,
    },
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CallbackContext {
    pub session_id: String,
    /// Session that registered this concrete tool handler; stable across inheritance.
    pub owner_session_id: String,
    pub prompt_id: Option<String>,
    pub tool_call_id: String,
    /// Actual invocation workspace on the runtime machine (including children).
    pub cwd: String,
    pub scheduled_invocation: Option<ScheduledInvocation>,
    pub originating_prompt: Option<NativePromptOrigin>,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct NativePromptOrigin {
    pub session_id: String,
    pub prompt_id: String,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledInvocation {
    /// Owning scheduler's native Session, not the invoking child Session.
    pub session_id: String,
    pub task_id: String,
    /// Exact consumed native cadence cursor in RFC3339, not a product Turn ID.
    pub occurrence: String,
}

#[derive(Clone, Serialize, Deserialize, TS)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ClientFrame {
    Request {
        id: String,
        request: Request,
    },
    CallbackResult {
        id: String,
        result: Option<Value>,
        error: Option<RpcError>,
    },
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ServerFrame {
    Ready {
        protocol_version: u32,
    },
    Response {
        id: String,
        result: Value,
    },
    Error {
        id: String,
        error: RpcError,
    },
    Event {
        sequence: u32,
        event: RuntimeEvent,
    },
    Callback {
        id: String,
        name: String,
        args: Value,
        context: CallbackContext,
    },
    CallbackCancelled {
        id: String,
    },
    /// Lossy bounded display channel, independent of ordered Session events.
    BrowserFrame {
        frame: Value,
    },
    Terminal {
        event: crate::native_terminal::TerminalEvent,
    },
    TerminalGap {
        dropped: u32,
    },
}

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(
    tag = "method",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Request {
    Initialize {
        config: RuntimeConfig,
    },
    CreateSession {
        options: SessionOptions,
    },
    LoadSession {
        id: String,
        options: SessionOptions,
    },
    ResumeSession {
        id: String,
        options: SessionOptions,
    },
    ListSessions {
        cwd: Option<String>,
        cursor: Option<String>,
    },
    Skills {
        cwd: String,
    },
    Prompt {
        session_id: String,
        prompt: Prompt,
    },
    Cancel {
        session_id: String,
        turn_id: Option<String>,
    },
    History {
        session_id: String,
    },
    Queue {
        session_id: String,
    },
    EffectiveConfig {
        session_id: String,
    },
    Dispose {
        session_id: String,
    },
    SetModel {
        session_id: String,
        model: String,
        reasoning_effort: Option<String>,
    },
    Browser {
        args: Value,
    },
    Terminal {
        request: crate::native_terminal::TerminalRequest,
    },
    ReadArtifact {
        session_id: String,
        path: String,
    },
    SubagentNewId {
        session_id: String,
    },
    SubagentStart {
        session_id: String,
        request: crate::subagent::SubagentStart,
    },
    SubagentQuery {
        session_id: String,
        id: crate::subagent::SubagentId,
    },
    SubagentWait {
        session_id: String,
        id: crate::subagent::SubagentId,
        timeout_ms: u32,
    },
    SubagentCancel {
        session_id: String,
        target: crate::subagent::SubagentHandle,
    },
    SubagentCancelId {
        session_id: String,
        id: crate::subagent::SubagentId,
    },
    SchedulerList {
        session_id: String,
    },
    SchedulerCreate {
        session_id: String,
        operation_id: crate::management::OperationId,
        expected: crate::management::Version,
        task: crate::management::ScheduledTaskCreate,
    },
    SchedulerUpdate {
        session_id: String,
        operation_id: crate::management::OperationId,
        expected: crate::management::Version,
        task: crate::management::ScheduledTaskUpdate,
    },
    SchedulerDelete {
        session_id: String,
        operation_id: crate::management::OperationId,
        expected: crate::management::Version,
        id: crate::management::ScheduledTaskId,
    },
    Quiesce {
        timeout_ms: u32,
    },
    FinalExit {
        timeout_ms: u32,
    },
}

/// Regenerate declarations in one command; imports refer only to generated DTOs.
pub fn export_types(path: &std::path::Path) -> Result<(), ts_rs::ExportError> {
    let config = ts_rs::Config::default()
        .with_out_dir(path)
        .with_large_int("number")
        .with_import_extension(Some("js"));
    ClientFrame::export_all(&config)?;
    ServerFrame::export_all(&config)?;
    SessionDescriptor::export_all(&config)?;
    HistorySnapshot::export_all(&config)?;
    crate::subagent::SubagentResult::export_all(&config)?;
    crate::subagent::SubagentSnapshot::export_all(&config)?;
    crate::subagent::SubagentCancelIdResult::export_all(&config)?;
    TerminalOpenResult::export_all(&config)?;
    TerminalCloseResult::export_all(&config)?;
    crate::management::SchedulerSnapshot::export_all(&config)?;
    crate::management::SessionEffectiveConfigSnapshot::export_all(&config)?;
    crate::management::SkillsSnapshot::export_all(&config)?;
    crate::management::SchedulerMutationResult::<crate::management::ScheduledTask>::export_all(
        &config,
    )?;
    PromptReceipt::export_all(&config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn removed_protocol_escape_hatches_are_rejected() {
        assert!(
            serde_json::from_value::<Request>(json!({
                "method": "extension", "name": "x.ai/models/list", "params": {}
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<SessionOptions>(json!({
                "workspace": {"id": "w", "cwd": "/workspace"}, "metadata": {}
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Prompt>(json!({
                "turnId": "p", "blocks": [], "metadata": {"sendNow": true}
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<PromptBlock>(json!({
                "type": "raw", "value": {"type": "text", "text": "hidden"}
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<crate::McpServer>(json!({
                "type": "http", "name": "legacy", "url": "https://example.test", "headers": []
            }))
            .is_err()
        );
    }
}
