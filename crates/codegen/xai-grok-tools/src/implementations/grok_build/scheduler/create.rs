use crate::types::requirements::{Expr, ToolRequirement};

use crate::types::tool::{ToolKind, ToolNamespace};

use super::interval::parse_interval;
use super::types::{
    ScheduledTask, SchedulerCommand, SchedulerHandle, SchedulerWakeSource, scheduler_tool_error,
};

// Canonical /loop wording lives in the light API crate so other consumers can
// link it without the tools implementation crate; re-exported to keep paths stable.
pub use xai_grok_tools_api::slash_commands::{
    LoopFireMode, SCHEDULER_CREATE_TOOL_NAME, loop_schedule_instruction, loop_usage_message,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SchedulerCreateInput {
    #[serde(default)]
    #[schemars(
        description = "Id of an existing task to update in place: provided fields replace old \
                       values, omitted ones are unchanged, the schedule keeps its phase, and an \
                       unknown id errors. Omit to create a task."
    )]
    pub task_id: Option<String>,

    #[serde(default)]
    #[schemars(description = "The prompt text to execute on each scheduled fire. \
                       Required to create; optional with task_id")]
    pub prompt: Option<String>,

    #[serde(default)]
    #[schemars(
        description = "Exactly one source that wakes the task. Required to create; \
                       optional with task_id"
    )]
    pub wake_source: Option<SchedulerWakeSourceInput>,

    /// Whether the task persists across sessions. Default false (session-only).
    #[serde(
        default,
        deserialize_with = "crate::types::schema::deserialize_lenient_option_bool"
    )]
    #[schemars(
        description = "Whether the task persists across sessions. Default: false. \
                       Create-only: ignored with task_id"
    )]
    pub durable: Option<bool>,

    #[serde(
        default,
        deserialize_with = "crate::types::schema::deserialize_lenient_option_bool"
    )]
    #[schemars(
        description = "Run each fire as a main-conversation turn instead of a background \
                       subagent; set true only when runs need the conversation's context. \
                       Default: false. Create-only: ignored with task_id"
    )]
    pub foreground: Option<bool>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SchedulerWakeSourceInput {
    Recurrence {
        #[schemars(description = "Cadence such as 5m, 2h, or 1d")]
        interval: String,
        #[serde(default = "default_true")]
        recurring: bool,
        #[serde(
            default,
            deserialize_with = "crate::types::schema::deserialize_lenient_bool"
        )]
        fire_immediately: bool,
    },
    ExternalEvent {
        service: String,
        event: String,
        #[serde(default = "default_true")]
        recurring: bool,
    },
    ProcessSettlement {
        process_id: String,
        command: String,
    },
}

fn default_true() -> bool {
    true
}

impl SchedulerWakeSourceInput {
    fn parse(self) -> Result<(SchedulerWakeSource, bool), xai_tool_runtime::ToolError> {
        let required = |label: &str, value: String| {
            let value = value.trim().to_owned();
            if value.is_empty() {
                Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                    "{label} cannot be empty"
                )))
            } else {
                Ok(value)
            }
        };
        match self {
            Self::Recurrence {
                interval,
                recurring,
                fire_immediately,
            } => Ok((
                SchedulerWakeSource::Recurrence {
                    interval_secs: parse_interval(&interval).map_err(|error| {
                        xai_tool_runtime::ToolError::invalid_arguments(error.to_string())
                    })?,
                    recurring,
                },
                fire_immediately,
            )),
            Self::ExternalEvent {
                service,
                event,
                recurring,
            } => Ok((
                SchedulerWakeSource::ExternalEvent {
                    service: required("service", service)?,
                    event: required("event", event)?,
                    recurring,
                },
                false,
            )),
            Self::ProcessSettlement {
                process_id,
                command,
            } => Ok((
                SchedulerWakeSource::ProcessSettlement {
                    process_id: required("process_id", process_id)?,
                    command: required("command", command)?,
                },
                false,
            )),
        }
    }
}

