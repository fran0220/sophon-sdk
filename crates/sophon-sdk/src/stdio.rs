//! Transport adapter for the existing native Agent. Requests may overlap;
//! only Grok Build owns prompt admission, scheduling, and durable execution.
use std::{collections::HashMap, num::NonZeroU64, sync::Arc, time::Duration};

use base64::Engine;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, mpsc, oneshot},
};

use crate::protocol as p;
use crate::{
    Agent, AgentConfig, ClientHandler, Error, MemoryMode, ModelConfig, PermissionPolicy,
    ProviderConfig, Session, SessionConfig,
};

type Result<T> = std::result::Result<T, Error>;
type PendingCallbacks = Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>>;

struct Callbacks {
    output: mpsc::UnboundedSender<p::ServerFrame>,
    pending: PendingCallbacks,
}

struct CallbackGuard {
    id: String,
    output: mpsc::UnboundedSender<p::ServerFrame>,
    pending: PendingCallbacks,
}

impl Drop for CallbackGuard {
    fn drop(&mut self) {
        if self.pending.lock().unwrap().remove(&self.id).is_some() {
            let _ = self.output.send(p::ServerFrame::CallbackCancelled {
                id: self.id.clone(),
            });
        }
    }
}

impl Callbacks {
    async fn call(
        &self,
        method: String,
        params: Value,
        context: Option<p::CallbackContext>,
    ) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        let _guard = CallbackGuard {
            id: id.clone(),
            output: self.output.clone(),
            pending: self.pending.clone(),
        };
        self.output
            .send(p::ServerFrame::Callback {
                id,
                method,
                params,
                context,
            })
            .map_err(|_| Error::RuntimeStopped)?;
        rx.await.map_err(|_| Error::RuntimeStopped)?
    }
}

#[async_trait::async_trait]
impl ClientHandler for Callbacks {
    async fn extension(&self, method: &str, params: Value) -> Result<Value> {
        self.call(method.to_owned(), params, None).await
    }
}

struct Runtime {
    agent: Agent,
    sessions: Mutex<HashMap<String, Session>>,
    workspaces: Mutex<HashMap<String, String>>,
    handlers: Arc<RuntimeTools>,
    native_specs: Vec<p::ToolSpec>,
    terminal: crate::native_terminal::NativeTerminalService,
}

struct RuntimeTools {
    callbacks: Arc<Callbacks>,
    browser: Option<Arc<sophon_browser::BrowserService>>,
    media: Option<crate::native_media::NativeMediaService>,
}

#[async_trait::async_trait]
impl crate::native_tools::NativeToolHandler for RuntimeTools {
    async fn execute(&self, name: &str, args: Value, context: p::CallbackContext) -> Result<Value> {
        match name {
            "browser" => {
                let browser = self
                    .browser
                    .as_ref()
                    .ok_or_else(|| Error::Operation("browser is not configured".into()))?;
                let mut result = browser.execute(name, args).await.map_err(operation)?;
                if let Some(id) = result.get("artifact_id").and_then(Value::as_str) {
                    let id = uuid::Uuid::parse_str(id).map_err(operation)?.to_string();
                    let artifact = browser
                        .execute_host(json!({"action":"artifact","artifact_id":id}))
                        .await
                        .map_err(operation)?;
                    result["artifact"] =
                        publish_browser_artifact(&context.cwd, &id, artifact).await?;
                    result["reviewRequired"] = Value::Bool(true);
                }
                Ok(result)
            }
            "generate_image" | "generate_speech" | "generate_video" => {
                self.media
                    .as_ref()
                    .ok_or_else(|| Error::Operation("media is not configured".into()))?
                    .execute(name, args, std::path::Path::new(&context.cwd))
                    .await
            }
            _ => {
                self.callbacks
                    .call(format!("tool/{name}"), args, Some(context))
                    .await
            }
        }
    }
}

