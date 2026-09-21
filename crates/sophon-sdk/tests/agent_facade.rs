use std::time::Duration;

use sophon_sdk::{
    Agent, AgentConfig, ConfigCandidate, Event, ModelBehaviorConfig, ModelConfig, PromptBlock,
    PromptOptions, ProviderConfig, SessionConfig, SessionUpdate, StopReason,
};
use xai_grok_test_support::{EnvGuard, MockInferenceServer};

fn authored_options(revision: &str, instructions: &str, model: &str) -> PromptOptions {
    PromptOptions {
        config_candidate: Some(ConfigCandidate {
            revision: revision.into(),
            instructions: instructions.into(),
            model: model.into(),
            reasoning_effort: None,
            skill_directories: Vec::new(),
            external_mcp_servers: Vec::new(),
            subagent_briefs: Vec::new(),
        }),
        ..Default::default()
    }
}

#[test]
fn all_provider_protocols_execute_through_the_agent_facade() {
    let grok_home = tempfile::tempdir().expect("temporary Grok home");
    let workspace = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(
        grok_home.path().join("config.toml"),
        "[features]\nsession_recap = false\nweb_fetch = true\n",
    )
    .expect("write effective Grok config");
    std::fs::write(
        grok_home.path().join("managed_config.toml"),
        "plugin_auto_update = false\n",
    )
    .expect("write embedding-owned managed policy");
    let _grok_home = EnvGuard::set("GROK_HOME", grok_home.path());
    let _telemetry = EnvGuard::set("GROK_TELEMETRY_ENABLED", "false");
    let _trace_upload = EnvGuard::set("GROK_TRACE_UPLOAD", "false");
    let _feedback = EnvGuard::set("GROK_FEEDBACK_ENABLED", "false");
    let _turn_summary = EnvGuard::set("GROK_TURN_SUMMARY", "false");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        // Observe TCP accepts, not only inference routes: upstream prewarm is
        // an origin GET and would evade the mock provider's POST capture.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("prewarm probe listener");
        let agent = Agent::start(AgentConfig::new(ModelConfig::new(
            "no-prewarm",
            ProviderConfig::openai_chat(
                format!("http://{}/v1", listener.local_addr().unwrap()),
                "sdk-secret",
                "wire-model",
            ),
        )))
        .await
        .expect("start without provider traffic");
        let session = agent
            .create_session(SessionConfig::new(workspace.path()))
            .await
            .expect("create without provider traffic");
        assert!(
            tokio::time::timeout(Duration::from_millis(250), listener.accept())
                .await
                .is_err(),
            "hermetic startup must not initiate detached provider prewarming"
        );
        session.close().await.expect("close unprompted session");
        agent.shutdown().await.expect("stop unprompted agent");

        let server = MockInferenceServer::start().await.expect("mock provider");
        server.set_response("response from Grok Build");
        let suggestion_server = MockInferenceServer::start()
            .await
            .expect("mock prompt-suggestion provider");
        suggestion_server.set_response("inspect the tests");

        let providers = [
            (
                ProviderConfig::openai_chat(server.url(), "sdk-secret", "wire-model"),
                "/v1/chat/completions",
                "authorization",
                "Bearer sdk-secret",
            ),
            (
                ProviderConfig::openai_responses(server.url(), "sdk-secret", "wire-model"),
                "/v1/responses",
                "authorization",
                "Bearer sdk-secret",
            ),
            (
                ProviderConfig::anthropic(server.url(), "sdk-secret", "wire-model"),
                "/v1/messages",
                "x-api-key",
                "sdk-secret",
            ),
        ];

        for (provider, path, auth_header, auth_value) in providers {
            let request_start = server.requests().len();
            let provider = provider.header("x-sdk-test", "configured");
            let agent = Agent::start(
                AgentConfig::new(ModelConfig::new("sdk-model", provider.clone()))
                    .model(ModelConfig::new("sdk-other", provider.clone()))
                    .model(ModelConfig::new("sdk-concise", provider.clone()).behavior(
                        ModelBehaviorConfig {
                            use_concise: Some(true),
                            ..Default::default()
                        },
                    ))
                    .model(ModelConfig::new("sdk-codex", provider.clone()).behavior(
                        ModelBehaviorConfig {
                            agent_type: Some("codex".into()),
                            ..Default::default()
                        },
                    ))
                    .model(ModelConfig::new("sdk-codex-concise", provider).behavior(
                        ModelBehaviorConfig {
                            agent_type: Some("codex".into()),
                            use_concise: Some(true),
                            ..Default::default()
                        },
                    ))
                    .model(ModelConfig::new(
                        "suggestion-model",
                        ProviderConfig::openai_responses(
                            suggestion_server.url(),
                            "suggestion-secret",
                            "suggestion-wire-model",
                        )
                        .header("x-suggestion-tenant", "tenant")
                        .query_param("tenant", "suggestion"),
                    ))
                    // Keep title side-calls off the main inference capture.
                    .session_summary_model("suggestion-model")
                    .prompt_suggestion_model("suggestion-model"),
            )
            .await
            .expect("start agent");
            assert!(xai_grok_config::hermetic_discovery());
            assert!(xai_grok_config::system_config_dir().is_none());
            assert!(xai_grok_config::claude_managed_settings_path().is_none());
            assert!(xai_grok_config::claude_managed_settings_probe_path().is_none());
            assert!(!xai_grok_shell::managed_config::is_fetch_enabled());
            xai_grok_shell::managed_config::clear_orphan();
            let layers = xai_grok_config::managed_config_layers();
            assert_eq!(layers.len(), 1);
            assert_eq!(layers[0].path, grok_home.path().join("managed_config.toml"));
            assert!(!layers[0].is_system);
            assert!(
                xai_grok_config::requirements_layers()
                    .iter()
                    .all(|l| !l.is_system)
            );
            let policy = xai_grok_workspace::permission::managed_policy::managed_settings();
            assert!(policy.plugin_auto_update.is_disabled());
            assert_eq!(
                policy.plugin_auto_update.source(),
                Some(layers[0].path.as_path())
            );
            let model_state = agent.effective_config_snapshot();
            assert!(
                model_state
                    .routes
                    .iter()
                    .any(|route| route.route_id == "sdk-model"),
                "configured model missing from typed routes: {model_state:?}"
            );
            let mut events = agent.subscribe();
            let session = agent
                .create_session(SessionConfig::new(workspace.path()))
                .await
                .expect("create session");
            session.set_mode("default").await.expect("set session mode");
            let result = session
                .prompt_blocks_with_options(
                    [PromptBlock::Text("exercise the SDK facade".into())],
                    authored_options(
                        "original",
                        "ORIGINAL_ATTACH_RULES: retain this authored rule",
                        "sdk-model",
                    ),
                )
                .await
                .expect("prompt");
            assert_eq!(result.stop_reason, StopReason::EndTurn);
            assert!(result.prompt_id.is_some());
            assert_eq!(result.prompt_index, Some(0));

            let assistant_text = tokio::time::timeout(Duration::from_secs(5), async {
                let mut assistant_text = String::new();
                loop {
                    if let Event::Session {
                        session_id,
                        update: SessionUpdate::AssistantText(text),
                        ..
                    } = events.recv().await.expect("session event")
                        && session_id == *session.id()
                    {
                        assistant_text.push_str(&text);
                        if assistant_text.contains("response from Grok Build") {
                            break assistant_text;
                        }
                    }
                }
            })
            .await
            .expect("assistant event timeout");
            assert!(assistant_text.contains("response from Grok Build"));

            assert_eq!(
                model_state.auxiliary.prompt_suggestion_model.as_deref(),
                Some("suggestion-model")
            );

            let requests = server.requests();
            let request = requests[request_start..]
                .iter()
                .find(|request| request.path == path)
                .unwrap_or_else(|| panic!("no {path} request; got {requests:?}"));
            assert_eq!(
                request.header(auth_header),
                Some(auth_value),
                "wrong authentication for {path}: {:?}",
                request.headers
            );
            assert_eq!(request.header("x-sdk-test"), Some("configured"));
            assert_eq!(
                request
                    .body
                    .as_ref()
                    .and_then(|body| body["model"].as_str()),
                Some("wire-model")
            );

            session
                .rename("SDK facade test")
                .await
                .expect("rename session");
            let page = agent
                .list_sessions(Some(workspace.path()), None)
                .await
                .expect("list sessions");
            let listed = page
                .sessions
                .iter()
                .find(|listed| listed.id == *session.id())
                .unwrap_or_else(|| panic!("created session missing from list: {page:?}"));
            assert_eq!(listed.title.as_deref(), Some("SDK facade test"));

            let session_id = session.id().clone();
            // The catalog key is sdk-model while persistence uses wire-model.
            // Exercise both live reattach and actor eviction/reload, first without
            // an override (original rules survive), then with explicit replacement.
            for (cold, replace) in [(false, false), (true, false), (false, true), (true, true)] {
                if cold {
                    session.close().await.expect("evict before cold attach");
                }
                let config = SessionConfig::new(workspace.path());
                let attached = agent
                    .resume_session(session_id.clone(), config)
                    .await
                    .expect("attach authored session");
                if replace {
                    attached
                        .set_model("sdk-other")
                        .await
                        .expect("switch after attach");
                    attached
                        .set_model("sdk-concise")
                        .await
                        .expect("concise after attach");
                    attached
                        .set_model("sdk-model")
                        .await
                        .expect("restore model after attach");
                }
                let start = server.requests().len();
                attached
                    .prompt_blocks_with_options(
                        [PromptBlock::Text("verify attached prompt head".into())],
                        if replace {
                            authored_options(
                                "replacement",
                                "ADMISSION_ATTACH_OVERRIDE: replacement system prompt",
                                "sdk-model",
                            )
                        } else {
                            PromptOptions::default()
                        },
                    )
                    .await
                    .expect("attached turn");
                let requests = server.requests();
                let body = requests[start..]
                    .iter()
                    .find(|request| request.path == path)
                    .expect("attached inference request")
                    .body
                    .as_ref()
                    .unwrap()
                    .to_string();
                let expected = if replace {
                    "ADMISSION_ATTACH_OVERRIDE"
                } else {
                    "ORIGINAL_ATTACH_RULES"
                };
                assert!(
                    body.contains(expected),
                    "authored head lost: cold={cold}, replace={replace}, protocol={path}"
                );
                if replace {
                    assert!(
                        !body.contains("ORIGINAL_ATTACH_RULES"),
                        "override must replace, not append"
                    );
                }
            }
            session.close().await.expect("close session");
            let resumed = agent
                .resume_session(session_id, SessionConfig::new(workspace.path()))
                .await
                .expect("resume session");
            resumed.close().await.expect("close resumed session");
            verify_model_switch_prompts(&agent, &server, workspace.path(), path).await;
            agent.shutdown().await.expect("shutdown agent");
        }
    });
}

