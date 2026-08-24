//! `AskUserQuestion` tool — new architecture (`Tool` trait).
//!
//! Interactive Q&A tool that presents the user with structured questions and
//! option sets. In plan mode it serves as the **interview mechanism** — the
//! agent clarifies requirements, disambiguates approaches, and gets user input
//! on design decisions before finalizing the plan. Outside plan mode it is a
//! general-purpose tool for gathering user preferences during implementation.
//!
//! ## How It Works
//!
//! 1. The agent calls `AskUserQuestion` with an array of structured questions
//!    (each with options, optional preview, optional multi_select).
//! 2. The tool sends a `UserQuestionAsked` **notification** to the gateway/client
//!    carrying the full question payload as JSON.
//! 3. The tool returns `AskUserQuestionOutput::QuestionsSent` to the model as
//!    an immediate confirmation.
//! 4. The client presents the question UI. The session coordinator injects an
//!    accepted answer as a synthetic user interjection at the next model-step
//!    boundary, or closes the ask unanswered when its Turn settles/withdraws.
//!
//! ## Plan-Mode Interview Actions
//!
//! When called during plan mode, the client can present two extra buttons:
//! - **"Respond to agent"** — partial answers, agent reformulates questions
//! - **"Finish plan interview"** — agent stops asking, proceeds with what it has
//!
//! These are client-side behaviors that produce different answer
//! interjections; the tool itself is identical in and out of plan mode.

pub mod format;
pub mod types;

pub use types::{
    AskUserQuestionExtRequest, AskUserQuestionExtResponse, AskUserQuestionMode, QuestionAnnotation,
    UserQuestionCommand, UserQuestionRequest, UserQuestionResponse, UserQuestionSender,
};

use crate::notification::types::UserQuestionAsked;
use crate::types::output::AskUserQuestionOutput;
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::NotificationHandle;
use crate::types::tool::{ToolKind, ToolNamespace};

/// A single option within a question.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct QuestionOption {
    /// Option text shown to the user; a few words at most.
    #[schemars(description = "Option text shown to the user. A few words at most.")]
    pub label: String,

    /// What picking this option means or implies.
    #[schemars(description = "What picking this option means or implies.")]
    pub description: String,

    /// Optional content shown while the option is focused — mockups, code
    /// snippets, anything the user should compare. Single-select only.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Optional content shown while the option is focused — mockups, code snippets, anything the user should compare. Single-select questions only."
    )]
    pub preview: Option<String>,

    /// Opaque id; hidden from the model. Grok callers leave it `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub id: Option<String>,
}

/// A single question with its options.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    /// The question to ask, phrased as a full question.
    #[schemars(description = "The question to ask, phrased as a full question.")]
    pub question: String,

    /// The choices for this question.
    #[schemars(description = "The choices for this question.")]
    pub options: Vec<QuestionOption>,

    /// Let the user pick more than one option (default false).
    // Model-facing schema name is snake_case (`multi_select`); deserialize also
    // accepts ACP `multiSelect` so this shared type serves the model schema and
    // the camelCase ACP ext_method without a second question aggregate.
    #[serde(
        default,
        alias = "multi_select",
        deserialize_with = "crate::types::schema::deserialize_lenient_option_bool"
    )]
    #[schemars(
        rename = "multi_select",
        description = "Let the user pick more than one option (default false)."
    )]
    pub multi_select: Option<bool>,

    /// See `QuestionOption.id`. Hidden from the JSON schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub id: Option<String>,
}

/// Input for the `AskUserQuestion` tool.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct AskUserQuestionInput {
    /// The questions to ask, each with its own options. Required unless
    /// withdrawing an earlier request.
    #[schemars(
        description = "Questions to open. Provide at least one unless withdrawing an earlier request."
    )]
    #[serde(default)]
    pub questions: Vec<Question>,

    /// Withdraw one still-consumable ask opened by an earlier call. The id is
    /// returned as `request_id` when questions are opened. Exactly one of
    /// `questions` or `withdraw` must be provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Request id of a still-consumable ask to withdraw. Use instead of questions."
    )]
    pub withdraw: Option<String>,

    /// Internal flag: when `true`, the answer interjection uses the alternate
    /// shape (referenced by id, not label).
    /// Skipped on the wire and from the JSON schema so the model never
    /// sees or controls this field.
    #[serde(default, skip)]
    #[schemars(skip)]
    pub use_id_keyed_format: bool,
}

/// `AskUserQuestion` tool.
///
/// Sends a request over an in-process mpsc channel to the session-owned
/// coordinator and returns immediately. The coordinator keeps the form open
/// while the Turn is active and injects an accepted answer at a model-step
/// boundary; an answer that misses that boundary is reported as unanswered.
#[derive(Debug, Default)]
pub struct AskUserQuestionTool;