async fn publish_browser_artifact(cwd: &str, id: &str, artifact: Value) -> Result<Value> {
    use sha2::Digest;
    let mime = artifact
        .get("mime_type")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Operation("browser artifact MIME is missing".into()))?;
    let extension = match mime {
        "image/png" => "png",
        "video/mp4" => "mp4",
        _ => return Err(Error::Operation("unsupported browser artifact MIME".into())),
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(
            artifact
                .get("base64")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Operation("browser artifact bytes are missing".into()))?,
        )
        .map_err(operation)?;
    let revision = format!("{:x}", sha2::Sha256::digest(&bytes));
    let relative = format!(".native-browser/{id}.{extension}");
    let result = json!({"path":relative,"mimeType":mime,"bytes":bytes.len(),"revision":revision});
    let cwd = std::path::PathBuf::from(cwd);
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let root = cwd.canonicalize().map_err(operation)?;
        let directory = root.join(".native-browser");
        std::fs::create_dir_all(&directory).map_err(operation)?;
        let directory = directory.canonicalize().map_err(operation)?;
        if !directory.starts_with(&root) {
            return Err(Error::Operation(
                "browser artifact directory escapes workspace".into(),
            ));
        }
        let destination = root.join(relative);
        let mut staging = tempfile::NamedTempFile::new_in(directory).map_err(operation)?;
        staging.write_all(&bytes).map_err(operation)?;
        staging.as_file().sync_all().map_err(operation)?;
        staging.persist_noclobber(destination).map_err(operation)?;
        Ok(result)
    })
    .await
    .map_err(operation)?
}