/// Runs the real facade, session command loop, harness builder and sampler.
/// Only the provider is mocked; assertions inspect the actual wire system head.
async fn verify_model_switch_prompts(
    agent: &Agent,
    server: &MockInferenceServer,
    workspace: &std::path::Path,
    path: &str,
) {
    const AUTHORED: &str =
        "CLIENT_AUTHORED: retain my exact instructions, including whitespace.\n ";
    const COMPACT: &str = "You are an AI coding agent. You operate in a workspace with a provided codebase.\n\nYour main goal is to complete the user's request, denoted within the <user_query> tag.";
    for explicit in [false, true] {
        for rebuild in [false, true] {
            let config = SessionConfig::new(workspace).model("sdk-model");
            let session = agent
                .create_session(config)
                .await
                .expect("model-switch session");
            let initial_agent = session.info().await.expect("initial harness").agent_name;
            assert_ne!(initial_agent.as_deref(), Some("codex"));
            let models = if rebuild {
                ["sdk-codex", "sdk-codex-concise", "sdk-codex"]
            } else {
                ["sdk-other", "sdk-concise", "sdk-model"]
            };
            let mut normal_head = None;
            for (step, model) in models.into_iter().enumerate() {
                // The first codex switch must rebuild the zero-turn harness.
                session
                    .set_model(model)
                    .await
                    .expect("explicit model switch");
                if rebuild {
                    assert_eq!(
                        session
                            .info()
                            .await
                            .expect("rebuilt harness")
                            .agent_name
                            .as_deref(),
                        Some("codex"),
                        "zero-turn model switch must actually rebuild the harness"
                    );
                }
                let start = server.requests().len();
                session
                    .prompt_blocks_with_options(
                        [PromptBlock::Text("verify model switch prompt".into())],
                        if explicit && step == 0 {
                            authored_options("authored", AUTHORED, model)
                        } else {
                            PromptOptions::default()
                        },
                    )
                    .await
                    .expect("model-switch turn");
                let requests = server.requests();
                let body = requests[start..]
                    .iter()
                    .find(|request| request.path == path)
                    .expect("model-switch inference request")
                    .body
                    .as_ref()
                    .unwrap();
                let head = match path {
                    "/v1/chat/completions" => {
                        body["messages"][0]["content"].as_str().unwrap().to_owned()
                    }
                    "/v1/responses" => body["input"][0]["content"].as_str().unwrap().to_owned(),
                    "/v1/messages" => body["system"][0]["text"].as_str().unwrap().to_owned(),
                    _ => unreachable!(),
                };
                if explicit {
                    assert_eq!(
                        head, AUTHORED,
                        "override lost: rebuild={rebuild}, model={model}, protocol={path}"
                    );
                } else if step == 1 {
                    assert_eq!(
                        head, COMPACT,
                        "unconfigured concise model must still use compact prompt"
                    );
                } else {
                    assert!(!head.is_empty());
                    assert_ne!(
                        head, COMPACT,
                        "normal model must restore its harness template"
                    );
                    assert!(!head.contains("CLIENT_AUTHORED"));
                    if let Some(original) = &normal_head {
                        assert_eq!(
                            &head, original,
                            "normal harness prompt changed after concise round trip"
                        );
                    } else {
                        normal_head = Some(head);
                    }
                }
            }
            session.close().await.expect("close model-switch session");
        }
    }
}