impl crate::types::tool_metadata::ToolMetadata for AskUserQuestionTool {
    fn kind(&self) -> ToolKind {
        ToolKind::AskUser
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn emitted_notifications(&self) -> &'static [&'static str] {
        &["UserQuestionAsked"]
    }

    fn description_template(&self) -> &str {
        r#"Ask the user one or more multiple-choice questions.

- Every question automatically gets an "Other" choice where the user can type their own answer.
- Put your recommended option first and append "(Recommended)" to its label."#
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        // Standalone. The plan-mode prompt note is
        // `${% if tools.by_kind.exit_plan %}`-guarded, so it renders
        // fine without the plan tools.
        Expr::True
    }
}

impl xai_tool_runtime::Tool for AskUserQuestionTool {
    type Args = AskUserQuestionInput;
    type Output = AskUserQuestionOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("ask_user_question").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "ask_user_question",
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(
        name = "tool.ask_user_question",
        skip_all,
        fields(question_count = input.questions.len()),
    )]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: AskUserQuestionInput,
    ) -> Result<AskUserQuestionOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        let question_count = input.questions.len();

        if let Some(request_id) = input.withdraw {
            if question_count != 0 || request_id.trim().is_empty() {
                return Err(xai_tool_runtime::ToolError::invalid_arguments(
                    "withdraw must be a non-empty request id and cannot be combined with questions",
                ));
            }
            let sender = {
                let resources = resources.lock().await;
                resources.get::<UserQuestionSender>().cloned()
            }
            .ok_or_else(|| {
                xai_tool_runtime::ToolError::custom(
                    "missing_resource",
                    "UserQuestionSender".to_string(),
                )
            })?;
            let (reply, response) = tokio::sync::oneshot::channel();
            sender
                .0
                .send(UserQuestionCommand::Withdraw {
                    tool_call_id: request_id.clone(),
                    reply,
                })
                .map_err(|_| {
                    xai_tool_runtime::ToolError::execution(
                        xai_tool_protocol::ToolId::new("ask_user_question").expect("valid"),
                        "User question session ended unexpectedly (coordinator channel closed)",
                    )
                })?;
            let withdrawn = response.await.map_err(|_| {
                xai_tool_runtime::ToolError::execution(
                    xai_tool_protocol::ToolId::new("ask_user_question").expect("valid"),
                    "User question coordinator dropped the withdrawal receipt",
                )
            })?;
            return Ok(AskUserQuestionOutput::QuestionWithdrawn {
                message: if withdrawn {
                    format!("Question request {request_id} was withdrawn unanswered.")
                } else {
                    format!("Question request {request_id} was no longer consumable.")
                },
                request_id,
                withdrawn,
            });
        }

        if question_count == 0 {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "provide at least one question or a request id to withdraw",
            ));
        }

        // ── Step 1: Validate unique question text ───────────────────────
        {
            let mut seen = std::collections::HashSet::new();
            for q in &input.questions {
                if !seen.insert(&q.question) {
                    return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                        "Duplicate question text: \"{}\"",
                        q.question
                    )));
                }
            }
        }

        // ── Step 2: Obtain UserQuestionSender ───────────────────────────
        let (sender, owning_prompt_id) = {
            let res = resources.lock().await;
            (
                res.get::<UserQuestionSender>().cloned(),
                res.get::<
                    crate::implementations::grok_build::task::types::CurrentPromptIdResource,
                >()
                .map(|prompt| prompt.0.clone())
                .filter(|prompt| !prompt.is_empty()),
            )
        };

        let sender = sender.ok_or_else(|| {
            xai_tool_runtime::ToolError::custom(
                "missing_resource",
                "UserQuestionSender".to_string(),
            )
        })?;
        let owning_prompt_id = owning_prompt_id.ok_or_else(|| {
            xai_tool_runtime::ToolError::custom(
                "missing_resource",
                "CurrentPromptIdResource".to_string(),
            )
        })?;

        // ── Step 3: Open the elicitation ────────────────────────────────
        let request = types::UserQuestionRequest {
            tool_call_id: ctx.call_id.as_str().to_owned(),
            owning_prompt_id,
            questions: input.questions.clone(),
            use_id_keyed_format: input.use_id_keyed_format,
        };

        if sender.0.send(UserQuestionCommand::Open(request)).is_err() {
            return Err(xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("ask_user_question").expect("valid"),
                "User question session ended unexpectedly (coordinator channel closed)",
            ));
        }

        // ── Step 4: Emit UserQuestionAsked ──────────────────────────────
        {
            let questions_json = serde_json::to_value(&input.questions)
                .unwrap_or_else(|_| serde_json::Value::Array(vec![]));
            let res = resources.lock().await;
            if let Some(handle) = res.get::<NotificationHandle>() {
                handle.0.send_user_question_asked(UserQuestionAsked {
                    tool_call_id: ctx.call_id.as_str().to_owned(),
                    questions_json,
                });
            }
        }
        tracing::info!(
            question_count,
            "Opened user questions without pausing the Turn"
        );

        let question_summary = input
            .questions
            .iter()
            .map(|question| format!("- {}", question.question))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(AskUserQuestionOutput::QuestionsSent {
            message: format!(
                "The questions remain answerable while this Turn continues. Keep working on anything that does not depend on the answer:\n{question_summary}"
            ),
            question_count,
            request_id: ctx.call_id.as_str().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::resources::{Resources, SharedResources};
    use crate::types::tool_metadata::test_ctx_with_call_id;
    use tokio::sync::mpsc;

    fn make_question(question: &str, labels: &[&str]) -> Question {
        Question {
            question: question.to_string(),
            options: labels
                .iter()
                .map(|l| QuestionOption {
                    label: l.to_string(),
                    description: format!("Description for {l}"),
                    preview: None,
                    id: None,
                })
                .collect(),
            multi_select: None,
            id: None,
        }
    }

    /// Create resources with a UserQuestionSender injected.
    /// Returns (shared_resources, rx) where rx receives UserQuestionRequests.
    fn resources_with_sender() -> (
        SharedResources,
        mpsc::UnboundedReceiver<types::UserQuestionCommand>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut resources = Resources::new();
        resources.insert(UserQuestionSender(tx));
        resources.insert(
            crate::implementations::grok_build::task::types::CurrentPromptIdResource(
                "turn-1".into(),
            ),
        );
        (resources.into_shared(), rx)
    }

    // ── Basic tool metadata tests ────────────────────────────────────────

    #[test]
    fn tool_name_and_description() {
        let tool = AskUserQuestionTool;
        assert_eq!(
            xai_tool_runtime::Tool::id(&tool).as_str(),
            "ask_user_question"
        );
        let desc = crate::types::tool_metadata::ToolMetadata::description_template(&tool);
        assert!(desc.contains("Ask the user"));
        assert!(desc.contains("Other"));
        assert!(desc.contains("(Recommended)"));
    }

    #[test]
    fn tool_is_read_only() {
        assert!(xai_tool_runtime::Tool::capabilities(&AskUserQuestionTool).is_read_only);
    }

    #[test]
    fn tool_kind_is_ask_user() {
        assert_eq!(
            crate::types::tool_metadata::ToolMetadata::kind(&AskUserQuestionTool),
            ToolKind::AskUser
        );
    }

    #[test]
    fn input_deserializes_from_json() {
        let json = serde_json::json!({
            "questions": [{
                "question": "Pick DB?",
                "options": [
                    {"label": "Postgres", "description": "Relational DB"},
                    {"label": "SQLite", "description": "Embedded SQL database", "preview": "```\nSELECT 1;\n```"}
                ],
                "multiSelect": false
            }]
        });

        let input: AskUserQuestionInput = serde_json::from_value(json).unwrap();
        assert_eq!(input.questions.len(), 1);
        assert_eq!(input.questions[0].question, "Pick DB?");
        assert_eq!(input.questions[0].options.len(), 2);
        assert_eq!(input.questions[0].options[0].label, "Postgres");
        assert!(input.questions[0].options[0].preview.is_none());
        assert_eq!(input.questions[0].options[1].label, "SQLite");
        assert!(input.questions[0].options[1].preview.is_some());
        assert_eq!(input.questions[0].multi_select, Some(false));
    }

    #[test]
    fn model_schema_advertises_snake_case_multi_select() {
        let schema = schemars::schema_for!(AskUserQuestionInput);
        let json = serde_json::to_string(&schema).unwrap();
        assert!(
            json.contains("multi_select"),
            "model schema should advertise multi_select: {json}"
        );
        assert!(
            !json.contains("multiSelect"),
            "model schema should not advertise camelCase multiSelect: {json}"
        );
    }

    #[test]
    fn input_accepts_snake_case_multi_select() {
        let json = serde_json::json!({
            "questions": [{
                "question": "Pick DB?",
                "options": [{"label": "Postgres", "description": "Relational DB"}],
                "multi_select": true
            }]
        });
        let input: AskUserQuestionInput = serde_json::from_value(json).unwrap();
        assert_eq!(input.questions[0].multi_select, Some(true));
    }

    #[tokio::test]
    async fn opens_elicitation_and_returns_without_waiting_for_an_answer() {
        let (shared, mut rx) = resources_with_sender();
        let input = AskUserQuestionInput {
            questions: vec![make_question("Which database?", &["Redis", "Postgres"])],
            withdraw: None,
            use_id_keyed_format: false,
        };

        let output = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            xai_tool_runtime::Tool::run(
                &AskUserQuestionTool,
                test_ctx_with_call_id(shared, "ask-1"),
                input,
            ),
        )
        .await
        .expect("the tool must not wait for the user")
        .expect("the request should open");

        assert!(matches!(
            output,
            AskUserQuestionOutput::QuestionsSent {
                question_count: 1,
                ..
            }
        ));
        let UserQuestionCommand::Open(request) =
            rx.try_recv().expect("the coordinator receives the form")
        else {
            panic!("expected an open command");
        };
        assert_eq!(request.tool_call_id, "ask-1");
        assert_eq!(request.owning_prompt_id, "turn-1");
        assert_eq!(request.questions.len(), 1);
    }

    #[tokio::test]
    async fn missing_coordinator_fails_closed() {
        let shared = Resources::new().into_shared();
        let input = AskUserQuestionInput {
            questions: vec![make_question("Proceed?", &["Yes", "No"])],
            withdraw: None,
            use_id_keyed_format: false,
        };

        let error = xai_tool_runtime::Tool::run(
            &AskUserQuestionTool,
            test_ctx_with_call_id(shared, "ask-2"),
            input,
        )
        .await
        .expect_err("an unavailable coordinator cannot truthfully open a form");

        assert!(error.to_string().contains("UserQuestionSender"));
    }

    #[tokio::test]
    async fn open_without_an_owning_turn_fails_closed() {
        let (sender, _commands) = mpsc::unbounded_channel();
        let mut resources = Resources::new();
        resources.insert(UserQuestionSender(sender));

        let error = xai_tool_runtime::Tool::run(
            &AskUserQuestionTool,
            test_ctx_with_call_id(resources.into_shared(), "ask-no-turn"),
            AskUserQuestionInput {
                questions: vec![make_question("Proceed?", &["Yes", "No"])],
                withdraw: None,
                use_id_keyed_format: false,
            },
        )
        .await
        .expect_err("an ask cannot outlive or attach to a different Turn");

        assert!(error.to_string().contains("CurrentPromptIdResource"));
    }

    #[tokio::test]
    async fn opening_emits_the_question_notification() {
        use crate::notification::types::{ToolNotification, ToolNotificationHandle};

        let (sender, _request_rx) = mpsc::unbounded_channel();
        let (notifications, mut notification_rx) = ToolNotificationHandle::channel();
        let mut resources = Resources::new();
        resources.insert(UserQuestionSender(sender));
        resources.insert(NotificationHandle(notifications));
        resources.insert(
            crate::implementations::grok_build::task::types::CurrentPromptIdResource(
                "turn-1".into(),
            ),
        );

        xai_tool_runtime::Tool::run(
            &AskUserQuestionTool,
            test_ctx_with_call_id(resources.into_shared(), "ask-3"),
            AskUserQuestionInput {
                questions: vec![make_question("Pick one?", &["A", "B"])],
                withdraw: None,
                use_id_keyed_format: false,
            },
        )
        .await
        .expect("the request should open");

        assert!(matches!(
            notification_rx.try_recv(),
            Ok(ToolNotification::UserQuestionAsked(asked)) if asked.tool_call_id == "ask-3"
        ));
    }

    #[tokio::test]
    async fn withdrawal_returns_the_coordinators_truthful_receipt() {
        let (shared, mut commands) = resources_with_sender();
        tokio::spawn(async move {
            let UserQuestionCommand::Withdraw {
                tool_call_id,
                reply,
            } = commands.recv().await.unwrap()
            else {
                panic!("expected withdrawal");
            };
            assert_eq!(tool_call_id, "ask-original");
            reply.send(true).unwrap();
        });

        let output = xai_tool_runtime::Tool::run(
            &AskUserQuestionTool,
            test_ctx_with_call_id(shared, "withdraw-call"),
            AskUserQuestionInput {
                questions: Vec::new(),
                withdraw: Some("ask-original".into()),
                use_id_keyed_format: false,
            },
        )
        .await
        .expect("withdrawal command succeeds");

        assert!(matches!(
            output,
            AskUserQuestionOutput::QuestionWithdrawn {
                request_id,
                withdrawn: true,
                ..
            } if request_id == "ask-original"
        ));
    }
}
