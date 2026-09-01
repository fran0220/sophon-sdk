//! Stable, provider-aware management contracts for an embedded agent.
//!
//! These types deliberately do not expose ACP or untyped JSON. Grok Build's
//! actors remain authoritative; snapshots are recovery points after an event
//! subscriber observes a broadcast lag or sequence gap.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::SessionId;

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub(crate) String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

string_id!(
    /// Stable ID of one native FIFO entry.
    QueueEntryId
);
string_id!(
    /// Client-chosen idempotency key for a management mutation.
    OperationId
);
string_id!(
    /// Stable ID of an upstream scheduled task.
    ScheduledTaskId
);
string_id!(
    /// Stable ID of an upstream background terminal task.
    BackgroundTaskId
);
string_id!(
    /// Stable ID of an upstream subagent.
    SubagentId
);

/// Actor-incarnation generation and monotonic revision.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Version {
    pub generation: String,
    pub revision: u64,
}

/// Structured management failure. Conflicts are returned as typed mutation
/// outcomes because the authoritative recovery snapshot is part of them.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct ManagementError {
    pub kind: ManagementErrorKind,
    pub message: String,
    pub session_id: Option<SessionId>,
    pub operation_id: Option<OperationId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ManagementErrorKind {
    InvalidRequest,
    NotFound,
    OperationIdReused,
    AdmissionClosed,
    AuthorityUnavailable,
    Timeout,
    RuntimeStopped,
    Upstream,
}

impl ManagementError {
    pub(crate) fn new(kind: ManagementErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            session_id: None,
            operation_id: None,
        }
    }

    pub(crate) fn session(mut self, session_id: SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub(crate) fn operation(mut self, operation_id: OperationId) -> Self {
        self.operation_id = Some(operation_id);
        self
    }
}