/// Execute the scheduler control-plane operation after its shared resource has
/// been resolved. Kept here so model tool calls and headless callers have
/// exactly the same validation and actor-command semantics.
pub(crate) async fn upsert_with_sender(
    sender: tokio::sync::mpsc::UnboundedSender<SchedulerCommand>,
    input: SchedulerCreateInput,
) -> Result<(ScheduledTask, bool), xai_tool_runtime::ToolError> {
    let wake_source = input
        .wake_source
        .map(SchedulerWakeSourceInput::parse)
        .transpose()?;
    let send_and_wait = |cmd: SchedulerCommand,
                         rx: tokio::sync::oneshot::Receiver<
        Result<ScheduledTask, super::types::SchedulerError>,
    >| async {
        sender.send(cmd).map_err(|_| {
            xai_tool_runtime::ToolError::custom("process_manager", "Scheduler actor stopped")
        })?;
        rx.await
            .map_err(|_| {
                xai_tool_runtime::ToolError::custom(
                    "process_manager",
                    "Scheduler actor dropped reply",
                )
            })?
            .map_err(scheduler_tool_error)
    };
    if let Some(task_id) = input.task_id {
        if input.prompt.is_none() && wake_source.is_none() {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "nothing to update: provide wake_source and/or prompt alongside task_id",
            ));
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = send_and_wait(
            SchedulerCommand::Update {
                id: task_id,
                prompt: input.prompt,
                wake_source: wake_source.map(|(source, _)| source),
                reply: tx,
            },
            rx,
        )
        .await?;
        return Ok((task, true));
    }
    let (wake_source, fire_immediately) = wake_source.ok_or_else(|| {
        xai_tool_runtime::ToolError::invalid_arguments(
            "wake_source is required when creating a task",
        )
    })?;
    let prompt = input.prompt.ok_or_else(|| {
        xai_tool_runtime::ToolError::invalid_arguments("prompt is required when creating a task")
    })?;
    let mut task =
        ScheduledTask::from_wake_source(prompt, wake_source, input.durable.unwrap_or(false));
    if fire_immediately {
        let interval_secs = task
            .interval_secs()
            .expect("only recurrence accepts fire_immediately");
        task.created_at -= chrono::Duration::seconds(interval_secs as i64);
    }
    task.foreground = input.foreground.unwrap_or(false);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = send_and_wait(SchedulerCommand::Create { task, reply: tx }, rx).await?;
    Ok((task, false))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SchedulerCreateOutput {
    pub id: String,
    pub human_schedule: String,
    #[serde(default)]
    pub updated: bool,
}

impl xai_tool_runtime::ToolOutput for SchedulerCreateOutput {}

#[derive(Debug, Default)]
pub struct SchedulerCreateTool;

