use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::{self as acp, Agent as _};
use indexmap::IndexMap;
use tokio::sync::{broadcast, mpsc, oneshot};
use xai_acp_lib::{AcpGatewayReceiver, AcpGatewaySender};
use xai_grok_sampler::AuthScheme;
use xai_grok_shell::agent::config::{
    AgentMode, Config as GrokConfig, ImagineProviderConfig as GrokImagineProviderConfig,
    ModelEntry, RuntimeResolutionContext,
};
use xai_grok_shell::agent::mvp_agent::MvpAgent;
use xai_grok_shell::config::{PromptSuggestModelPin, RequirementSource};
use xai_grok_shell::sampling::ApiBackend;

use crate::config::{AgentConfig, PermissionPolicy, ProviderProtocol};
use crate::event::{Event, PlanEntry, SessionUpdate, ToolCall, ToolCallUpdate};
use crate::{
    ClientHandler, Error, PermissionDecision, PermissionOption, PermissionOptionKind,
    PermissionRequest, PromptBlock, PromptResult, Session, SessionConfig, SessionId, SessionInfo,
    SessionPage, StopReason,
};

type Reply<T> = oneshot::Sender<Result<T, Error>>;

enum Command {
    CreateSession(SessionConfig, Reply<(SessionId, serde_json::Value)>),
    LoadSession(SessionConfig, SessionId, Reply<serde_json::Value>),
    ResumeSession(SessionConfig, SessionId, Reply<serde_json::Value>),
    ListSessions(Option<PathBuf>, Option<String>, Reply<SessionPage>),
    Prompt(
        SessionId,
        Vec<PromptBlock>,
        serde_json::Map<String, serde_json::Value>,
        Reply<PromptResult>,
    ),
    SetModel(
        SessionId,
        String,
        serde_json::Map<String, serde_json::Value>,
        Reply<()>,
    ),
    SetMode(SessionId, String, Reply<()>),
    Extension(String, serde_json::Value, Reply<serde_json::Value>),
    ExtensionNotification(String, serde_json::Value, Reply<()>),
    Cancel(
        SessionId,
        serde_json::Map<String, serde_json::Value>,
        Reply<()>,
    ),
    Close(SessionId, Reply<()>),
    Shutdown(Reply<()>),
}