impl Runtime {
    async fn session(&self, id: &str) -> Result<Session> {
        self.sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| Error::Operation("session is not attached".into()))
    }

    async fn dispatch(&self, request: p::Request) -> Result<Value> {
        use p::Request::*;
        match request {
            Initialize { .. } => Err(Error::Operation("runtime is already initialized".into())),
            CreateSession { options }
            | LoadSession { id: _, options }
            | ResumeSession { id: _, options } => {
                // Attach is dispatched separately so create can never be a load fallback.
                let _ = options;
                unreachable!("attach dispatched by run_request")
            }
            ListSessions { cwd, cursor } => {
                let page = self
                    .agent
                    .list_sessions(cwd.as_deref().map(std::path::Path::new), cursor.as_deref())
                    .await?;
                Ok(
                    json!({"sessions": page.sessions.into_iter().map(|s| json!({"id":s.id.to_string(),"cwd":s.cwd,"title":s.title,"updatedAt":s.updated_at})).collect::<Vec<_>>(), "nextCursor":page.next_cursor}),
                )
            }
            Prompt { session_id, prompt } => {
                let session = self.session(&session_id).await?;
                if prompt.turn_id.is_empty() {
                    return Err(Error::invalid_config("turnId cannot be empty"));
                }
                let mut metadata: serde_json::Map<_, _> = prompt.metadata.into_iter().collect();
                metadata.insert("promptId".into(), Value::String(prompt.turn_id));
                let result = session
                    .prompt_blocks_with_metadata(
                        prompt.blocks.into_iter().map(prompt_block),
                        metadata,
                    )
                    .await?;
                encode(p::PromptReceipt {
                    stop_reason: stop_reason(result.stop_reason),
                    prompt_id: result.prompt_id,
                    prompt_index: result.prompt_index,
                    raw_response: result.raw_response,
                })
            }
            Cancel {
                session_id,
                turn_id,
            } => {
                let session = self.session(&session_id).await?;
                match turn_id {
                    Some(id) => session.cancel_prompt(id).await?,
                    None => session.cancel().await?,
                }
                Ok(Value::Null)
            }
            History { session_id } => {
                let snapshot = self
                    .session(&session_id)
                    .await?
                    .history_snapshot()
                    .await
                    .map_err(operation)?;
                encode(p::HistorySnapshot {
                    session_id: snapshot.session_id.to_string(),
                    revision: snapshot.revision,
                    boundary_id: snapshot.boundary_id,
                    records: snapshot.records.into_iter().map(history_record).collect(),
                })
            }
            Queue { session_id } => {
                let queue = self
                    .session(&session_id)
                    .await?
                    .queue_snapshot()
                    .await
                    .map_err(operation)?;
                encode(queue)
            }
            Dispose { session_id } => {
                self.session(&session_id).await?.close().await?;
                self.sessions.lock().await.remove(&session_id);
                Ok(Value::Null)
            }
            SetModel {
                session_id,
                model,
                metadata,
            } => {
                self.session(&session_id)
                    .await?
                    .set_model_with_metadata(model, metadata.into_iter().collect())
                    .await?;
                Ok(Value::Null)
            }
            Extension {
                session_id,
                name,
                params,
            } => match session_id {
                Some(id) => self.session(&id).await?.extension(name, params).await,
                None => self.agent.extension(name, params).await,
            },
            Browser { args } => self
                .handlers
                .browser
                .as_ref()
                .ok_or_else(|| Error::Operation("browser is not configured".into()))?
                .execute_host(args)
                .await
                .map_err(operation),
            Terminal { request } => self.terminal.execute(request).await,
            ReadArtifact { session_id, path } => {
                let cwd = self
                    .workspaces
                    .lock()
                    .await
                    .get(&session_id)
                    .cloned()
                    .ok_or_else(|| Error::Operation("session is not attached".into()))?;
                let root = tokio::fs::canonicalize(cwd).await.map_err(operation)?;
                let relative = std::path::Path::new(&path);
                if relative.is_absolute()
                    || relative
                        .components()
                        .any(|v| matches!(v, std::path::Component::ParentDir))
                {
                    return Err(Error::invalid_config(
                        "artifact path must be workspace-relative",
                    ));
                }
                let file = tokio::fs::canonicalize(root.join(relative))
                    .await
                    .map_err(operation)?;
                if !file.starts_with(root) {
                    return Err(Error::invalid_config("artifact escapes workspace"));
                }
                if tokio::fs::metadata(&file).await.map_err(operation)?.len() > 32 * 1024 * 1024 {
                    return Err(Error::Operation(
                        "artifact exceeds 32 MiB transport limit".into(),
                    ));
                }
                let bytes = tokio::fs::read(file).await.map_err(operation)?;
                Ok(
                    json!({"path":path,"base64":base64::engine::general_purpose::STANDARD.encode(bytes)}),
                )
            }
            SubagentNewId { session_id } => {
                self.session(&session_id).await?;
                encode(crate::subagent::SubagentId::new(
                    uuid::Uuid::now_v7().to_string(),
                ))
            }
            SubagentStart {
                session_id,
                request,
            } => encode(
                self.session(&session_id)
                    .await?
                    .subagents()
                    .start(request)
                    .await
                    .map_err(operation)?,
            ),
            SubagentQuery { session_id, id } => encode(
                self.session(&session_id)
                    .await?
                    .subagents()
                    .query(&id)
                    .await
                    .map_err(operation)?,
            ),
            SubagentWait {
                session_id,
                id,
                timeout_ms,
            } => {
                let subagents = self.session(&session_id).await?.subagents();
                tokio::time::timeout(Duration::from_millis(timeout_ms.into()), async {
                    loop {
                        let snapshot = subagents
                            .query(&id)
                            .await
                            .map_err(operation)?
                            .ok_or_else(|| Error::Operation("unknown subagent".into()))?;
                        if !matches!(
                            snapshot.state,
                            crate::subagent::SubagentState::Initializing
                                | crate::subagent::SubagentState::Running
                        ) {
                            return encode(snapshot);
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                })
                .await
                .map_err(|_| {
                    Error::Operation("subagent wait timed out; native child remains running".into())
                })?
            }
            SubagentCancel { session_id, target } => {
                let outcome = self
                    .session(&session_id)
                    .await?
                    .subagents()
                    .cancel(&target)
                    .await
                    .map_err(operation)?;
                encode(outcome)
            }
            SubagentCancelId { session_id, id } => encode(
                self.session(&session_id)
                    .await?
                    .subagents()
                    .cancel_id(&id)
                    .await
                    .map_err(operation)?,
            ),
            SchedulerList { session_id } => encode(
                self.session(&session_id)
                    .await?
                    .scheduler_snapshot()
                    .await
                    .map_err(operation)?,
            ),
            SchedulerCreate {
                session_id,
                operation_id,
                expected,
                task,
            } => encode(
                self.session(&session_id)
                    .await?
                    .create_scheduled_task(operation_id, expected, task)
                    .await
                    .map_err(operation)?,
            ),
            SchedulerUpdate {
                session_id,
                operation_id,
                expected,
                task,
            } => encode(
                self.session(&session_id)
                    .await?
                    .update_scheduled_task(operation_id, expected, task)
                    .await
                    .map_err(operation)?,
            ),
            SchedulerDelete {
                session_id,
                operation_id,
                expected,
                id,
            } => encode(
                self.session(&session_id)
                    .await?
                    .delete_scheduled_task(operation_id, expected, id)
                    .await
                    .map_err(operation)?,
            ),
            Quiesce { timeout_ms } => {
                let report = self
                    .agent
                    .quiesce(Duration::from_millis(timeout_ms.into()))
                    .await
                    .map_err(operation)?;
                Ok(
                    json!({"drained":report.drained(),"rejectedDuringQuiesce":report.rejected_during_quiesce()}),
                )
            }
            FinalExit { timeout_ms } => {
                self.agent
                    .final_exit(Duration::from_millis(timeout_ms.into()))
                    .await
                    .map_err(operation)?;
                if let Some(browser) = &self.handlers.browser {
                    browser.close().await.map_err(operation)?;
                }
                self.terminal.shutdown().await?;
                Ok(Value::Null)
            }
        }
    }

    async fn run_request(&self, request: p::Request) -> Result<Value> {
        let (options, id, load) = match request {
            p::Request::CreateSession { options } => (options, None, false),
            p::Request::LoadSession { id, options } => (options, Some(id), true),
            p::Request::ResumeSession { id, options } => (options, Some(id), false),
            other => return self.dispatch(other).await,
        };
        let specs = self
            .native_specs
            .iter()
            .cloned()
            .chain(options.tools)
            .collect::<Vec<_>>();
        let mut config = SessionConfig::new(&options.workspace.cwd);
        if let Some(model) = options.model {
            config = config.model(model);
        }
        for (key, value) in options.metadata {
            config = config.metadata(key, value);
        }
        for server in options.mcp_servers {
            config = config.mcp_server(server);
        }
        let session = match id {
            None => self.agent.create_session(config).await?,
            Some(id) if load => self.agent.load_session(id.into(), config).await?,
            Some(id) => self.agent.resume_session(id.into(), config).await?,
        };
        session
            .register_tools(
                specs
                    .into_iter()
                    .map(|spec| crate::native_tools::NativeTool {
                        spec,
                        handler: self.handlers.clone(),
                    })
                    .collect(),
            )
            .await?;
        self.workspaces
            .lock()
            .await
            .insert(session.id().to_string(), options.workspace.cwd.clone());
        let descriptor = p::SessionDescriptor {
            id: session.id().to_string(),
            workspace: options.workspace,
            initial_response: session.initial_response().clone(),
        };
        self.sessions
            .lock()
            .await
            .insert(descriptor.id.clone(), session);
        encode(descriptor)
    }
}

fn agent_config(config: p::RuntimeConfig, callbacks: Arc<Callbacks>) -> Result<AgentConfig> {
    let mut models = Vec::new();
    for model in config.models {
        let route = model.provider;
        let mut provider = match route.protocol {
            p::ProviderProtocol::OpenaiChat => {
                ProviderConfig::openai_chat(route.base_url, route.api_key, route.model)
            }
            p::ProviderProtocol::OpenaiResponses => {
                ProviderConfig::openai_responses(route.base_url, route.api_key, route.model)
            }
            p::ProviderProtocol::Anthropic => {
                ProviderConfig::anthropic(route.base_url, route.api_key, route.model)
            }
        };
        provider.headers = route.headers;
        provider.query_params = route.query_params;
        let mut native = ModelConfig::new(model.id, provider);
        if let Some(tokens) = model.context_window {
            native.context_window = NonZeroU64::new(tokens.into())
                .ok_or_else(|| Error::invalid_config("contextWindow must be positive"))?;
        }
        native.behavior.max_completion_tokens = model.max_completion_tokens;
        models.push(native);
    }
    let first = models
        .first()
        .cloned()
        .ok_or_else(|| Error::invalid_config("at least one explicit model route is required"))?;
    let mut native = AgentConfig::new(first)
        .default_model(config.default_model)
        .permission_policy(PermissionPolicy::AllowAll)
        .memory_mode(MemoryMode::Disabled)
        .client_handler(callbacks);
    native.models = models;
    native.web_search_model = config.web_search_model;
    native.session_summary_model = config.session_summary_model;
    native.compaction_model = config.compaction_model;
    native.image_description_model = config.image_description_model;
    native.subagents = config.subagents;
    Ok(native)
}

fn operation(error: impl std::fmt::Display) -> Error {
    Error::Operation(error.to_string())
}
fn encode(value: impl serde::Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(operation)
}

fn prompt_block(block: p::PromptBlock) -> crate::PromptBlock {
    match block {
        p::PromptBlock::Text { text } => crate::PromptBlock::Text(text),
        p::PromptBlock::Image { data, mime_type } => crate::PromptBlock::Image { data, mime_type },
        p::PromptBlock::Audio { data, mime_type } => crate::PromptBlock::Audio { data, mime_type },
        p::PromptBlock::ResourceLink { name, uri } => {
            crate::PromptBlock::ResourceLink { name, uri }
        }
        p::PromptBlock::EmbeddedText {
            uri,
            text,
            mime_type,
        } => crate::PromptBlock::EmbeddedText {
            uri,
            text,
            mime_type,
        },
    }
}

fn stop_reason(reason: crate::StopReason) -> String {
    match reason {
        crate::StopReason::EndTurn => "end_turn",
        crate::StopReason::MaxTokens => "max_tokens",
        crate::StopReason::MaxTurnRequests => "max_turn_requests",
        crate::StopReason::Refusal => "refusal",
        crate::StopReason::Cancelled => "cancelled",
        crate::StopReason::Error => "error",
        crate::StopReason::Other => "other",
    }
    .into()
}

fn update(update: crate::SessionUpdate) -> p::Update {
    use crate::SessionUpdate as U;
    match update {
        U::UserText(v) => p::Update::UserText(v),
        U::AssistantText(v) => p::Update::AssistantText(v),
        U::ThoughtText(v) => p::Update::ThoughtText(v),
        U::ToolCall(v) => p::Update::ToolCall(p::ToolCall {
            id: v.id,
            title: Some(v.title),
            kind: Some(v.kind),
            status: Some(v.status),
            raw_input: v.raw_input,
            raw_output: v.raw_output,
        }),
        U::ToolCallUpdate(v) => p::Update::ToolCallUpdate(p::ToolCall {
            id: v.id,
            title: v.title,
            kind: v.kind,
            status: v.status,
            raw_input: v.raw_input,
            raw_output: v.raw_output,
        }),
        U::Plan(v) => p::Update::Plan(
            v.into_iter()
                .map(|v| p::PlanEntry {
                    content: v.content,
                    priority: v.priority,
                    status: v.status,
                })
                .collect(),
        ),
        U::TurnCompleted(v) => p::Update::TurnCompleted(v),
        U::Other(v) => native_update(v),
    }
}

fn native_update(value: Value) -> p::Update {
    use xai_grok_shell::extensions::notification::SessionUpdate as N;
    match serde_json::from_value::<N>(value.clone()) {
        Ok(N::AutoCompactStarted {
            tokens_used,
            context_window,
            percentage,
            reason,
        }) => p::Update::Compaction(p::CompactionUpdate::Started {
            tokens_used,
            context_window,
            percentage,
            reason,
        }),
        Ok(N::AutoCompactCompleted {
            tokens_before,
            tokens_after,
            elapsed_ms,
            summary_preview,
        }) => p::Update::Compaction(p::CompactionUpdate::Completed {
            tokens_before,
            tokens_after,
            elapsed_ms,
            summary_preview,
        }),
        Ok(N::AutoCompactFailed { error }) => {
            p::Update::Compaction(p::CompactionUpdate::Failed { error })
        }
        Ok(N::AutoCompactCancelled { reason }) => {
            p::Update::Compaction(p::CompactionUpdate::Cancelled {
                reason: serde_json::to_value(reason)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "unknown".into()),
            })
        }
        Ok(N::DiffReview { .. } | N::HookAnnotation { .. }) => p::Update::Other(value),
        Ok(_) => p::Update::NativeStatus(value),
        Err(_)
            if matches!(
                value.get("sessionUpdate").and_then(Value::as_str),
                Some(
                    "available_commands_update"
                        | "current_mode_update"
                        | "config_option_update"
                        | "session_info_update"
                        | "usage_update"
                )
            ) =>
        {
            p::Update::NativeStatus(value)
        }
        Err(_) => p::Update::Other(value),
    }
}