// ── Runtime lifecycle / quiesce ───────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeState {
    Starting,
    Ready,
    Quiescing,
    Quiesced,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeHealth {
    pub generation: u64,
    pub state: RuntimeState,
    pub failure: Option<RuntimeFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeFailure {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionState {
    Open,
    Quiescing,
    Quiesced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionSource {
    Human,
    Peer,
    Scheduler,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionSnapshot {
    pub generation: u64,
    pub state: AdmissionState,
    pub active: u64,
    pub accepted: u64,
    pub rejected: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDrainSnapshot {
    pub session_id: SessionId,
    pub queued_prompts: usize,
    pub running_prompt: bool,
    pub pending_interactions: usize,
    pub outstanding_background_tasks: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentDrainSnapshot {
    pub sessions: Vec<SessionDrainSnapshot>,
    pub subagents: usize,
    pub completion_presentations: usize,
    pub unreachable_sessions: Vec<SessionId>,
}

impl AgentDrainSnapshot {
    pub fn is_idle(&self) -> bool {
        self.unreachable_sessions.is_empty()
            && self.subagents == 0
            && self.completion_presentations == 0
            && self.sessions.iter().all(|session| {
                session.queued_prompts == 0
                    && !session.running_prompt
                    && session.pending_interactions == 0
                    && session.outstanding_background_tasks == 0
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuiesceReport {
    pub fence: AdmissionSnapshot,
    pub admission: AdmissionSnapshot,
    pub initial: AgentDrainSnapshot,
    pub final_snapshot: AgentDrainSnapshot,
    pub polls: u64,
    pub elapsed: Duration,
    pub timed_out: bool,
}

impl QuiesceReport {
    pub fn drained(&self) -> bool {
        !self.timed_out && self.admission.active == 0 && self.final_snapshot.is_idle()
    }

    pub fn rejected_during_quiesce(&self) -> u64 {
        self.admission.rejected.saturating_sub(self.fence.rejected)
    }
}

// ── Native FIFO ────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueEntry {
    pub id: QueueEntryId,
    pub version: u64,
    pub owner: Option<String>,
    pub last_editor: Option<String>,
    pub kind: String,
    pub text: String,
    pub combined_texts: Option<Vec<String>>,
    pub position: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningQueueEntry {
    pub id: QueueEntryId,
    pub kind: Option<String>,
    pub text: Option<String>,
    pub combined_texts: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueSnapshot {
    pub session_id: SessionId,
    pub version: Version,
    pub running: Option<RunningQueueEntry>,
    pub pending: Vec<QueueEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum QueueMutation {
    Remove {
        id: QueueEntryId,
        expected_entry_version: u64,
        owner: Option<String>,
    },
    Reorder {
        ordered_ids: Vec<QueueEntryId>,
    },
    Clear {
        owner: Option<String>,
    },
    Edit {
        id: QueueEntryId,
        expected_entry_version: u64,
        new_text: String,
        editor: Option<String>,
    },
    Interject {
        id: QueueEntryId,
        expected_entry_version: u64,
        owner: Option<String>,
        new_text: Option<String>,
    },
    Hold {
        id: QueueEntryId,
    },
    Release {
        id: QueueEntryId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueMutationRequest {
    pub operation_id: OperationId,
    pub expected: Version,
    pub mutation: QueueMutation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum QueueMutationResult {
    Committed {
        operation_id: OperationId,
        applied: bool,
        replayed: bool,
        committed_version: Version,
        snapshot: QueueSnapshot,
    },
    Conflict {
        operation_id: OperationId,
        expected: Version,
        actual: Version,
        snapshot: QueueSnapshot,
    },
    OperationIdReused {
        operation_id: OperationId,
    },
}

// ── Scheduler ──────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledTask {
    pub id: ScheduledTaskId,
    pub interval_secs: u64,
    pub prompt: String,
    pub recurring: bool,
    pub durable: bool,
    pub foreground: bool,
    pub created_at: String,
    pub last_fired_at: Option<String>,
    pub next_fire_at: Option<String>,
    pub expires_at: Option<String>,
    pub last_subagent_id: Option<SubagentId>,
    pub iterations_since_fresh: u32,
    pub chain_reset_pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchedulerSnapshot {
    pub version: Version,
    pub tasks: Vec<ScheduledTask>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledTaskCreate {
    /// Upstream supports recurring tasks only. Values below 60 seconds are
    /// clamped to 60 seconds by the same rule as the native scheduler tool.
    pub interval_secs: u64,
    pub prompt: String,
    pub durable: bool,
    pub foreground: bool,
    pub fire_immediately: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledTaskUpdate {
    pub id: ScheduledTaskId,
    pub prompt: Option<String>,
    pub interval_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SchedulerMutationResult<T> {
    Committed {
        operation_id: OperationId,
        value: T,
        version: Version,
        replayed: bool,
    },
    Conflict {
        operation_id: OperationId,
        expected: Version,
        snapshot: SchedulerSnapshot,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScheduledTaskRemovalReason {
    Deleted,
    Expired,
    Completed,
    RejectedByAdmissionFence,
    Unknown,
}

// ── Rewind ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RewindMode {
    All,
    ConversationOnly,
    FilesOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindPoint {
    pub prompt_index: usize,
    pub created_at: String,
    pub file_snapshot_count: usize,
    pub has_file_changes: bool,
    pub prompt_preview: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindSnapshot {
    pub version: Version,
    pub points: Vec<RewindPoint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindRequest {
    pub expected: Version,
    pub target_prompt_index: usize,
    pub force: bool,
    pub mode: RewindMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindConflict {
    pub path: PathBuf,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindResult {
    pub success: bool,
    pub target_prompt_index: usize,
    pub mode: RewindMode,
    pub reverted_files: Vec<PathBuf>,
    pub clean_files: Vec<PathBuf>,
    pub conflicts: Vec<RewindConflict>,
    pub prompt_text: Option<String>,
    pub error: Option<String>,
    pub used_compaction_replay: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RewindExecutionResult {
    Committed {
        version: Version,
        result: RewindResult,
    },
    Conflict {
        expected: Version,
        snapshot: RewindSnapshot,
    },
}

// ── Credential-free effective configuration ───────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderProtocol {
    OpenAiChatCompletions,
    OpenAiResponses,
    AnthropicMessages,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteFacts {
    pub route_id: String,
    pub base_url: String,
    pub model: String,
    pub protocol: ProviderProtocol,
    pub context_window: Option<u64>,
    /// Header/query names only. Values and credential locations are never
    /// included in a management snapshot.
    pub header_names: Vec<String>,
    pub query_parameter_names: Vec<String>,
    pub environment_header_names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuxiliaryRouteFacts {
    pub web_search_model: Option<String>,
    pub session_summary_model: Option<String>,
    pub image_description_model: Option<String>,
    pub prompt_suggestion_model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaRouteFacts {
    pub base_url: Option<String>,
    pub image_generation_enabled: bool,
    pub image_edit_enabled: bool,
    pub video_generation_enabled: bool,
    pub image_generation_model: Option<String>,
    pub image_edit_model: Option<String>,
    pub header_names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEffectiveConfigSnapshot {
    pub version: Version,
    pub default_model: Option<String>,
    pub routes: Vec<RouteFacts>,
    pub auxiliary: AuxiliaryRouteFacts,
    pub media: Option<MediaRouteFacts>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchOverrideFacts {
    pub x_search_from_date: Option<String>,
    pub x_search_to_date: Option<String>,
    pub web_allowed_domains: Vec<String>,
    pub web_excluded_domains: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRouteFacts {
    pub base_url: String,
    pub model: String,
    pub protocol: ProviderProtocol,
    pub context_window: u64,
    pub reasoning_effort: Option<String>,
    pub header_names: Vec<String>,
    pub query_parameter_names: Vec<String>,
    pub environment_header_names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEffectiveConfigSnapshot {
    pub session_id: SessionId,
    pub version: Version,
    pub route: SessionRouteFacts,
    pub backend_search_active: bool,
    /// Configuration attached to the batch currently draining.
    pub active_batch_search: SearchOverrideFacts,
    /// Configuration after every already-admitted FIFO row drains.
    pub next_empty_fifo_search: SearchOverrideFacts,
    pub pending_config_prompt_ids: Vec<QueueEntryId>,
}

// ── Session info and usage ─────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub reasoning_tokens: u64,
    pub model_calls: u64,
    pub api_duration_ms: u64,
    /// 1 USD is 10,000,000,000 ticks. Absent means unknown/untrustworthy,
    /// never free.
    pub cost_usd_ticks: Option<i64>,
    pub cost_is_partial: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionUsage {
    pub totals: UsageTotals,
    pub by_model: BTreeMap<String, UsageTotals>,
    pub turns: u64,
    pub incomplete: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContextUsageCategory {
    pub label: String,
    pub tokens: u64,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContextUsage {
    pub used: u64,
    pub total: u64,
    pub system_prompt_tokens: u64,
    pub tool_definitions_count: u64,
    pub tool_definitions_tokens: u64,
    pub compaction_count: u64,
    pub turn_count: u64,
    pub tool_call_count: u64,
    pub message_count: u64,
    pub message_tokens: u64,
    pub free_tokens: u64,
    pub usage_percent: u8,
    pub auto_compact_threshold_percent: u8,
    pub categories: Vec<ContextUsageCategory>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSessionInfo {
    pub session_id: SessionId,
    pub cwd: PathBuf,
    pub agent_name: Option<String>,
    pub model: Option<String>,
    pub model_display_name: Option<String>,
    pub resolved_model_id: Option<String>,
    pub model_fingerprint: Option<String>,
    pub show_model_fingerprint: bool,
    pub api_backend: Option<String>,
    pub conversation_id: Option<String>,
    pub turns: u64,
    pub turn_index: u64,
    pub context: ContextUsage,
}

// ── Hooks / skills / workflows / MCP ─────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HookEventKind {
    SessionStart,
    SessionEnd,
    Stop,
    StopFailure,
    StopCancelled,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionDenied,
    UserPromptSubmit,
    Notification,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HookHandlerKind {
    Command,
    Http,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookInfo {
    pub name: String,
    pub event: HookEventKind,
    pub handler: HookHandlerKind,
    pub matcher: Option<String>,
    pub command: Option<String>,
    pub url: Option<String>,
    pub timeout_ms: u64,
    pub source_dir: PathBuf,
    pub disabled: bool,
    pub pinned: bool,
    pub removable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HooksSnapshot {
    pub hooks: Vec<HookInfo>,
    pub project_trusted: bool,
    pub load_errors: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HookAction {
    Reload,
    TrustProject,
    UntrustProject,
    AddPath(PathBuf),
    RemovePath(PathBuf),
    Enable(String),
    Disable(String),
    ToggleSource {
        hook_names: Vec<String>,
        disable: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActionStatus {
    Success,
    ValidationError,
    ConfirmationRequired,
    NotFound,
    InternalError,
    Unsupported,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionOutcome {
    pub status: ActionStatus,
    pub message: String,
    pub requires_reload: bool,
    pub requires_restart: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkillScope {
    Local,
    Repository,
    User,
    Server,
    Bundled,
    Plugin,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillInfo {
    pub name: String,
    pub display_name: Option<String>,
    pub description: String,
    pub short_description: Option<String>,
    pub when_to_use: Option<String>,
    pub paths: Option<Vec<String>>,
    pub author: Option<String>,
    pub argument_hint: Option<String>,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub path: PathBuf,
    pub scope: SkillScope,
    pub plugin_name: Option<String>,
    pub plugin_version: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub user_invocable: bool,
    pub model_invocation_disabled: bool,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillsSnapshot {
    pub skills: Vec<SkillInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillsConfigSnapshot {
    pub paths: Vec<PathBuf>,
    pub ignored_paths: Vec<PathBuf>,
    pub skills: Vec<SkillInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowInfo {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub source: String,
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowsSnapshot {
    pub workflows: Vec<WorkflowInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpServerSource {
    Managed,
    Local,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpServerStatus {
    Ready,
    Initializing,
    SetupRequired,
    Unavailable,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpTransportFacts {
    Http {
        url: String,
        scope: Option<String>,
        scope_id: Option<String>,
        scope_name: Option<String>,
    },
    Stdio {
        command: PathBuf,
        args: Vec<String>,
        /// Environment variable names only; values are deliberately omitted.
        environment_names: Vec<String>,
    },
    ManagedGateway,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpToolInfo {
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerInfo {
    pub name: String,
    pub display_name: Option<String>,
    pub source: McpServerSource,
    pub source_label: Option<String>,
    pub transport: McpTransportFacts,
    pub enabled: Option<bool>,
    pub status: Option<McpServerStatus>,
    pub tools: Vec<McpToolInfo>,
    pub auth_required: bool,
    pub setup_required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpInventorySnapshot {
    pub servers: Vec<McpServerInfo>,
}

// ── Background tasks / subagents ──────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackgroundTaskKind {
    Command,
    Monitor,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundTask {
    pub id: BackgroundTaskId,
    pub command: String,
    pub display_command: Option<String>,
    pub cwd: PathBuf,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub output: String,
    pub output_file: PathBuf,
    pub output_truncated: bool,
    pub output_total_bytes: usize,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub completed: bool,
    pub kind: BackgroundTaskKind,
    pub explicitly_killed: bool,
    pub owner_session_id: Option<SessionId>,
    pub description: Option<String>,
    pub backgrounded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackgroundTaskKillSource {
    Client,
    Teardown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackgroundTaskKillOutcome {
    Killed,
    AlreadyExited,
    NotFound,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningSubagent {
    pub id: SubagentId,
    pub parent_session_id: SessionId,
    pub child_session_id: SessionId,
    pub subagent_type: String,
    pub description: String,
    pub started_at_epoch_ms: u64,
    pub duration_ms: u64,
    pub turn_count: u32,
    pub tool_call_count: u32,
    pub tokens_used: u64,
    pub context_window_tokens: u64,
    pub context_usage_percent: u8,
    pub tools_used: Vec<String>,
    pub error_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SubagentStatus {
    Initializing,
    Running {
        turn_count: u32,
        tool_call_count: u32,
        tokens_used: u64,
        context_window_tokens: u64,
        context_usage_percent: u8,
        tools_used: Vec<String>,
        error_count: u32,
    },
    Completed {
        output: String,
        tool_calls: u32,
        turns: u32,
        worktree_path: Option<PathBuf>,
    },
    Failed {
        error: String,
    },
    Cancelled {
        reason: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentSnapshot {
    pub id: SubagentId,
    pub parent_session_id: SessionId,
    pub child_session_id: SessionId,
    pub subagent_type: String,
    pub description: String,
    pub started_at_epoch_ms: u64,
    pub duration_ms: u64,
    pub status: SubagentStatus,
    pub fork_parent_prompt_id: Option<QueueEntryId>,
    pub resumed_from: Option<SubagentId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SubagentCancelOutcome {
    Cancelled,
    AlreadyFinished { status: String },
    NotFound,
}

// ── Management events ─────────────────────────────────────────────────

/// Global monotonic sequence shared by all management event domains. A
/// `broadcast::error::RecvError::Lagged` or sequence jump means the consumer
/// must fetch the corresponding authoritative snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagementEvent {
    pub sequence: u64,
    pub kind: ManagementEventKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ManagementEventKind {
    Runtime(RuntimeHealth),
    Queue(QueueSnapshot),
    EffectiveConfigChanged {
        session_id: SessionId,
        version: Version,
        snapshot_required: bool,
    },
    /// Scheduler notifications identify the occurrence but do not race a
    /// second scheduler mirror into the SDK. Fetch `scheduler_snapshot()` to
    /// recover the authoritative version after this invalidation.
    Scheduler {
        session_id: SessionId,
        task_id: ScheduledTaskId,
        version: Version,
        occurrence: ScheduledTaskEvent,
        snapshot_required: bool,
    },
    BackgroundTask {
        session_id: SessionId,
        task_id: BackgroundTaskId,
        occurrence: BackgroundTaskEvent,
        snapshot_required: bool,
    },
    Subagent {
        session_id: SessionId,
        subagent_id: SubagentId,
        occurrence: SubagentEvent,
        snapshot_required: bool,
    },
    HooksChanged {
        session_id: SessionId,
        snapshot_required: bool,
    },
    McpChanged {
        session_id: Option<SessionId>,
        snapshot_required: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScheduledTaskEvent {
    /// The upstream notification is emitted for both create and update.
    Upserted,
    Fired {
        subagent_id: Option<SubagentId>,
    },
    Removed {
        reason: ScheduledTaskRemovalReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackgroundTaskEvent {
    Started,
    Completed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SubagentEvent {
    Spawned,
    Progress,
    Finished { status: String },
}

// ── Upstream projections (private to the facade) ──────────────────────

pub(crate) fn queue_snapshot(snapshot: xai_prompt_queue::QueueChanged) -> QueueSnapshot {
    let running = snapshot.running_prompt_id.map(|id| RunningQueueEntry {
        id: QueueEntryId::new(id),
        kind: snapshot.running_kind,
        text: snapshot.running_text,
        combined_texts: snapshot.running_combined_texts,
    });
    QueueSnapshot {
        session_id: SessionId(snapshot.session_id),
        version: Version {
            generation: snapshot.generation,
            revision: snapshot.revision,
        },
        running,
        pending: snapshot
            .entries
            .into_iter()
            .map(|entry| QueueEntry {
                id: QueueEntryId::new(entry.id),
                version: entry.version,
                owner: entry.owner,
                last_editor: entry.last_editor,
                kind: entry.kind,
                text: entry.text,
                combined_texts: entry.combined_texts,
                position: entry.position,
            })
            .collect(),
    }
}

pub(crate) fn queue_mutation(
    request: QueueMutationRequest,
) -> xai_grok_shell::session::commands::QueueMutationRequest {
    use xai_grok_shell::session::commands::QueueMutation as Upstream;
    let mutation = match request.mutation {
        QueueMutation::Remove {
            id,
            expected_entry_version,
            owner,
        } => Upstream::Remove {
            id: id.0,
            expected_entry_version,
            owner,
        },
        QueueMutation::Reorder { ordered_ids } => Upstream::Reorder {
            ordered_ids: ordered_ids.into_iter().map(|id| id.0).collect(),
        },
        QueueMutation::Clear { owner } => Upstream::Clear { owner },
        QueueMutation::Edit {
            id,
            expected_entry_version,
            new_text,
            editor,
        } => Upstream::Edit {
            id: id.0,
            expected_entry_version,
            new_text,
            editor,
        },
        QueueMutation::Interject {
            id,
            expected_entry_version,
            owner,
            new_text,
        } => Upstream::Interject {
            id: id.0,
            expected_entry_version,
            owner,
            new_text,
        },
        QueueMutation::Hold { id } => Upstream::Hold { id: id.0 },
        QueueMutation::Release { id } => Upstream::Release { id: id.0 },
    };
    xai_grok_shell::session::commands::QueueMutationRequest {
        operation_id: request.operation_id.0,
        expected: xai_prompt_queue::QueueVersion {
            generation: request.expected.generation,
            revision: request.expected.revision,
        },
        mutation,
    }
}

pub(crate) fn queue_mutation_result(
    result: xai_grok_shell::session::commands::QueueMutationResult,
) -> QueueMutationResult {
    use xai_grok_shell::session::commands::QueueMutationResult as Upstream;
    match result {
        Upstream::Committed {
            operation_id,
            applied,
            replayed,
            committed_version,
            snapshot,
        } => QueueMutationResult::Committed {
            operation_id: OperationId::new(operation_id),
            applied,
            replayed,
            committed_version: Version {
                generation: committed_version.generation,
                revision: committed_version.revision,
            },
            snapshot: queue_snapshot(snapshot),
        },
        Upstream::Conflict {
            operation_id,
            expected,
            actual,
            snapshot,
        } => QueueMutationResult::Conflict {
            operation_id: OperationId::new(operation_id),
            expected: Version {
                generation: expected.generation,
                revision: expected.revision,
            },
            actual: Version {
                generation: actual.generation,
                revision: actual.revision,
            },
            snapshot: queue_snapshot(snapshot),
        },
        Upstream::OperationIdReused { operation_id } => QueueMutationResult::OperationIdReused {
            operation_id: OperationId::new(operation_id),
        },
        _ => unreachable!("unsupported queue mutation result from pinned Grok Build"),
    }
}

pub(crate) fn scheduler_version(
    version: xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerVersion,
) -> Version {
    Version {
        generation: version.generation(),
        revision: version.revision(),
    }
}

pub(crate) fn scheduled_task(
    task: xai_grok_tools::implementations::grok_build::scheduler::types::ScheduledTask,
) -> ScheduledTask {
    let now = chrono::Utc::now();
    let next_fire_at = task
        .pending_fire_at(now)
        .map(|timestamp| timestamp.to_rfc3339());
    ScheduledTask {
        id: ScheduledTaskId::new(task.id),
        interval_secs: task.interval_secs,
        prompt: task.prompt,
        recurring: task.recurring,
        durable: task.durable,
        foreground: task.foreground,
        created_at: task.created_at.to_rfc3339(),
        last_fired_at: task.last_fired_at.map(|timestamp| timestamp.to_rfc3339()),
        next_fire_at,
        expires_at: task.expires_at.map(|timestamp| timestamp.to_rfc3339()),
        last_subagent_id: task.last_subagent_id.map(SubagentId::new),
        iterations_since_fresh: task.iterations_since_fresh,
        chain_reset_pending: task.chain_reset_pending,
    }
}

pub(crate) fn scheduler_snapshot(
    snapshot: xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerSnapshot,
) -> SchedulerSnapshot {
    SchedulerSnapshot {
        version: scheduler_version(snapshot.version),
        tasks: snapshot.tasks.into_iter().map(scheduled_task).collect(),
    }
}

pub(crate) fn scheduler_task_result(
    result: xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerMutationResult<
        xai_grok_tools::implementations::grok_build::scheduler::types::ScheduledTask,
    >,
) -> SchedulerMutationResult<ScheduledTask> {
    use xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerMutationResult as Upstream;
    match result {
        Upstream::Committed {
            operation_id,
            value,
            version,
            replayed,
        } => SchedulerMutationResult::Committed {
            operation_id: OperationId::new(operation_id),
            value: scheduled_task(value),
            version: scheduler_version(version),
            replayed,
        },
        Upstream::Conflict {
            operation_id,
            expected,
            snapshot,
        } => SchedulerMutationResult::Conflict {
            operation_id: OperationId::new(operation_id),
            expected: scheduler_version(expected),
            snapshot: scheduler_snapshot(snapshot),
        },
        _ => unreachable!("unsupported scheduler result from pinned Grok Build"),
    }
}

pub(crate) fn scheduler_bool_result(
    result: xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerMutationResult<
        bool,
    >,
) -> SchedulerMutationResult<bool> {
    use xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerMutationResult as Upstream;
    match result {
        Upstream::Committed {
            operation_id,
            value,
            version,
            replayed,
        } => SchedulerMutationResult::Committed {
            operation_id: OperationId::new(operation_id),
            value,
            version: scheduler_version(version),
            replayed,
        },
        Upstream::Conflict {
            operation_id,
            expected,
            snapshot,
        } => SchedulerMutationResult::Conflict {
            operation_id: OperationId::new(operation_id),
            expected: scheduler_version(expected),
            snapshot: scheduler_snapshot(snapshot),
        },
        _ => unreachable!("unsupported scheduler result from pinned Grok Build"),
    }
}

pub(crate) fn rewind_snapshot(snapshot: xai_grok_shell::session::RewindSnapshot) -> RewindSnapshot {
    RewindSnapshot {
        version: Version {
            generation: snapshot.version.generation,
            revision: snapshot.version.revision,
        },
        points: snapshot
            .rewind_points
            .into_iter()
            .map(|point| RewindPoint {
                prompt_index: point.prompt_index,
                created_at: point.created_at,
                file_snapshot_count: point.num_file_snapshots,
                has_file_changes: point.has_file_changes,
                prompt_preview: point.prompt_preview,
            })
            .collect(),
    }
}

pub(crate) fn rewind_mode(mode: RewindMode) -> xai_grok_shell::session::RewindMode {
    match mode {
        RewindMode::All => xai_grok_shell::session::RewindMode::All,
        RewindMode::ConversationOnly => xai_grok_shell::session::RewindMode::ConversationOnly,
        RewindMode::FilesOnly => xai_grok_shell::session::RewindMode::FilesOnly,
    }
}

fn projected_rewind_mode(mode: xai_grok_shell::session::RewindMode) -> RewindMode {
    match mode {
        xai_grok_shell::session::RewindMode::All => RewindMode::All,
        xai_grok_shell::session::RewindMode::ConversationOnly => RewindMode::ConversationOnly,
        xai_grok_shell::session::RewindMode::FilesOnly => RewindMode::FilesOnly,
    }
}

pub(crate) fn rewind_execution_result(
    result: xai_grok_shell::session::RewindExecutionResult,
) -> RewindExecutionResult {
    use xai_grok_shell::session::RewindExecutionResult as Upstream;
    match result {
        Upstream::Committed {
            version,
            response,
            used_compaction_replay,
        } => RewindExecutionResult::Committed {
            version: Version {
                generation: version.generation,
                revision: version.revision,
            },
            result: RewindResult {
                success: response.success,
                target_prompt_index: response.target_prompt_index,
                mode: projected_rewind_mode(response.mode),
                reverted_files: response
                    .reverted_files
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
                clean_files: response
                    .clean_files
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
                conflicts: response
                    .conflicts
                    .into_iter()
                    .map(|conflict| RewindConflict {
                        path: PathBuf::from(conflict.path),
                        kind: conflict.conflict_type,
                    })
                    .collect(),
                prompt_text: response.prompt_text,
                error: response.error,
                used_compaction_replay,
            },
        },
        Upstream::Conflict { expected, snapshot } => RewindExecutionResult::Conflict {
            expected: Version {
                generation: expected.generation,
                revision: expected.revision,
            },
            snapshot: rewind_snapshot(snapshot),
        },
        _ => unreachable!("unsupported rewind result from pinned Grok Build"),
    }
}

fn search_facts(
    facts: xai_grok_shell::session::commands::SearchOverrideFacts,
) -> SearchOverrideFacts {
    SearchOverrideFacts {
        x_search_from_date: facts.x_search_from_date,
        x_search_to_date: facts.x_search_to_date,
        web_allowed_domains: facts.web_allowed_domains,
        web_excluded_domains: facts.web_excluded_domains,
    }
}

pub(crate) fn effective_config_snapshot(
    snapshot: xai_grok_shell::session::commands::SessionEffectiveConfigSnapshot,
) -> SessionEffectiveConfigSnapshot {
    SessionEffectiveConfigSnapshot {
        session_id: SessionId(snapshot.session_id),
        version: Version {
            generation: snapshot.version.generation,
            revision: snapshot.version.revision,
        },
        route: SessionRouteFacts {
            base_url: snapshot.route.base_url,
            model: snapshot.route.model,
            protocol: protocol_from_backend(&snapshot.route.api_backend),
            context_window: snapshot.route.context_window,
            reasoning_effort: snapshot.route.reasoning_effort,
            header_names: snapshot.route.header_names,
            query_parameter_names: snapshot.route.query_parameter_names,
            environment_header_names: snapshot.route.environment_header_names,
        },
        backend_search_active: snapshot.backend_search_active,
        active_batch_search: search_facts(snapshot.active_batch_search),
        next_empty_fifo_search: search_facts(snapshot.next_empty_fifo_search),
        pending_config_prompt_ids: snapshot
            .pending_config_prompt_ids
            .into_iter()
            .map(QueueEntryId::new)
            .collect(),
    }
}

fn protocol_from_backend(backend: &str) -> ProviderProtocol {
    match backend.to_ascii_lowercase().as_str() {
        "chat_completions" | "chatcompletions" | "openai_chat_completions" => {
            ProviderProtocol::OpenAiChatCompletions
        }
        "responses" | "openai_responses" => ProviderProtocol::OpenAiResponses,
        "messages" | "anthropic_messages" => ProviderProtocol::AnthropicMessages,
        _ => ProviderProtocol::Other,
    }
}

pub(crate) fn quiesce_report(
    report: xai_grok_shell::agent::activity::QuiesceReport,
) -> QuiesceReport {
    QuiesceReport {
        fence: admission_snapshot(report.fence),
        admission: admission_snapshot(report.admission),
        initial: drain_snapshot(report.initial),
        final_snapshot: drain_snapshot(report.final_snapshot),
        polls: report.polls,
        elapsed: report.elapsed,
        timed_out: report.timed_out,
    }
}

fn admission_snapshot(
    snapshot: xai_grok_tools::management::admission::AdmissionSnapshot,
) -> AdmissionSnapshot {
    use xai_grok_tools::management::admission::AdmissionState as Upstream;
    AdmissionSnapshot {
        generation: snapshot.generation,
        state: match snapshot.state {
            Upstream::Open => AdmissionState::Open,
            Upstream::Quiescing => AdmissionState::Quiescing,
            Upstream::Quiesced => AdmissionState::Quiesced,
            _ => unreachable!("unsupported admission state from pinned Grok Build"),
        },
        active: snapshot.active,
        accepted: snapshot.accepted,
        rejected: snapshot.rejected,
    }
}

fn drain_snapshot(
    snapshot: xai_grok_shell::agent::activity::AgentDrainSnapshot,
) -> AgentDrainSnapshot {
    AgentDrainSnapshot {
        sessions: snapshot
            .sessions
            .into_iter()
            .map(|session| SessionDrainSnapshot {
                session_id: SessionId(session.session_id),
                queued_prompts: session.queued_prompts,
                running_prompt: session.running_prompt,
                pending_interactions: session.pending_interactions,
                outstanding_background_tasks: session.outstanding_background_tasks,
            })
            .collect(),
        subagents: snapshot.subagents,
        completion_presentations: snapshot.presentations,
        unreachable_sessions: snapshot
            .unreachable_sessions
            .into_iter()
            .map(SessionId)
            .collect(),
    }
}

pub(crate) fn background_task(task: xai_grok_tools::types::TaskSnapshot) -> BackgroundTask {
    use xai_grok_tools::computer::types::TaskKind;
    BackgroundTask {
        id: BackgroundTaskId::new(task.task_id),
        command: task.command,
        display_command: task.display_command,
        cwd: PathBuf::from(task.cwd),
        started_at: xai_grok_tools::types::format_system_time_rfc3339(task.start_time),
        ended_at: task
            .end_time
            .map(xai_grok_tools::types::format_system_time_rfc3339),
        output: task.output,
        output_file: task.output_file,
        output_truncated: task.truncated,
        output_total_bytes: task.output_total_bytes,
        exit_code: task.exit_code,
        signal: task.signal,
        completed: task.completed,
        kind: match task.kind {
            TaskKind::Bash => BackgroundTaskKind::Command,
            TaskKind::Monitor => BackgroundTaskKind::Monitor,
        },
        explicitly_killed: task.explicitly_killed,
        owner_session_id: task.owner_session_id.map(SessionId),
        description: task.description,
        backgrounded: task.is_backgrounded,
    }
}

pub(crate) fn running_subagent(
    inspection: xai_grok_tools::implementations::grok_build::task::types::SubagentInspection,
) -> RunningSubagent {
    use xai_grok_tools::implementations::grok_build::task::types::SubagentSnapshotStatus;
    let SubagentSnapshotStatus::Running {
        turn_count,
        tool_call_count,
        tokens_used,
        context_window_tokens,
        context_usage_pct,
        tools_used,
        error_count,
    } = inspection.snapshot.status
    else {
        unreachable!("the upstream list_running authority returned a terminal subagent")
    };
    RunningSubagent {
        id: SubagentId::new(inspection.snapshot.subagent_id),
        parent_session_id: SessionId(inspection.parent_session_id),
        child_session_id: SessionId(inspection.child_session_id),
        subagent_type: inspection.snapshot.subagent_type,
        description: inspection.snapshot.description,
        started_at_epoch_ms: inspection.snapshot.started_at_epoch_ms,
        duration_ms: inspection.snapshot.duration_ms,
        turn_count,
        tool_call_count,
        tokens_used,
        context_window_tokens,
        context_usage_percent: context_usage_pct,
        tools_used,
        error_count,
    }
}

pub(crate) fn subagent_snapshot(
    inspection: xai_grok_tools::implementations::grok_build::task::types::SubagentInspection,
) -> SubagentSnapshot {
    use xai_grok_tools::implementations::grok_build::task::types::SubagentSnapshotStatus as Upstream;
    let status = match inspection.snapshot.status {
        Upstream::Initializing => SubagentStatus::Initializing,
        Upstream::Running {
            turn_count,
            tool_call_count,
            tokens_used,
            context_window_tokens,
            context_usage_pct,
            tools_used,
            error_count,
        } => SubagentStatus::Running {
            turn_count,
            tool_call_count,
            tokens_used,
            context_window_tokens,
            context_usage_percent: context_usage_pct,
            tools_used,
            error_count,
        },
        Upstream::Completed {
            output,
            tool_calls,
            turns,
            worktree_path,
        } => SubagentStatus::Completed {
            output,
            tool_calls,
            turns,
            worktree_path: worktree_path.map(PathBuf::from),
        },
        Upstream::Failed { error } => SubagentStatus::Failed { error },
        Upstream::Cancelled { reason } => SubagentStatus::Cancelled { reason },
    };
    SubagentSnapshot {
        id: SubagentId::new(inspection.snapshot.subagent_id),
        parent_session_id: SessionId(inspection.parent_session_id),
        child_session_id: SessionId(inspection.child_session_id),
        subagent_type: inspection.snapshot.subagent_type,
        description: inspection.snapshot.description,
        started_at_epoch_ms: inspection.snapshot.started_at_epoch_ms,
        duration_ms: inspection.snapshot.duration_ms,
        status,
        fork_parent_prompt_id: inspection.fork_parent_prompt_id.map(QueueEntryId::new),
        resumed_from: inspection.resumed_from.map(SubagentId::new),
    }
}

pub(crate) fn agent_config_snapshot(
    config: &crate::config::AgentConfig,
) -> AgentEffectiveConfigSnapshot {
    let routes = config
        .models
        .iter()
        .map(|model| RouteFacts {
            route_id: model.id.clone(),
            base_url: model.provider.base_url.clone(),
            model: model.provider.model.clone(),
            protocol: match model.provider.protocol {
                crate::config::ProviderProtocol::OpenAiChatCompletions => {
                    ProviderProtocol::OpenAiChatCompletions
                }
                crate::config::ProviderProtocol::OpenAiResponses => {
                    ProviderProtocol::OpenAiResponses
                }
                crate::config::ProviderProtocol::AnthropicMessages => {
                    ProviderProtocol::AnthropicMessages
                }
            },
            context_window: Some(model.context_window.get()),
            header_names: model.provider.headers.keys().cloned().collect(),
            query_parameter_names: model.provider.query_params.keys().cloned().collect(),
            environment_header_names: Vec::new(),
        })
        .collect();
    let media = config.media.as_ref().map(|media| MediaRouteFacts {
        base_url: Some(media.provider.base_url.clone()),
        image_generation_enabled: media.image_generation,
        image_edit_enabled: media.image_edit,
        video_generation_enabled: media.video_generation,
        image_generation_model: media.image_generation_model.clone(),
        image_edit_model: media.image_edit_model.clone(),
        header_names: media.provider.headers.keys().cloned().collect(),
    });
    AgentEffectiveConfigSnapshot {
        version: Version {
            generation: uuid::Uuid::new_v4().to_string(),
            revision: 0,
        },
        default_model: config.default_model.clone(),
        routes,
        auxiliary: AuxiliaryRouteFacts {
            web_search_model: config.web_search_model.clone(),
            session_summary_model: config.session_summary_model.clone(),
            image_description_model: config.image_description_model.clone(),
            prompt_suggestion_model: config.prompt_suggestion_model.clone(),
        },
        media,
    }
}

pub(crate) fn extension_result(
    response: serde_json::Value,
) -> Result<serde_json::Value, ManagementError> {
    let Some(object) = response.as_object() else {
        return Ok(response);
    };
    if !object.contains_key("result") {
        return Ok(response);
    }
    if let Some(result) = object.get("result").filter(|value| !value.is_null()) {
        return Ok(result.clone());
    }
    let message = object
        .get("error")
        .and_then(|error| {
            error
                .as_str()
                .map(str::to_owned)
                .or_else(|| error.get("message")?.as_str().map(str::to_owned))
        })
        .unwrap_or_else(|| "management extension returned no result".into());
    Err(ManagementError::new(ManagementErrorKind::Upstream, message))
}

pub(crate) fn deserialize_extension<T: serde::de::DeserializeOwned>(
    response: serde_json::Value,
) -> Result<T, ManagementError> {
    serde_json::from_value(extension_result(response)?).map_err(|error| {
        ManagementError::new(
            ManagementErrorKind::Upstream,
            format!("invalid typed management response: {error}"),
        )
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageResponseWire {
    usage: UsageWire,
}

#[derive(serde::Deserialize)]
struct UsageWire {
    #[serde(flatten)]
    totals: UsageTotalsWire,
    #[serde(default, rename = "modelUsage")]
    model_usage: BTreeMap<String, UsageTotalsWire>,
    #[serde(default, rename = "numTurns")]
    turns: u64,
    #[serde(default, rename = "usageIsIncomplete")]
    incomplete: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageTotalsWire {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    cached_read_tokens: u64,
    #[serde(default)]
    cache_creation_tokens: u64,
    #[serde(default)]
    reasoning_tokens: u64,
    #[serde(default)]
    model_calls: u64,
    #[serde(default)]
    api_duration_ms: u64,
    #[serde(default)]
    cost_usd_ticks: Option<i64>,
    #[serde(default)]
    cost_is_partial: bool,
}

fn usage_totals(wire: UsageTotalsWire) -> UsageTotals {
    UsageTotals {
        input_tokens: wire.input_tokens,
        output_tokens: wire.output_tokens,
        total_tokens: wire.total_tokens,
        cached_read_tokens: wire.cached_read_tokens,
        cache_creation_tokens: wire.cache_creation_tokens,
        reasoning_tokens: wire.reasoning_tokens,
        model_calls: wire.model_calls,
        api_duration_ms: wire.api_duration_ms,
        cost_usd_ticks: wire.cost_usd_ticks,
        cost_is_partial: wire.cost_is_partial,
    }
}

pub(crate) fn session_usage(response: serde_json::Value) -> Result<SessionUsage, ManagementError> {
    let response: UsageResponseWire = deserialize_extension(response)?;
    Ok(SessionUsage {
        totals: usage_totals(response.usage.totals),
        by_model: response
            .usage
            .model_usage
            .into_iter()
            .map(|(model, usage)| (model, usage_totals(usage)))
            .collect(),
        turns: response.usage.turns,
        incomplete: response.usage.incomplete,
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LiveSessionInfoWire {
    session_id: String,
    cwd: String,
    #[serde(default)]
    agent_name: Option<String>,
    model: Option<String>,
    #[serde(default)]
    model_display_name: Option<String>,
    resolved_model_id: Option<String>,
    model_fingerprint: Option<String>,
    #[serde(default)]
    show_model_fingerprint: bool,
    #[serde(default)]
    api_backend: Option<String>,
    #[serde(default)]
    conversation_id: Option<String>,
    turns: u64,
    #[serde(default)]
    turn_index: u64,
    context: ContextUsageWire,
}

#[derive(Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ContextUsageWire {
    used: u64,
    total: u64,
    system_prompt_tokens: u64,
    tool_definitions_count: u64,
    tool_definitions_tokens: u64,
    compaction_count: u64,
    turn_count: u64,
    tool_call_count: u64,
    message_count: u64,
    message_tokens: u64,
    free_tokens: u64,
    usage_pct: u8,
    auto_compact_threshold_percent: u8,
    usage_categories: Vec<ContextCategoryWire>,
}

#[derive(serde::Deserialize)]
struct ContextCategoryWire {
    label: String,
    tokens: u64,
    detail: Option<String>,
}

pub(crate) fn live_session_info(
    response: serde_json::Value,
) -> Result<LiveSessionInfo, ManagementError> {
    let wire: LiveSessionInfoWire = deserialize_extension(response)?;
    Ok(LiveSessionInfo {
        session_id: SessionId(wire.session_id),
        cwd: PathBuf::from(wire.cwd),
        agent_name: wire.agent_name,
        model: wire.model,
        model_display_name: wire.model_display_name,
        resolved_model_id: wire.resolved_model_id,
        model_fingerprint: wire.model_fingerprint,
        show_model_fingerprint: wire.show_model_fingerprint,
        api_backend: wire.api_backend,
        conversation_id: wire.conversation_id,
        turns: wire.turns,
        turn_index: wire.turn_index,
        context: ContextUsage {
            used: wire.context.used,
            total: wire.context.total,
            system_prompt_tokens: wire.context.system_prompt_tokens,
            tool_definitions_count: wire.context.tool_definitions_count,
            tool_definitions_tokens: wire.context.tool_definitions_tokens,
            compaction_count: wire.context.compaction_count,
            turn_count: wire.context.turn_count,
            tool_call_count: wire.context.tool_call_count,
            message_count: wire.context.message_count,
            message_tokens: wire.context.message_tokens,
            free_tokens: wire.context.free_tokens,
            usage_percent: wire.context.usage_pct,
            auto_compact_threshold_percent: wire.context.auto_compact_threshold_percent,
            categories: wire
                .context
                .usage_categories
                .into_iter()
                .map(|category| ContextUsageCategory {
                    label: category.label,
                    tokens: category.tokens,
                    detail: category.detail,
                })
                .collect(),
        },
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HooksSnapshotWire {
    hooks: Vec<HookInfoWire>,
    project_trusted: bool,
    #[serde(default)]
    load_errors: Vec<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookInfoWire {
    name: String,
    event: String,
    handler_type: String,
    matcher: Option<String>,
    command: Option<String>,
    url: Option<String>,
    timeout_ms: u64,
    source_dir: String,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    pinned: bool,
    #[serde(default)]
    removable: bool,
}

pub(crate) fn hooks_snapshot(
    response: serde_json::Value,
) -> Result<HooksSnapshot, ManagementError> {
    let wire: HooksSnapshotWire = deserialize_extension(response)?;
    Ok(HooksSnapshot {
        hooks: wire
            .hooks
            .into_iter()
            .map(|hook| HookInfo {
                name: hook.name,
                event: match hook.event.as_str() {
                    "session_start" => HookEventKind::SessionStart,
                    "session_end" => HookEventKind::SessionEnd,
                    "stop" => HookEventKind::Stop,
                    "stop_failure" => HookEventKind::StopFailure,
                    "stop_cancelled" => HookEventKind::StopCancelled,
                    "pre_tool_use" => HookEventKind::PreToolUse,
                    "post_tool_use" => HookEventKind::PostToolUse,
                    "post_tool_use_failure" => HookEventKind::PostToolUseFailure,
                    "permission_denied" => HookEventKind::PermissionDenied,
                    "user_prompt_submit" => HookEventKind::UserPromptSubmit,
                    "notification" => HookEventKind::Notification,
                    "subagent_start" => HookEventKind::SubagentStart,
                    "subagent_stop" => HookEventKind::SubagentStop,
                    "pre_compact" => HookEventKind::PreCompact,
                    "post_compact" => HookEventKind::PostCompact,
                    _ => HookEventKind::Unknown,
                },
                handler: match hook.handler_type.as_str() {
                    "command" => HookHandlerKind::Command,
                    "http" => HookHandlerKind::Http,
                    _ => HookHandlerKind::Unknown,
                },
                matcher: hook.matcher,
                command: hook.command,
                url: hook.url,
                timeout_ms: hook.timeout_ms,
                source_dir: PathBuf::from(hook.source_dir),
                disabled: hook.disabled,
                pinned: hook.pinned,
                removable: hook.removable,
            })
            .collect(),
        project_trusted: wire.project_trusted,
        load_errors: wire.load_errors,
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionOutcomeWire {
    status: String,
    message: String,
    requires_reload: bool,
    requires_restart: bool,
}

pub(crate) fn action_outcome(
    response: serde_json::Value,
) -> Result<ActionOutcome, ManagementError> {
    let wire: ActionOutcomeWire = deserialize_extension(response)?;
    Ok(ActionOutcome {
        status: match wire.status.as_str() {
            "success" => ActionStatus::Success,
            "validation_error" => ActionStatus::ValidationError,
            "confirmation_required" => ActionStatus::ConfirmationRequired,
            "not_found" => ActionStatus::NotFound,
            "internal_error" => ActionStatus::InternalError,
            "unsupported" => ActionStatus::Unsupported,
            _ => ActionStatus::Unknown,
        },
        message: wire.message,
        requires_reload: wire.requires_reload,
        requires_restart: wire.requires_restart,
    })
}

#[derive(serde::Deserialize)]
struct SkillsResponseWire {
    skills: Vec<SkillInfoWire>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkillsConfigWire {
    paths: Vec<String>,
    ignore: Vec<String>,
    skills: Vec<SkillInfoWire>,
}

#[derive(serde::Deserialize)]
struct SkillInfoWire {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    description: String,
    #[serde(default)]
    paths: Option<Vec<String>>,
    #[serde(default)]
    when_to_use: Option<String>,
    #[serde(default)]
    short_description: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    argument_hint: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    compatibility: Option<String>,
    #[serde(default)]
    metadata: Option<BTreeMap<String, String>>,
    path: String,
    scope: String,
    #[serde(default)]
    plugin_name: Option<String>,
    #[serde(default)]
    plugin_version: Option<String>,
    #[serde(default)]
    allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default = "default_true")]
    user_invocable: bool,
    #[serde(default)]
    disable_model_invocation: bool,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

fn skill_info(wire: SkillInfoWire) -> SkillInfo {
    SkillInfo {
        name: wire.name,
        display_name: wire.display_name,
        description: wire.description,
        short_description: wire.short_description,
        when_to_use: wire.when_to_use,
        paths: wire.paths,
        author: wire.author,
        argument_hint: wire.argument_hint,
        license: wire.license,
        compatibility: wire.compatibility,
        metadata: wire.metadata.unwrap_or_default(),
        path: PathBuf::from(wire.path),
        scope: match wire.scope.as_str() {
            "local" => SkillScope::Local,
            "repo" => SkillScope::Repository,
            "user" => SkillScope::User,
            "server" => SkillScope::Server,
            "bundled" => SkillScope::Bundled,
            "plugin" => SkillScope::Plugin,
            _ => SkillScope::Unknown,
        },
        plugin_name: wire.plugin_name,
        plugin_version: wire.plugin_version,
        allowed_tools: wire.allowed_tools,
        model: wire.model,
        effort: wire.effort,
        user_invocable: wire.user_invocable,
        model_invocation_disabled: wire.disable_model_invocation,
        enabled: wire.enabled,
    }
}

pub(crate) fn skills_snapshot(
    response: serde_json::Value,
) -> Result<SkillsSnapshot, ManagementError> {
    let wire: SkillsResponseWire = deserialize_extension(response)?;
    Ok(SkillsSnapshot {
        skills: wire.skills.into_iter().map(skill_info).collect(),
    })
}

pub(crate) fn skills_config_snapshot(
    response: serde_json::Value,
) -> Result<SkillsConfigSnapshot, ManagementError> {
    let wire: SkillsConfigWire = deserialize_extension(response)?;
    Ok(SkillsConfigSnapshot {
        paths: wire.paths.into_iter().map(PathBuf::from).collect(),
        ignored_paths: wire.ignore.into_iter().map(PathBuf::from).collect(),
        skills: wire.skills.into_iter().map(skill_info).collect(),
    })
}

#[derive(serde::Deserialize)]
struct WorkflowsWire {
    workflows: Vec<WorkflowInfoWire>,
}

#[derive(serde::Deserialize)]
struct WorkflowInfoWire {
    name: String,
    description: String,
    #[serde(default)]
    when_to_use: Option<String>,
    source: String,
    #[serde(default)]
    path: Option<String>,
}

pub(crate) fn workflows_snapshot(
    response: serde_json::Value,
) -> Result<WorkflowsSnapshot, ManagementError> {
    let wire: WorkflowsWire = deserialize_extension(response)?;
    Ok(WorkflowsSnapshot {
        workflows: wire
            .workflows
            .into_iter()
            .map(|workflow| WorkflowInfo {
                name: workflow.name,
                description: workflow.description,
                when_to_use: workflow.when_to_use,
                source: workflow.source,
                path: workflow.path.map(PathBuf::from),
            })
            .collect(),
    })
}

#[derive(serde::Deserialize)]
struct McpInventoryWire {
    servers: Vec<McpServerWire>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpServerWire {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    source: String,
    #[serde(default)]
    source_label: Option<String>,
    #[serde(flatten)]
    transport: McpTransportWire,
    #[serde(default)]
    session: Option<McpSessionWire>,
}

#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum McpTransportWire {
    #[serde(rename = "http")]
    Http {
        url: String,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default, rename = "scopeId")]
        scope_id: Option<String>,
        #[serde(default, rename = "scopeName")]
        scope_name: Option<String>,
    },
    #[serde(rename = "stdio")]
    Stdio {
        command: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<McpEnvironmentWire>,
    },
    #[serde(rename = "managedGateway")]
    ManagedGateway,
}

#[derive(serde::Deserialize)]
struct McpEnvironmentWire {
    name: String,
    #[allow(dead_code)]
    value: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpSessionWire {
    enabled: bool,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    tools: Vec<McpToolWire>,
    #[serde(default)]
    auth_required: bool,
    #[serde(default)]
    setup_required: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpToolWire {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
}

pub(crate) fn mcp_inventory_snapshot(
    response: serde_json::Value,
) -> Result<McpInventorySnapshot, ManagementError> {
    let wire: McpInventoryWire = deserialize_extension(response)?;
    Ok(McpInventorySnapshot {
        servers: wire
            .servers
            .into_iter()
            .map(|server| {
                let (enabled, status, tools, auth_required, setup_required) = server
                    .session
                    .map(|session| {
                        (
                            Some(session.enabled),
                            session.status.map(|status| match status.as_str() {
                                "ready" => McpServerStatus::Ready,
                                "initializing" => McpServerStatus::Initializing,
                                "setuprequired" | "setup_required" => {
                                    McpServerStatus::SetupRequired
                                }
                                "unavailable" => McpServerStatus::Unavailable,
                                _ => McpServerStatus::Unknown,
                            }),
                            session
                                .tools
                                .into_iter()
                                .map(|tool| McpToolInfo {
                                    name: tool.name,
                                    display_name: tool.display_name,
                                    description: tool.description,
                                    enabled: tool.enabled,
                                })
                                .collect(),
                            session.auth_required,
                            session.setup_required,
                        )
                    })
                    .unwrap_or((None, None, Vec::new(), false, false));
                McpServerInfo {
                    name: server.name,
                    display_name: server.display_name,
                    source: match server.source.as_str() {
                        "managed" => McpServerSource::Managed,
                        "local" => McpServerSource::Local,
                        _ => McpServerSource::Unknown,
                    },
                    source_label: server.source_label,
                    transport: match server.transport {
                        McpTransportWire::Http {
                            url,
                            scope,
                            scope_id,
                            scope_name,
                        } => McpTransportFacts::Http {
                            url,
                            scope,
                            scope_id,
                            scope_name,
                        },
                        McpTransportWire::Stdio { command, args, env } => {
                            McpTransportFacts::Stdio {
                                command,
                                args,
                                environment_names: env
                                    .into_iter()
                                    .map(|variable| variable.name)
                                    .collect(),
                            }
                        }
                        McpTransportWire::ManagedGateway => McpTransportFacts::ManagedGateway,
                    },
                    enabled,
                    status,
                    tools,
                    auth_required,
                    setup_required,
                }
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_conflict_projects_authoritative_version_and_snapshot() {
        let expected = xai_prompt_queue::QueueVersion {
            generation: "queue-generation".into(),
            revision: 3,
        };
        let actual = xai_prompt_queue::QueueVersion {
            generation: "queue-generation".into(),
            revision: 4,
        };
        let result = queue_mutation_result(
            xai_grok_shell::session::commands::QueueMutationResult::Conflict {
                operation_id: "op-1".into(),
                expected,
                actual: actual.clone(),
                snapshot: xai_prompt_queue::QueueChanged {
                    session_id: "s1".into(),
                    generation: actual.generation,
                    revision: actual.revision,
                    ..Default::default()
                },
            },
        );
        assert!(matches!(
            result,
            QueueMutationResult::Conflict {
                operation_id,
                expected: Version { revision: 3, .. },
                actual: Version { revision: 4, .. },
                snapshot: QueueSnapshot {
                    version: Version { revision: 4, .. },
                    ..
                },
            } if operation_id.as_str() == "op-1"
        ));
    }

    #[test]
    fn scheduler_and_rewind_results_round_trip_without_json() {
        let scheduler_version =
            xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerVersion::parse(
                "018f47a6-7c00-7000-8000-000000000001",
                9,
            )
            .unwrap();
        let task =
            xai_grok_tools::implementations::grok_build::scheduler::types::ScheduledTask::new(
                300,
                "inspect deploy".into(),
                true,
                false,
            );
        let scheduled = scheduler_task_result(
            xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerMutationResult::Committed {
                operation_id: "schedule-op".into(),
                value: task,
                version: scheduler_version,
                replayed: false,
            },
        );
        assert!(matches!(
            scheduled,
            SchedulerMutationResult::Committed {
                operation_id,
                value: ScheduledTask { interval_secs: 300, .. },
                version: Version { revision: 9, .. },
                replayed: false,
            } if operation_id.as_str() == "schedule-op"
        ));

        let rewind =
            rewind_execution_result(xai_grok_shell::session::RewindExecutionResult::Committed {
                version: xai_grok_shell::session::RewindVersion {
                    generation: "rewind-generation".into(),
                    revision: 5,
                },
                response: xai_grok_shell::session::RewindResponse {
                    success: true,
                    target_prompt_index: 2,
                    mode: xai_grok_shell::session::RewindMode::ConversationOnly,
                    reverted_files: Vec::new(),
                    clean_files: vec!["src/lib.rs".into()],
                    conflicts: Vec::new(),
                    prompt_text: Some("retry this".into()),
                    error: None,
                },
                used_compaction_replay: true,
            });
        assert!(matches!(
            rewind,
            RewindExecutionResult::Committed {
                version: Version { revision: 5, .. },
                result: RewindResult {
                    target_prompt_index: 2,
                    mode: RewindMode::ConversationOnly,
                    used_compaction_replay: true,
                    ..
                },
            }
        ));
    }

    #[test]
    fn typed_extension_parsers_drop_mcp_environment_values() {
        let inventory = mcp_inventory_snapshot(serde_json::json!({
            "result": {
                "servers": [{
                    "name": "local-tools",
                    "displayName": "Local tools",
                    "source": "local",
                    "type": "stdio",
                    "command": "/usr/bin/tool-server",
                    "args": ["serve"],
                    "env": [{"name": "SERVICE_TOKEN", "value": "secret-token-value"}],
                    "session": {
                        "enabled": true,
                        "status": "ready",
                        "tools": [{"name": "lookup", "enabled": true}],
                        "authRequired": false,
                        "setupRequired": false
                    }
                }]
            },
            "error": null
        }))
        .unwrap();
        assert_eq!(inventory.servers.len(), 1);
        assert!(matches!(
            &inventory.servers[0].transport,
            McpTransportFacts::Stdio {
                environment_names,
                ..
            } if environment_names == &["SERVICE_TOKEN"]
        ));
        assert!(!format!("{inventory:?}").contains("secret-token-value"));

        let usage = session_usage(serde_json::json!({
            "usage": {
                "inputTokens": 10,
                "outputTokens": 4,
                "totalTokens": 14,
                "modelCalls": 1,
                "modelUsage": {
                    "model-a": {"inputTokens": 10, "outputTokens": 4, "totalTokens": 14}
                },
                "numTurns": 2,
                "usageIsIncomplete": false
            }
        }))
        .unwrap();
        assert_eq!(usage.totals.total_tokens, 14);
        assert_eq!(usage.by_model["model-a"].output_tokens, 4);
        assert_eq!(usage.turns, 2);
    }
}
