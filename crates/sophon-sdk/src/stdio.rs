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
            "browser" => self
                .browser
                .as_ref()
                .ok_or_else(|| Error::Operation("browser is not configured".into()))?
                .execute(name, args)
                .await
                .map_err(operation),
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
                Ok(
                    json!({"sessionId":queue.session_id.to_string(),"version":{"generation":queue.version.generation,"revision":queue.version.revision}, "running":queue.running.map(|v|json!({"id":v.id.as_str(),"text":v.text})), "pending": queue.pending.into_iter().map(|v| json!({"id":v.id.as_str(),"text":v.text,"position":v.position})).collect::<Vec<_>>()}),
                )
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
                .execute("browser", args)
                .await
                .map_err(operation),
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
                Ok(json!({"outcome":format!("{outcome:?}")}))
            }
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
    native.image_description_model = config.image_description_model;
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
        U::TurnCompleted(v) => p::Update::TurnCompleted(
            json!({"promptId":v.prompt_id,"promptIndex":v.prompt_index,"stopReason":stop_reason(v.stop_reason),"agentResult":v.agent_result,"errorKind":v.error_kind,"elapsedMs":v.elapsed_ms}),
        ),
        U::Other(v) => p::Update::Other(v),
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
    let (frame_tx, mut frame_rx) = tokio::sync::watch::channel::<Option<Value>>(None);
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        loop {
            let frame = tokio::select! {
                biased;
                frame = outgoing.recv() => match frame { Some(frame)=>frame,None=>break },
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
    drop(output);
    // Keep the latest-frame channel alive until the reliable writer drains.
    let result = writer.await.map_err(operation)?.map_err(operation);
    drop(frame_tx);
    result
}