fn history_record(record: crate::HistoryRecord) -> p::HistoryRecord {
    p::HistoryRecord {
        session_id: record.session_id.to_string(),
        event_id: record.event_id,
        prompt_id: record.prompt_id,
        prompt_index: record.prompt_index,
        hide_from_scrollback: record.hide_from_scrollback,
        model: record.model,
        is_replay: record.is_replay,
        update: update(record.update),
        envelope_metadata: record.envelope_metadata,
        chunk_metadata: record.chunk_metadata,
    }
}

fn event(event: crate::Event) -> Option<p::RuntimeEvent> {
    match event {
        crate::Event::HistoryRecord(record) => Some(p::RuntimeEvent::HistoryRecord {
            record: history_record(*record),
        }),
        crate::Event::HistoryBoundary {
            session_id,
            boundary_id,
        } => Some(p::RuntimeEvent::HistoryBoundary {
            session_id: session_id.to_string(),
            boundary_id,
        }),
        crate::Event::Session {
            session_id,
            update: u,
            metadata,
        } => Some(p::RuntimeEvent::Session {
            session_id: session_id.to_string(),
            update: update(u),
            metadata,
        }),
        crate::Event::Extension { method, payload } => {
            Some(p::RuntimeEvent::Extension { method, payload })
        }
        crate::Event::Management(crate::management::ManagementEvent {
            kind: crate::management::ManagementEventKind::Subagent(event),
            ..
        }) => Some(p::RuntimeEvent::Subagent { event }),
        crate::Event::Management(crate::management::ManagementEvent {
            kind: crate::management::ManagementEventKind::Queue(snapshot),
            ..
        }) => Some(p::RuntimeEvent::Queue { snapshot }),
        crate::Event::Management(crate::management::ManagementEvent {
            kind:
                crate::management::ManagementEventKind::Scheduler {
                    session_id,
                    task_id,
                    version,
                    occurrence,
                    snapshot_required,
                },
            ..
        }) => Some(p::RuntimeEvent::Scheduler {
            session_id: session_id.to_string(),
            task_id,
            version,
            occurrence,
            snapshot_required,
        }),
        crate::Event::Management(_) => None,
    }
}