struct AgentInner {
    commands: mpsc::UnboundedSender<Command>,
    events: broadcast::Sender<Event>,
    initialization_response: serde_json::Value,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for AgentInner {
    fn drop(&mut self) {
        let (reply, _) = oneshot::channel();
        let _ = self.commands.send(Command::Shutdown(reply));
    }
}

/// Send/Sync handle to a Grok Build agent running on a private local executor.
#[derive(Clone)]
pub struct Agent {
    inner: Arc<AgentInner>,
}

impl Agent {
    pub async fn start(config: AgentConfig) -> Result<Self, Error> {
        require_hermetic_discovery()?;
        config.validate()?;
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(1024);
        let (ready_tx, ready_rx) = oneshot::channel();
        let worker_events = events.clone();
        let worker = std::thread::Builder::new()
            .name("sophon-grok-build".into())
            .spawn(move || run_worker(config, command_rx, worker_events, ready_tx))
            .map_err(|error| Error::Start(error.to_string()))?;

        match ready_rx.await {
            Ok(Ok(initialization_response)) => Ok(Self {
                inner: Arc::new(AgentInner {
                    commands,
                    events,
                    initialization_response,
                    worker: Mutex::new(Some(worker)),
                }),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let _ = worker.join();
                Err(Error::RuntimeStopped)
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Complete upstream initialization response as forward-compatible JSON.
    ///
    /// This includes advertised capabilities, model state, commands, MCP
    /// servers, agent identity/version metadata, and future additions without
    /// exposing ACP types.
    pub fn initialization_response(&self) -> &serde_json::Value {
        &self.inner.initialization_response
    }

    pub async fn create_session(&self, config: SessionConfig) -> Result<Session, Error> {
        let (id, initial_response) = self
            .request(|reply| Command::CreateSession(config, reply))
            .await?;
        Ok(Session::new(self.clone(), id, initial_response))
    }

    pub async fn load_session(
        &self,
        id: SessionId,
        config: SessionConfig,
    ) -> Result<Session, Error> {
        let initial_response = self
            .request(|reply| Command::LoadSession(config, id.clone(), reply))
            .await?;
        Ok(Session::new(self.clone(), id, initial_response))
    }

    /// Reattach to a live or persisted session without replaying its history.
    pub async fn resume_session(
        &self,
        id: SessionId,
        config: SessionConfig,
    ) -> Result<Session, Error> {
        let initial_response = self
            .request(|reply| Command::ResumeSession(config, id.clone(), reply))
            .await?;
        Ok(Session::new(self.clone(), id, initial_response))
    }

    /// List persisted sessions, optionally scoped to a working directory.
    ///
    /// Pass `next_cursor` back as `cursor` to fetch the next page.
    pub async fn list_sessions(
        &self,
        cwd: Option<&Path>,
        cursor: Option<&str>,
    ) -> Result<SessionPage, Error> {
        self.request(|reply| {
            Command::ListSessions(cwd.map(Path::to_path_buf), cursor.map(str::to_owned), reply)
        })
        .await
    }

    /// Invoke any Grok Build `x.ai/*` extension without exposing ACP types.
    pub async fn extension(
        &self,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, Error> {
        let method = method.into();
        self.request(|reply| Command::Extension(method, params, reply))
            .await
    }

    /// Send a one-way Grok Build extension notification.
    pub async fn notify_extension(
        &self,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Result<(), Error> {
        let method = method.into();
        self.request(|reply| Command::ExtensionNotification(method, params, reply))
            .await
    }

    pub async fn shutdown(&self) -> Result<(), Error> {
        let result = self.request(Command::Shutdown).await;
        let worker = self
            .inner
            .worker
            .lock()
            .map_err(|_| Error::RuntimeStopped)?
            .take();
        if let Some(worker) = worker {
            tokio::task::spawn_blocking(move || worker.join())
                .await
                .map_err(|error| Error::Operation(error.to_string()))?
                .map_err(|_| Error::Operation("Grok Build worker panicked".into()))?;
        }
        result
    }

    pub(crate) async fn prompt(
        &self,
        session_id: SessionId,
        prompt: Vec<PromptBlock>,
        metadata: serde_json::Map<String, serde_json::Value>,
    ) -> Result<PromptResult, Error> {
        self.request(|reply| Command::Prompt(session_id, prompt, metadata, reply))
            .await
    }

    pub(crate) async fn set_model(
        &self,
        session_id: SessionId,
        model: String,
        metadata: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), Error> {
        self.request(|reply| Command::SetModel(session_id, model, metadata, reply))
            .await
    }

    pub(crate) async fn set_mode(&self, session_id: SessionId, mode: String) -> Result<(), Error> {
        self.request(|reply| Command::SetMode(session_id, mode, reply))
            .await
    }

    pub(crate) async fn cancel(
        &self,
        session_id: SessionId,
        metadata: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), Error> {
        self.request(|reply| Command::Cancel(session_id, metadata, reply))
            .await
    }

    pub(crate) async fn close(&self, session_id: SessionId) -> Result<(), Error> {
        self.request(|reply| Command::Close(session_id, reply))
            .await
    }

    async fn request<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Result<T, Error> {
        let (reply, response) = oneshot::channel();
        self.inner
            .commands
            .send(command(reply))
            .map_err(|_| Error::RuntimeStopped)?;
        response.await.map_err(|_| Error::RuntimeStopped)?
    }
}

fn run_worker(
    config: AgentConfig,
    commands: mpsc::UnboundedReceiver<Command>,
    events: broadcast::Sender<Event>,
    ready: oneshot::Sender<Result<serde_json::Value, Error>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(Error::Start(error.to_string())));
            return;
        }
    };
    let local = tokio::task::LocalSet::new();
    runtime.block_on(local.run_until(async move {
        let result = start_worker(config, commands, events).await;
        match result {
            Ok((agent, commands, initialization_response)) => {
                let _ = ready.send(Ok(initialization_response));
                command_loop(agent, commands).await;
            }
            Err(error) => {
                let _ = ready.send(Err(error));
            }
        }
    }));
}

async fn start_worker(
    config: AgentConfig,
    commands: mpsc::UnboundedReceiver<Command>,
    events: broadcast::Sender<Event>,
) -> Result<
    (
        Rc<MvpAgent>,
        mpsc::UnboundedReceiver<Command>,
        serde_json::Value,
    ),
    Error,
> {
    let (grok_config, models) = grok_config(&config)?;
    let auth_manager = Arc::new(grok_config.create_auth_manager());
    let (gateway_tx, gateway_rx) = mpsc::unbounded_channel();
    let agent = Rc::new(
        MvpAgent::new(
            AcpGatewaySender::new(gateway_tx),
            &grok_config,
            auth_manager,
            Some(models),
        )
        .map_err(Error::Start)?,
    );
    let client = EmbeddedClient {
        events,
        permission_policy: config.permission_policy,
        handler: config.client_handler,
    };
    tokio::task::spawn_local(
        AcpGatewayReceiver::<acp::AgentSide, _>::new(gateway_rx, client).run(),
    );

    let initialized = agent
        .initialize(
            acp::InitializeRequest::new(acp::ProtocolVersion::V1)
                .client_capabilities(acp::ClientCapabilities::new().terminal(false))
                .meta(
                    serde_json::json!({
                        "startupHints": { "nonInteractive": true },
                        "clientType": "sophon-sdk",
                        "clientVersion": env!("CARGO_PKG_VERSION"),
                    })
                    .as_object()
                    .cloned(),
                ),
        )
        .await
        .map_err(acp_error)?;
    let auth_method = initialized
        .auth_methods
        .iter()
        .find(|method| method.id().0.as_ref() == "xai.api_key")
        .ok_or_else(|| Error::Start("Grok Build did not advertise API-key auth".into()))?;
    agent
        .authenticate(
            acp::AuthenticateRequest::new(auth_method.id().clone())
                .meta(serde_json::json!({ "headless": true }).as_object().cloned()),
        )
        .await
        .map_err(acp_error)?;
    let initialization_response = raw_response(&initialized)?;
    Ok((agent, commands, initialization_response))
}

async fn command_loop(agent: Rc<MvpAgent>, mut commands: mpsc::UnboundedReceiver<Command>) {
    while let Some(command) = commands.recv().await {
        match command {
            Command::CreateSession(config, reply) => {
                let agent = agent.clone();
                tokio::task::spawn_local(async move {
                    let result = match session_request(config) {
                        Ok(request) => agent
                            .new_session(request)
                            .await
                            .map_err(acp_error)
                            .and_then(|response| {
                                let id = SessionId(response.session_id.0.to_string());
                                raw_response(&response).map(|response| (id, response))
                            }),
                        Err(error) => Err(error),
                    };
                    let _ = reply.send(result);
                });
            }
            Command::LoadSession(config, id, reply) => {
                let agent = agent.clone();
                tokio::task::spawn_local(async move {
                    let result = match session_parts(config) {
                        Ok((cwd, mcp_servers, metadata)) => {
                            let request =
                                acp::LoadSessionRequest::new(acp::SessionId::new(id.0), cwd)
                                    .mcp_servers(mcp_servers)
                                    .meta(metadata);
                            agent
                                .load_session(request)
                                .await
                                .map_err(acp_error)
                                .and_then(|response| raw_response(&response))
                        }
                        Err(error) => Err(error),
                    };
                    let _ = reply.send(result);
                });
            }
            Command::ResumeSession(config, id, reply) => {
                let agent = agent.clone();
                tokio::task::spawn_local(async move {
                    let result = match session_parts(config) {
                        Ok((cwd, mcp_servers, metadata)) => {
                            let request =
                                acp::ResumeSessionRequest::new(acp::SessionId::new(id.0), cwd)
                                    .mcp_servers(mcp_servers)
                                    .meta(metadata);
                            agent
                                .resume_session(request)
                                .await
                                .map_err(acp_error)
                                .and_then(|response| raw_response(&response))
                        }
                        Err(error) => Err(error),
                    };
                    let _ = reply.send(result);
                });
            }
            Command::ListSessions(cwd, cursor, reply) => {
                let agent = agent.clone();
                tokio::task::spawn_local(async move {
                    let response = agent
                        .list_sessions(acp::ListSessionsRequest::new().cwd(cwd).cursor(cursor))
                        .await
                        .map_err(acp_error);
                    let result = response.map(|response| SessionPage {
                        sessions: response
                            .sessions
                            .into_iter()
                            .map(|session| SessionInfo {
                                id: SessionId(session.session_id.0.to_string()),
                                cwd: session.cwd,
                                title: session.title,
                                updated_at: session.updated_at,
                            })
                            .collect(),
                        next_cursor: response.next_cursor,
                    });
                    let _ = reply.send(result);
                });
            }
            Command::Prompt(id, prompt, metadata, reply) => {
                let agent = agent.clone();
                tokio::task::spawn_local(async move {
                    let blocks = prompt
                        .into_iter()
                        .map(prompt_block)
                        .collect::<Result<Vec<_>, _>>();
                    let result = match blocks {
                        Ok(blocks) => agent
                            .prompt(
                                acp::PromptRequest::new(acp::SessionId::new(id.0), blocks)
                                    .meta(metadata),
                            )
                            .await
                            .map_err(acp_error)
                            .and_then(|response| {
                                let stop_reason = stop_reason(response.stop_reason);
                                raw_response(&response).map(|raw_response| PromptResult {
                                    stop_reason,
                                    raw_response,
                                })
                            }),
                        Err(error) => Err(error),
                    };
                    let _ = reply.send(result);
                });
            }
            Command::SetModel(id, model, metadata, reply) => {
                let result = agent
                    .set_session_model(
                        acp::SetSessionModelRequest::new(
                            acp::SessionId::new(id.0),
                            acp::ModelId::new(model),
                        )
                        .meta(metadata),
                    )
                    .await
                    .map(|_| ())
                    .map_err(acp_error);
                let _ = reply.send(result);
            }
            Command::Extension(method, params, reply) => {
                let result = extension_request(&agent, method, params).await;
                let _ = reply.send(result);
            }
            Command::ExtensionNotification(method, params, reply) => {
                let result = extension_notification(&agent, method, params).await;
                let _ = reply.send(result);
            }
            Command::SetMode(id, mode, reply) => {
                let result = agent
                    .set_session_mode(acp::SetSessionModeRequest::new(
                        acp::SessionId::new(id.0),
                        acp::SessionModeId::new(mode),
                    ))
                    .await
                    .map(|_| ())
                    .map_err(acp_error);
                let _ = reply.send(result);
            }
            Command::Cancel(id, metadata, reply) => {
                let result = agent
                    .cancel(acp::CancelNotification::new(acp::SessionId::new(id.0)).meta(metadata))
                    .await
                    .map_err(acp_error);
                let _ = reply.send(result);
            }
            Command::Close(id, reply) => {
                let result = agent
                    .close_session(acp::CloseSessionRequest::new(acp::SessionId::new(id.0)))
                    .await
                    .map(|_| ())
                    .map_err(acp_error);
                let _ = reply.send(result);
            }
            Command::Shutdown(reply) => {
                agent.flush_all_sessions(Duration::from_secs(5)).await;
                let _ = reply.send(Ok(()));
                break;
            }
        }
    }
}

fn require_hermetic_discovery() -> Result<(), Error> {
    if xai_grok_config::set_hermetic_discovery(true) {
        Ok(())
    } else {
        Err(Error::Start(
            "Grok discovery mode was resolved before sophon-sdk startup".into(),
        ))
    }
}

fn grok_config(config: &AgentConfig) -> Result<(GrokConfig, IndexMap<String, ModelEntry>), Error> {
    // An embedding application owns its whole configuration surface, so the
    // facade fixes hermetic discovery before the first config load. Fail
    // closed if another caller already resolved this process to ambient mode.
    require_hermetic_discovery()?;
    let raw_config = xai_grok_shell::config::load_effective_config()
        .map_err(|error| Error::Start(format!("failed to load Grok config: {error}")))?;
    let mut grok = GrokConfig::new_from_toml_cfg(&raw_config).map_err(Error::Start)?;
    let remote_settings = Default::default();
    grok.resolve_runtime_fields(&RuntimeResolutionContext {
        raw_config: &raw_config,
        remote_settings: Some(&remote_settings),
        is_headless: true,
        cli_subagents: None,
        cli_web_search_model: None,
        cli_session_summary_model: None,
        memory_enabled_override: None,
        disable_web_search: false,
        todo_gate: false,
        laziness_debug_log: None,
        storage_mode: None,
    });
    grok.remote_settings = Some(remote_settings);
    grok.mode = AgentMode::Headless;
    grok.default_model_override = config.default_model.clone();
    grok.models.default.clone_from(&config.default_model);
    grok.default_yolo_mode = config.permission_policy == PermissionPolicy::AllowAll;
    grok.web_search_model = config.web_search_model.clone().unwrap_or_default();
    grok.session_summary_model = config
        .session_summary_model
        .clone()
        .or_else(|| config.default_model.clone());
    grok.image_description_model = config
        .image_description_model
        .clone()
        .or_else(|| config.default_model.clone());
    grok.prompt_suggest_model_pin = PromptSuggestModelPin::Pinned(
        config
            .prompt_suggestion_model
            .clone()
            .or_else(|| config.default_model.clone())
            .expect("validated AgentConfig always has a default model"),
    );

    let (image_generation, image_edit, video_generation) = if let Some(media) = &config.media {
        grok.imagine_provider = Some(GrokImagineProviderConfig {
            base_url: media.provider.base_url.clone(),
            api_key: media.provider.api_key.clone(),
            extra_headers: media
                .provider
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        });
        grok.features
            .image_gen_model_override
            .clone_from(&media.image_generation_model);
        grok.features
            .image_edit_model_override
            .clone_from(&media.image_edit_model);
        (
            media.image_generation,
            media.image_edit,
            media.video_generation,
        )
    } else {
        (false, false, false)
    };
    // SDK media routing is explicit and must not be re-enabled by ambient
    // GROK_IMAGE_* environment variables or upstream defaults.
    grok.features.image_gen = Some(image_generation);
    grok.features.video_gen = Some(video_generation);
    grok.requirements
        .image_gen
        .pin(image_generation, RequirementSource::Unknown);
    grok.requirements
        .image_edit
        .pin(image_edit, RequirementSource::Unknown);
    grok.requirements
        .video_gen
        .pin(video_generation, RequirementSource::Unknown);

    let models = config
        .models
        .iter()
        .map(|model| {
            let mut entry = ModelEntry::fallback(&model.id, &grok.endpoints);
            entry.info.id = Some(model.id.clone());
            entry.info.model.clone_from(&model.provider.model);
            entry.info.base_url.clone_from(&model.provider.base_url);
            entry.info.context_window = model.context_window;
            entry.info.api_backend = match model.provider.protocol {
                ProviderProtocol::OpenAiChatCompletions => ApiBackend::ChatCompletions,
                ProviderProtocol::OpenAiResponses => ApiBackend::Responses,
                ProviderProtocol::AnthropicMessages => ApiBackend::Messages,
            };
            entry.info.auth_scheme = match model.provider.protocol {
                ProviderProtocol::OpenAiChatCompletions | ProviderProtocol::OpenAiResponses => {
                    AuthScheme::Bearer
                }
                ProviderProtocol::AnthropicMessages => AuthScheme::XApiKey,
            };
            entry.info.extra_headers = model
                .provider
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            if model.provider.protocol == ProviderProtocol::AnthropicMessages {
                // Grok Build reconstructs auth facts from its on-disk config during
                // a turn, where an SDK-prefetched model is absent. Preserve the
                // Messages API's required header at the provider boundary.
                entry
                    .info
                    .extra_headers
                    .insert("x-api-key".into(), model.provider.api_key.clone());
            }
            entry.info.query_params = model
                .provider
                .query_params
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            entry.api_key = Some(model.provider.api_key.clone());
            (model.id.clone(), entry)
        })
        .collect();
    Ok((grok, models))
}

type SessionParts = (
    PathBuf,
    Vec<acp::McpServer>,
    serde_json::Map<String, serde_json::Value>,
);

fn session_parts(config: SessionConfig) -> Result<SessionParts, Error> {
    let SessionConfig {
        cwd,
        model,
        mut metadata,
        mcp_servers,
    } = config;
    if let Some(model) = model {
        metadata.insert("modelId".into(), serde_json::Value::String(model));
    }
    let mcp_servers = mcp_servers
        .into_iter()
        .map(|server| {
            serde_json::from_value(server)
                .map_err(|error| Error::invalid_config(format!("invalid MCP server: {error}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((cwd, mcp_servers, metadata))
}

fn session_request(config: SessionConfig) -> Result<acp::NewSessionRequest, Error> {
    let (cwd, mcp_servers, metadata) = session_parts(config)?;
    Ok(acp::NewSessionRequest::new(cwd)
        .mcp_servers(mcp_servers)
        .meta(metadata))
}

fn prompt_block(block: PromptBlock) -> Result<acp::ContentBlock, Error> {
    let block = match block {
        PromptBlock::Text(text) => acp::ContentBlock::Text(acp::TextContent::new(text)),
        PromptBlock::Image { data, mime_type } => {
            acp::ContentBlock::Image(acp::ImageContent::new(data, mime_type))
        }
        PromptBlock::Audio { data, mime_type } => {
            acp::ContentBlock::Audio(acp::AudioContent::new(data, mime_type))
        }
        PromptBlock::ResourceLink { name, uri } => {
            acp::ContentBlock::ResourceLink(acp::ResourceLink::new(name, uri))
        }
        PromptBlock::EmbeddedText {
            uri,
            text,
            mime_type,
        } => acp::ContentBlock::Resource(acp::EmbeddedResource::new(
            acp::EmbeddedResourceResource::TextResourceContents(
                acp::TextResourceContents::new(text, uri).mime_type(mime_type),
            ),
        )),
        PromptBlock::EmbeddedBlob {
            uri,
            blob,
            mime_type,
        } => acp::ContentBlock::Resource(acp::EmbeddedResource::new(
            acp::EmbeddedResourceResource::BlobResourceContents(
                acp::BlobResourceContents::new(blob, uri).mime_type(mime_type),
            ),
        )),
        PromptBlock::Raw(value) => serde_json::from_value(value).map_err(|error| {
            Error::invalid_config(format!("invalid raw prompt content block: {error}"))
        })?,
    };
    Ok(block)
}

async fn extension_request(
    agent: &MvpAgent,
    method: String,
    params: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    let params = serde_json::value::to_raw_value(&params)
        .map_err(|error| Error::Operation(error.to_string()))?;
    let response = agent
        .ext_method(acp::ExtRequest::new(method, params.into()))
        .await
        .map_err(acp_error)?;
    serde_json::from_str(response.0.get()).map_err(|error| Error::Operation(error.to_string()))
}

async fn extension_notification(
    agent: &MvpAgent,
    method: String,
    params: serde_json::Value,
) -> Result<(), Error> {
    let params = serde_json::value::to_raw_value(&params)
        .map_err(|error| Error::Operation(error.to_string()))?;
    agent
        .ext_notification(acp::ExtNotification::new(method, params.into()))
        .await
        .map_err(acp_error)
}

fn stop_reason(reason: acp::StopReason) -> StopReason {
    match reason {
        acp::StopReason::EndTurn => StopReason::EndTurn,
        acp::StopReason::MaxTokens => StopReason::MaxTokens,
        acp::StopReason::MaxTurnRequests => StopReason::MaxTurnRequests,
        acp::StopReason::Refusal => StopReason::Refusal,
        acp::StopReason::Cancelled => StopReason::Cancelled,
        _ => StopReason::Other,
    }
}

fn acp_error(error: acp::Error) -> Error {
    Error::Operation(error.to_string())
}

fn raw_response(response: &impl serde::Serialize) -> Result<serde_json::Value, Error> {
    serde_json::to_value(response).map_err(|error| Error::Operation(error.to_string()))
}

struct EmbeddedClient {
    events: broadcast::Sender<Event>,
    permission_policy: PermissionPolicy,
    handler: Option<Arc<dyn ClientHandler>>,
}

#[async_trait::async_trait(?Send)]
impl acp::Client for EmbeddedClient {
    async fn request_permission(
        &self,
        request: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        if self.permission_policy == PermissionPolicy::Delegate {
            let Some(handler) = &self.handler else {
                return Ok(acp::RequestPermissionResponse::new(
                    acp::RequestPermissionOutcome::Cancelled,
                ));
            };
            let options = request
                .options
                .iter()
                .map(|option| PermissionOption {
                    id: option.option_id.0.to_string(),
                    name: option.name.clone(),
                    kind: permission_option_kind(option.kind),
                })
                .collect();
            let decision = handler
                .request_permission(PermissionRequest {
                    session_id: SessionId(request.session_id.0.to_string()),
                    tool_call: serde_json::to_value(&request.tool_call).unwrap_or_default(),
                    options,
                    metadata: request.meta.map(serde_json::Value::Object),
                })
                .await;
            let outcome = match decision {
                PermissionDecision::Select(id) => request
                    .options
                    .iter()
                    .find(|option| option.option_id.0.as_ref() == id)
                    .map(|option| {
                        acp::RequestPermissionOutcome::Selected(
                            acp::SelectedPermissionOutcome::new(option.option_id.clone()),
                        )
                    })
                    .unwrap_or(acp::RequestPermissionOutcome::Cancelled),
                PermissionDecision::Cancel => acp::RequestPermissionOutcome::Cancelled,
            };
            return Ok(acp::RequestPermissionResponse::new(outcome));
        }
        let preferred = match self.permission_policy {
            PermissionPolicy::DenyAll => [
                acp::PermissionOptionKind::RejectOnce,
                acp::PermissionOptionKind::RejectAlways,
            ],
            PermissionPolicy::AllowAll => [
                acp::PermissionOptionKind::AllowOnce,
                acp::PermissionOptionKind::AllowAlways,
            ],
            PermissionPolicy::Delegate => unreachable!("handled above"),
        };
        let outcome = preferred
            .iter()
            .find_map(|kind| request.options.iter().find(|option| option.kind == *kind))
            .map(|option| {
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    option.option_id.clone(),
                ))
            })
            .unwrap_or(acp::RequestPermissionOutcome::Cancelled);
        Ok(acp::RequestPermissionResponse::new(outcome))
    }

    async fn ext_method(&self, request: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        let Some(handler) = &self.handler else {
            return Err(acp::Error::method_not_found());
        };
        let params = serde_json::from_str(request.params.get())
            .map_err(|error| acp::Error::invalid_params().data(error.to_string()))?;
        let response = handler
            .extension(request.method.as_ref(), params)
            .await
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        let response = serde_json::value::to_raw_value(&response)
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        Ok(acp::ExtResponse::new(response.into()))
    }

    async fn session_notification(
        &self,
        notification: acp::SessionNotification,
    ) -> acp::Result<()> {
        let _ = self.events.send(Event::Session {
            session_id: SessionId(notification.session_id.0.to_string()),
            update: session_update(notification.update),
            metadata: notification.meta.map(serde_json::Value::Object),
        });
        Ok(())
    }

    async fn ext_notification(&self, notification: acp::ExtNotification) -> acp::Result<()> {
        let payload = serde_json::from_str(notification.params.get()).unwrap_or_default();
        let _ = self.events.send(Event::Extension {
            method: notification.method.to_string(),
            payload,
        });
        Ok(())
    }
}

fn permission_option_kind(kind: acp::PermissionOptionKind) -> PermissionOptionKind {
    match kind {
        acp::PermissionOptionKind::AllowOnce => PermissionOptionKind::AllowOnce,
        acp::PermissionOptionKind::AllowAlways => PermissionOptionKind::AllowAlways,
        acp::PermissionOptionKind::RejectOnce => PermissionOptionKind::RejectOnce,
        acp::PermissionOptionKind::RejectAlways => PermissionOptionKind::RejectAlways,
        _ => PermissionOptionKind::Other,
    }
}

fn session_update(update: acp::SessionUpdate) -> SessionUpdate {
    match update {
        acp::SessionUpdate::UserMessageChunk(chunk) => {
            text_update(chunk.content, SessionUpdate::UserText, "user_message_chunk")
        }
        acp::SessionUpdate::AgentMessageChunk(chunk) => text_update(
            chunk.content,
            SessionUpdate::AssistantText,
            "agent_message_chunk",
        ),
        acp::SessionUpdate::AgentThoughtChunk(chunk) => text_update(
            chunk.content,
            SessionUpdate::ThoughtText,
            "agent_thought_chunk",
        ),
        acp::SessionUpdate::ToolCall(tool) => SessionUpdate::ToolCall(Box::new(ToolCall {
            id: tool.tool_call_id.0.to_string(),
            title: tool.title,
            kind: json_string(&tool.kind),
            status: json_string(&tool.status),
            raw_input: tool.raw_input,
            raw_output: tool.raw_output,
        })),
        acp::SessionUpdate::ToolCallUpdate(tool) => {
            SessionUpdate::ToolCallUpdate(Box::new(ToolCallUpdate {
                id: tool.tool_call_id.0.to_string(),
                title: tool.fields.title,
                kind: tool.fields.kind.as_ref().map(json_string),
                status: tool.fields.status.as_ref().map(json_string),
                raw_input: tool.fields.raw_input,
                raw_output: tool.fields.raw_output,
            }))
        }
        acp::SessionUpdate::Plan(plan) => SessionUpdate::Plan(
            plan.entries
                .into_iter()
                .map(|entry| PlanEntry {
                    content: entry.content,
                    priority: json_string(&entry.priority),
                    status: json_string(&entry.status),
                })
                .collect(),
        ),
        other => SessionUpdate::Other(serde_json::to_value(other).unwrap_or_default()),
    }
}

fn text_update(
    content: acp::ContentBlock,
    text: impl FnOnce(String) -> SessionUpdate,
    kind: &str,
) -> SessionUpdate {
    match content {
        acp::ContentBlock::Text(content) => text(content.text),
        content => SessionUpdate::Other(serde_json::json!({
            "sessionUpdate": kind,
            "content": content,
        })),
    }
}

fn json_string(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

impl SessionConfig {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            model: None,
            metadata: serde_json::Map::new(),
            mcp_servers: Vec::new(),
        }
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Add Grok Build session metadata such as `agentProfile`, `pluginDirs`,
    /// `toolOverrides`, reasoning effort, or forward-compatible additions.
    pub fn metadata(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }

    /// Add one MCP server in the upstream ACP JSON shape.
    pub fn mcp_server(mut self, server: serde_json::Value) -> Self {
        self.mcp_servers.push(server);
        self
    }
}

impl Session {
    pub(crate) fn new(agent: Agent, id: SessionId, initial_response: serde_json::Value) -> Self {
        Self {
            agent,
            id,
            initial_response,
        }
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Complete response from the create/load/resume operation that attached
    /// this handle, including initial model/mode/config state and metadata.
    pub fn initial_response(&self) -> &serde_json::Value {
        &self.initial_response
    }

    pub async fn prompt(&self, text: impl Into<String>) -> Result<PromptResult, Error> {
        self.prompt_blocks([PromptBlock::Text(text.into())]).await
    }

    pub async fn prompt_blocks(
        &self,
        blocks: impl IntoIterator<Item = PromptBlock>,
    ) -> Result<PromptResult, Error> {
        self.prompt_blocks_with_metadata(blocks, serde_json::Map::new())
            .await
    }

    /// Prompt with raw upstream metadata for per-turn options and future
    /// additions that do not warrant an SDK mirror.
    pub async fn prompt_blocks_with_metadata(
        &self,
        blocks: impl IntoIterator<Item = PromptBlock>,
        metadata: serde_json::Map<String, serde_json::Value>,
    ) -> Result<PromptResult, Error> {
        let blocks = blocks.into_iter().collect::<Vec<_>>();
        if blocks.is_empty() {
            return Err(Error::invalid_config(
                "prompt must contain at least one block",
            ));
        }
        self.agent.prompt(self.id.clone(), blocks, metadata).await
    }

    pub async fn set_model(&self, model: impl Into<String>) -> Result<(), Error> {
        self.set_model_with_metadata(model, serde_json::Map::new())
            .await
    }

    /// Switch model with upstream metadata, including reasoning effort.
    pub async fn set_model_with_metadata(
        &self,
        model: impl Into<String>,
        metadata: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), Error> {
        self.agent
            .set_model(self.id.clone(), model.into(), metadata)
            .await
    }

    /// Switch Grok Build's session mode (`default`, `plan`, or `ask`).
    pub async fn set_mode(&self, mode: impl Into<String>) -> Result<(), Error> {
        self.agent.set_mode(self.id.clone(), mode.into()).await
    }

    /// Invoke a session-scoped `x.ai/*` extension. The session ID is injected
    /// into the object payload.
    pub async fn extension(
        &self,
        method: impl Into<String>,
        mut params: serde_json::Value,
    ) -> Result<serde_json::Value, Error> {
        let object = params.as_object_mut().ok_or_else(|| {
            Error::invalid_config("session extension parameters must be a JSON object")
        })?;
        object.insert(
            "sessionId".into(),
            serde_json::Value::String(self.id.0.clone()),
        );
        self.agent.extension(method, params).await
    }

    /// Set a manual persisted title through Grok Build's native rename path.
    pub async fn rename(&self, title: impl Into<String>) -> Result<(), Error> {
        let response = self
            .extension(
                "x.ai/session/rename",
                serde_json::json!({ "title": title.into() }),
            )
            .await?;
        if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
            return Err(Error::Operation(error.to_string()));
        }
        Ok(())
    }

    pub async fn cancel(&self) -> Result<(), Error> {
        self.cancel_with_metadata(serde_json::Map::new()).await
    }

    /// Cancel with upstream metadata such as `cancelSubagents`,
    /// `rewindIfNoOutput`, or a specific `promptId`.
    pub async fn cancel_with_metadata(
        &self,
        metadata: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), Error> {
        self.agent.cancel(self.id.clone(), metadata).await
    }

    pub async fn close(&self) -> Result<(), Error> {
        self.agent.close(self.id.clone()).await
    }
}

impl SessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SessionId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl SessionConfig {
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MediaConfig, MediaProviderConfig, ModelConfig, ProviderConfig};

    fn config() -> AgentConfig {
        AgentConfig::new(ModelConfig::new(
            "default",
            ProviderConfig::openai_responses("https://model.example/v1", "key", "model"),
        ))
    }

    #[test]
    fn media_tools_are_disabled_without_an_explicit_provider() {
        let (grok, _) = grok_config(&config()).expect("Grok config");
        assert!(grok.compat_resolved.hermetic);
        assert_eq!(grok.requirements.image_gen.pinned(), Some(false));
        assert_eq!(grok.requirements.image_edit.pinned(), Some(false));
        assert_eq!(grok.requirements.video_gen.pinned(), Some(false));
    }

    #[test]
    fn auxiliary_agent_work_defaults_to_the_sdk_model_without_disabling_web_fetch() {
        let (grok, _) = grok_config(&config()).expect("Grok config");
        assert_eq!(grok.web_search_model, "");
        assert!(!grok.disable_web_search);
        assert_eq!(grok.session_summary_model.as_deref(), Some("default"));
        assert_eq!(grok.image_description_model.as_deref(), Some("default"));
        assert_eq!(
            grok.prompt_suggest_model_pin,
            PromptSuggestModelPin::Pinned("default".into())
        );
    }

    #[test]
    fn auxiliary_agent_work_can_use_independent_catalog_models() {
        let config = config()
            .model(ModelConfig::new(
                "search",
                ProviderConfig::openai_responses(
                    "https://search.example/v1",
                    "search-key",
                    "search-model",
                ),
            ))
            .web_search_model("search")
            .session_summary_model("search")
            .image_description_model("search")
            .prompt_suggestion_model("search");
        let (grok, models) = grok_config(&config).expect("Grok config");
        assert_eq!(grok.web_search_model, "search");
        assert_eq!(grok.session_summary_model.as_deref(), Some("search"));
        assert_eq!(grok.image_description_model.as_deref(), Some("search"));
        assert_eq!(
            grok.prompt_suggest_model_pin,
            PromptSuggestModelPin::Pinned("search".into())
        );
        assert_eq!(models["search"].info.base_url, "https://search.example/v1");
    }

    #[test]
    fn media_provider_routes_upstream_tools_and_gates_operations() {
        let media = MediaConfig::new(
            MediaProviderConfig::new("https://media.example/v1", "media-key")
                .header("x-media-tenant", "tenant"),
        )
        .image_edit(false)
        .image_generation_model("image-model")
        .image_edit_model("edit-model");
        let (grok, _) = grok_config(&config().media(media)).expect("Grok config");

        let provider = grok.imagine_provider.expect("media provider");
        assert_eq!(provider.base_url, "https://media.example/v1");
        assert_eq!(provider.api_key, "media-key");
        assert_eq!(
            provider
                .extra_headers
                .get("x-media-tenant")
                .map(String::as_str),
            Some("tenant")
        );
        assert_eq!(grok.requirements.image_gen.pinned(), Some(true));
        assert_eq!(grok.requirements.image_edit.pinned(), Some(false));
        assert_eq!(grok.requirements.video_gen.pinned(), Some(true));
        assert_eq!(
            grok.features.image_gen_model_override.as_deref(),
            Some("image-model")
        );
        assert_eq!(
            grok.features.image_edit_model_override.as_deref(),
            Some("edit-model")
        );
    }
}