impl crate::types::tool_metadata::ToolMetadata for SchedulerCreateTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        // Formatted once so the TTL copy is derived from RECURRING_TASK_TTL_DAYS instead of
        // being pinned by a duplicate literal.
        static DESCRIPTION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
            format!(
                r#"Create a scheduled task with one wake source, or update an existing one in place.

The source is a recurrence, a mounted Service event, or a detached process settlement. A recurrence may be one-time or recurring and may fire immediately. Service and process occurrences are delivered by the Host.

To change an existing task, pass its task_id: provided fields replace old values and omitted ones are unchanged. An unknown id errors.

Usage notes:
- Interval format: "5m" (minutes), "2h" (hours), "1d" (days), "60s" (seconds, min 60)
- Maximum 50 scheduled tasks at once
- Recurring tasks auto-expire after {} days"#,
                super::types::RECURRING_TASK_TTL_DAYS
            )
        });
        &DESCRIPTION
        // TODO: scheduler tools share ToolKind::Other so they can't be template-ized
        // via ${{ tools.by_kind.* }}. If tool name randomization is needed, add
        // dedicated ToolKind variants (SchedulerCreate, SchedulerDelete, SchedulerList).
    }

    fn emitted_notifications(&self) -> &'static [&'static str] {
        &["ScheduledTaskCreated"]
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for SchedulerCreateTool {
    type Args = SchedulerCreateInput;
    type Output = SchedulerCreateOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(SCHEDULER_CREATE_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "scheduler_create",
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(
        name = "tool.scheduler_create",
        skip_all,
        fields(task_id = input.task_id.as_deref().unwrap_or(""))
    )]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: SchedulerCreateInput,
    ) -> Result<SchedulerCreateOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        let sender = {
            let res = resources.lock().await;
            res.get::<SchedulerHandle>()
                .ok_or_else(|| {
                    xai_tool_runtime::ToolError::custom("missing_resource", "SchedulerHandle")
                })?
                .0
                .clone()
        };

        let (created, updated) = upsert_with_sender(sender, input).await?;
        Ok(SchedulerCreateOutput {
            human_schedule: created.human_schedule(),
            id: created.id,
            updated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::implementations::grok_build::scheduler::actor::SchedulerActor;
    use crate::notification::types::ToolNotificationHandle;
    use crate::types::resources::{Resources, SharedResources, State};
    use crate::types::tool_metadata::test_ctx;
    use xai_tool_runtime::Tool;

    fn scheduler_resources() -> (SharedResources, tokio_util::sync::CancellationToken) {
        let mut resources = Resources::new();
        resources.register_state::<super::super::types::SchedulerState>();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        resources.insert(SchedulerHandle(cmd_tx));
        let shared = resources.into_shared();

        let (notif_handle, _notif_rx) = ToolNotificationHandle::channel();
        let cancel_token = tokio_util::sync::CancellationToken::new();
        let actor = SchedulerActor {
            resources: shared.clone(),
            resources_persistence: std::sync::Arc::new(
                crate::persistence::ResourcesPersistence::noop(),
            ),
            notification_handle: notif_handle,
            cmd_rx,
            cancel_token: cancel_token.clone(),
            clock: Default::default(),
            pending_removal: None,
            blocked_expiries: Default::default(),
        };
        tokio::spawn(actor.run());
        (shared, cancel_token)
    }

    fn input(json: serde_json::Value) -> SchedulerCreateInput {
        serde_json::from_value(json).expect("valid input json")
    }

    async fn task_count(resources: &SharedResources) -> usize {
        let res = resources.lock().await;
        res.get::<State<super::super::types::SchedulerState>>()
            .map(|s| s.tasks.len())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn create_requires_wake_source_and_prompt() {
        let (resources, cancel) = scheduler_resources();

        let err = SchedulerCreateTool
            .run(test_ctx(resources.clone()), input(serde_json::json!({})))
            .await
            .expect_err("create without wake source must fail");
        assert!(err.to_string().contains("wake_source is required"));

        let err = SchedulerCreateTool
            .run(
                test_ctx(resources.clone()),
                input(serde_json::json!({
                    "wake_source": {
                        "kind": "recurrence",
                        "interval": "5m",
                        "recurring": true,
                        "fireImmediately": false
                    }
                })),
            )
            .await
            .expect_err("create without prompt must fail");
        assert!(err.to_string().contains("prompt is required"));

        assert_eq!(task_count(&resources).await, 0);
        cancel.cancel();
    }

    #[tokio::test]
    async fn one_shot_recurrence_is_a_supported_wake_source() {
        let (resources, cancel) = scheduler_resources();

        let created = SchedulerCreateTool
            .run(
                test_ctx(resources.clone()),
                input(serde_json::json!({
                    "wake_source": {
                        "kind": "recurrence",
                        "interval": "5m",
                        "recurring": false,
                        "fireImmediately": false
                    },
                    "prompt": "check"
                })),
            )
            .await
            .expect("one-shot recurrence is supported");
        assert!(!created.updated);
        assert_eq!(task_count(&resources).await, 1);
        cancel.cancel();
    }

    #[tokio::test]
    async fn update_unknown_task_id_errors_and_never_creates() {
        let (resources, cancel) = scheduler_resources();

        let err = SchedulerCreateTool
            .run(
                test_ctx(resources.clone()),
                input(serde_json::json!({
                    "task_id": "nonexistent", "prompt": "new prompt"
                })),
            )
            .await
            .expect_err("unknown id must error");
        assert!(err.to_string().contains("no scheduled task with id"));
        assert_eq!(
            task_count(&resources).await,
            0,
            "strict update must not fall back to create"
        );
        cancel.cancel();
    }

    #[tokio::test]
    async fn update_replaces_the_typed_wake_source() {
        let (resources, cancel) = scheduler_resources();

        let created = SchedulerCreateTool
            .run(
                test_ctx(resources.clone()),
                input(serde_json::json!({
                    "wake_source": {
                        "kind": "recurrence", "interval": "5m",
                        "recurring": true, "fireImmediately": false
                    },
                    "prompt": "check deploy"
                })),
            )
            .await
            .expect("create succeeds");

        let updated = SchedulerCreateTool
            .run(
                test_ctx(resources.clone()),
                input(serde_json::json!({
                    "task_id": created.id,
                    "wake_source": {
                        "kind": "externalEvent",
                        "service": "github",
                        "event": "pull_request.updated",
                        "recurring": true
                    }
                })),
            )
            .await
            .expect("typed wake source update succeeds");
        assert!(updated.updated);
        assert_eq!(updated.human_schedule, "github: pull_request.updated");
        cancel.cancel();
    }

    #[tokio::test]
    async fn update_with_no_patch_fields_errors() {
        let (resources, cancel) = scheduler_resources();

        let err = SchedulerCreateTool
            .run(
                test_ctx(resources.clone()),
                input(serde_json::json!({"task_id": "abc123"})),
            )
            .await
            .expect_err("empty patch must error");
        assert!(err.to_string().contains("nothing to update"));
        cancel.cancel();
    }

    #[tokio::test]
    async fn create_then_update_patches_in_place() {
        let (resources, cancel) = scheduler_resources();

        let created = SchedulerCreateTool
            .run(
                test_ctx(resources.clone()),
                input(serde_json::json!({
                    "wake_source": {
                        "kind": "recurrence", "interval": "5m",
                        "recurring": true, "fireImmediately": false
                    },
                    "prompt": "check deploy"
                })),
            )
            .await
            .expect("create succeeds");
        assert!(!created.updated);
        assert_eq!(created.human_schedule, "every 5 minutes");

        let updated = SchedulerCreateTool
            .run(
                test_ctx(resources.clone()),
                input(serde_json::json!({
                    "task_id": created.id,
                    "wake_source": {
                        "kind": "recurrence", "interval": "10m",
                        "recurring": true, "fireImmediately": false
                    }
                })),
            )
            .await
            .expect("update succeeds");
        assert!(updated.updated);
        assert_eq!(updated.id, created.id, "identity preserved");
        assert_eq!(updated.human_schedule, "every 10 minutes");
        assert_eq!(task_count(&resources).await, 1, "no second task");
        cancel.cancel();
    }

    #[test]
    fn schema_advertises_typed_wake_sources_and_task_id() {
        let schema = schemars::schema_for!(SchedulerCreateInput);
        let json = serde_json::to_string(&schema).unwrap();
        assert!(json.contains("task_id"));
        assert!(json.contains("wake_source"));
        assert!(json.contains("externalEvent"));
        assert!(json.contains("processSettlement"));
    }

    #[test]
    fn loop_usage_message_has_no_host_default() {
        let usage = loop_usage_message();
        assert!(usage.contains("Usage: /loop"));
        assert!(
            !usage.contains("10m"),
            "usage must not claim a default: {usage}"
        );
    }

    #[test]
    fn loop_schedule_instruction_holds_invariants() {
        let args = "every 30 minutes do x";
        let instr = loop_schedule_instruction(args, LoopFireMode::Detached);
        assert!(
            !instr.contains("10m"),
            "instruction must not default: {instr}"
        );
        assert!(instr.contains("Deriving the interval"));
        assert!(instr.contains("<number><unit>"));
        assert!(instr.contains("ask the user how often"));
        assert!(instr.contains("Do NOT execute the prompt inline"));
        // Raw request forwarded verbatim for the model to parse.
        assert!(instr.contains(args));
    }
}