fn respond(output: &mpsc::UnboundedSender<p::ServerFrame>, id: String, result: Result<Value>) {
    let frame = match result {
        Ok(result) => p::ServerFrame::Response { id, result },
        Err(error) => p::ServerFrame::Error {
            id,
            error: p::RpcError {
                code: match error {
                    Error::InvalidConfig(_) => "invalid_config",
                    Error::AdmissionRejected { .. } => "admission_closed",
                    Error::RuntimeStopped => "runtime_stopped",
                    _ => "operation_failed",
                }
                .into(),
                message: error.to_string(),
            },
        },
    };
    let _ = output.send(frame);
}

/// Runs one Agent over inherited private stdin/stdout. EOF invokes checked exit;
/// it is never interpreted as successful persistence without a native receipt.
pub async fn run() -> Result<()> {
    let (output, mut outgoing) = mpsc::unbounded_channel();
    let (terminal_tx, mut terminal_rx) = mpsc::channel(256);
    let (frame_tx, mut frame_rx) = tokio::sync::watch::channel::<Option<Value>>(None);
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut control_open = true;
        let mut terminal_open = true;
        loop {
            if !control_open && !terminal_open {
                break;
            }
            let frame = tokio::select! {
                biased;
                frame = outgoing.recv(), if control_open => match frame { Some(frame)=>frame,None=>{control_open=false;continue;} },
                frame = terminal_rx.recv(), if terminal_open => match frame {Some(frame)=>frame,None=>{terminal_open=false;continue;}},
                changed = frame_rx.changed() => {
                    if changed.is_err() { break; }
                    let frame = frame_rx.borrow_and_update().clone();
                    match frame {Some(frame)=>p::ServerFrame::BrowserFrame {frame},None=>continue}
                }
            };
            let mut bytes = serde_json::to_vec(&frame).map_err(std::io::Error::other)?;
            bytes.push(b'\n');
            stdout.write_all(&bytes).await?;
            stdout.flush().await?;
        }
        Ok::<_, std::io::Error>(())
    });
    output
        .send(p::ServerFrame::Ready {
            protocol_version: p::PROTOCOL_VERSION,
        })
        .map_err(|_| Error::RuntimeStopped)?;
    let callbacks = Arc::new(Callbacks {
        output: output.clone(),
        pending: Arc::default(),
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut runtime: Option<Arc<Runtime>> = None;
    let mut requests = tokio::task::JoinSet::new();
    let mut events = None;
    let mut browser_frames = None;
    let mut terminal_events = None;
    let mut exit_requested = false;
    while let Some(line) = lines.next_line().await.map_err(operation)? {
        let frame: p::ClientFrame = serde_json::from_str(&line)
            .map_err(|_| Error::Operation("invalid protocol frame".into()))?;
        match frame {
            p::ClientFrame::CallbackResult { id, result, error } => {
                if let Some(reply) = callbacks.pending.lock().unwrap().remove(&id) {
                    let _ = reply.send(match (result, error) {
                        (_, Some(error)) => Err(Error::Operation(error.message)),
                        (Some(value), None) => Ok(value),
                        (None, None) => Ok(Value::Null),
                    });
                }
            }
            p::ClientFrame::Request {
                id,
                request: p::Request::Initialize { config },
            } if runtime.is_none() => {
                let browser = config.browser.as_ref().map(|config| {
                    Arc::new(sophon_browser::BrowserService::new(
                        sophon_browser::BrowserConfig {
                            executable: config.executable.clone().into(),
                            data_dir: config.data_dir.clone().into(),
                            artifact_dir: config.artifact_dir.clone().into(),
                            headless: config.headless,
                            no_sandbox: config.no_sandbox,
                        },
                    ))
                });
                let mut native_specs = Vec::new();
                if browser.is_some() {
                    native_specs.extend(
                        sophon_browser::BrowserService::tool_specs()
                            .into_iter()
                            .map(|v| p::ToolSpec {
                                name: v.name,
                                description: v.description,
                                input_schema: v.input_schema,
                            }),
                    );
                }
                if let Some(media) = &config.media {
                    native_specs.extend(
                        crate::native_media::NativeMediaService::tool_specs()
                            .into_iter()
                            .filter(|v| match v.name.as_str() {
                                "generate_image" => media.image.is_some(),
                                "generate_speech" => media.speech.is_some(),
                                "generate_video" => media.video.is_some(),
                                _ => false,
                            }),
                    );
                }
                let handlers = Arc::new(RuntimeTools {
                    callbacks: callbacks.clone(),
                    browser: browser.clone(),
                    media: config
                        .media
                        .clone()
                        .map(crate::native_media::NativeMediaService::new),
                });
                let started = match agent_config(config, callbacks.clone()) {
                    Ok(config) => Agent::start(config).await,
                    Err(error) => Err(error),
                };
                match started {
                    Ok(agent) => {
                        let terminal = crate::native_terminal::NativeTerminalService::new();
                        let mut rx = terminal.subscribe();
                        let tx = terminal_tx.clone();
                        terminal_events = Some(tokio::spawn(async move {
                            loop {
                                let frame = match rx.recv().await {
                                    Ok(event) => p::ServerFrame::Terminal { event },
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(
                                        dropped,
                                    )) => p::ServerFrame::TerminalGap {
                                        dropped: dropped.min(u32::MAX as u64) as u32,
                                    },
                                    Err(_) => break,
                                };
                                if tx.send(frame).await.is_err() {
                                    break;
                                }
                            }
                        }));
                        if let Some(browser) = browser {
                            let mut rx = browser.subscribe_frames();
                            let tx = frame_tx.clone();
                            browser_frames = Some(tokio::spawn(async move {
                                loop {
                                    match rx.recv().await {
                                        Ok(frame) => {
                                            tx.send_replace(Some(frame));
                                        }
                                        Err(tokio::sync::broadcast::error::RecvError::Lagged(
                                            _,
                                        )) => continue,
                                        Err(_) => break,
                                    }
                                }
                            }));
                        }
                        let mut rx = agent.subscribe();
                        let out = output.clone();
                        events = Some(tokio::spawn(async move {
                            let mut sequence = 0;
                            loop {
                                let next = match rx.recv().await {
                                    Ok(v) => event(v),
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                        Some(p::RuntimeEvent::Gap { dropped: n as u32 })
                                    }
                                    Err(_) => break,
                                };
                                if let Some(event) = next {
                                    sequence += 1;
                                    if out.send(p::ServerFrame::Event { sequence, event }).is_err()
                                    {
                                        break;
                                    }
                                }
                            }
                        }));
                        let initial = agent.initialization_response().clone();
                        runtime = Some(Arc::new(Runtime {
                            agent,
                            sessions: Mutex::default(),
                            workspaces: Mutex::default(),
                            handlers,
                            native_specs,
                            terminal,
                        }));
                        respond(&output, id, Ok(initial));
                    }
                    Err(error) => respond(&output, id, Err(error)),
                }
            }
            p::ClientFrame::Request { id, request } => {
                if exit_requested {
                    respond(&output, id, Err(Error::RuntimeStopped));
                    continue;
                }
                if let Some(runtime) = &runtime {
                    let runtime = runtime.clone();
                    let out = output.clone();
                    exit_requested = matches!(request, p::Request::FinalExit { .. });
                    let is_exit = exit_requested;
                    requests.spawn(async move {
                        let result = runtime.run_request(request).await;
                        let exit_error = if is_exit {
                            result
                                .as_ref()
                                .err()
                                .map(|e| Error::Operation(e.to_string()))
                        } else {
                            None
                        };
                        respond(&out, id, result);
                        match exit_error {
                            Some(error) => Err(error),
                            None => Ok(()),
                        }
                    });
                    if exit_requested {
                        break;
                    }
                } else {
                    respond(
                        &output,
                        id,
                        Err(Error::Operation("initialize required".into())),
                    );
                }
            }
        }
    }
    if !exit_requested {
        if let Some(runtime) = &runtime {
            runtime
                .dispatch(p::Request::FinalExit { timeout_ms: 30_000 })
                .await?;
        }
    }
    while let Some(result) = requests.join_next().await {
        result.map_err(operation)??;
    }
    drop(runtime);
    drop(callbacks);
    if let Some(events) = events {
        events.await.map_err(operation)?;
    }
    if let Some(task) = browser_frames {
        task.abort();
        let _ = task.await;
    }
    if let Some(task) = terminal_events {
        task.await.map_err(operation)?;
    }
    drop(terminal_tx);
    drop(output);
    // Keep the latest-frame channel alive until the reliable writer drains.
    let result = writer.await.map_err(operation)?.map_err(operation);
    drop(frame_tx);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn browser_artifacts_use_invocation_workspace_and_never_clobber() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let id = "01995175-3b18-7001-8ac1-3ac1a7e80d11";
        let artifact = |bytes: &[u8]| json!({"mime_type":"image/png", "base64":base64::engine::general_purpose::STANDARD.encode(bytes)});
        let (a, b) = tokio::join!(
            publish_browser_artifact(
                first.path().to_str().unwrap(),
                id,
                artifact(b"first-workspace")
            ),
            publish_browser_artifact(
                second.path().to_str().unwrap(),
                id,
                artifact(b"second-workspace-29")
            ),
        );
        let a = a.unwrap();
        let b = b.unwrap();
        assert_eq!(a["mimeType"], "image/png");
        assert_eq!(b["bytes"], 19);
        assert_ne!(a["revision"], b["revision"]);
        let relative = a["path"].as_str().unwrap();
        assert_eq!(
            std::fs::read(first.path().join(relative)).unwrap(),
            b"first-workspace"
        );
        assert_eq!(
            std::fs::read(second.path().join(relative)).unwrap(),
            b"second-workspace-29"
        );
        assert!(
            publish_browser_artifact(first.path().to_str().unwrap(), id, artifact(b"overwrite"))
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read(first.path().join(relative)).unwrap(),
            b"first-workspace"
        );
        assert_eq!(
            std::fs::read_dir(first.path().join(".native-browser"))
                .unwrap()
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn browser_artifact_symlink_escape_fails_without_publication() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join(".native-browser")).unwrap();
        assert!(
            publish_browser_artifact(
                root.path().to_str().unwrap(),
                "artifact",
                json!({"mime_type":"image/png","base64":"YWJj"})
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
