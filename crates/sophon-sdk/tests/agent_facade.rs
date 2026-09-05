use std::time::Duration;

use sophon_sdk::{
    Agent, AgentConfig, Event, ModelConfig, ProviderConfig, SessionConfig, SessionUpdate,
    StopReason,
};
use xai_grok_test_support::{EnvGuard, MockInferenceServer};

#[test]
fn all_provider_protocols_execute_through_the_agent_facade() {
    let grok_home = tempfile::tempdir().expect("temporary Grok home");
    let workspace = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(
        grok_home.path().join("config.toml"),
        "[features]\nsession_recap = false\nweb_fetch = true\n",
    )
    .expect("write effective Grok config");
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
            let suggestion_request_start = suggestion_server.requests().len();
            let provider = provider.header("x-sdk-test", "configured");
            let agent = Agent::start(
                AgentConfig::new(ModelConfig::new("sdk-model", provider))
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
                    .prompt_suggestion_model("suggestion-model"),
            )
            .await
            .expect("start agent");
            assert!(
                agent
                    .initialization_response()
                    .get("agentCapabilities")
                    .is_some(),
                "initialization capabilities were not preserved"
            );
            assert_eq!(
                agent.initialization_response()["_meta"]["sessionRecap"],
                false,
                "effective GROK_HOME config was not preserved"
            );
            let model_state = agent
                .extension("x.ai/models/list", serde_json::json!({}))
                .await
                .expect("raw model extension");
            assert!(
                model_state.to_string().contains("sdk-model"),
                "configured model missing from extension response: {model_state}"
            );
            let mut events = agent.subscribe();
            let session = agent
                .create_session(SessionConfig::new(workspace.path()).metadata(
                    "rules",
                    serde_json::json!("ORIGINAL_ATTACH_RULES: retain this authored rule"),
                ))
                .await
                .expect("create session");
            assert!(
                session.initial_response().get("_meta").is_some(),
                "session initialization metadata was not preserved"
            );
            session.set_mode("default").await.expect("set session mode");
            let result = session
                .prompt("exercise the SDK facade")
                .await
                .expect("prompt");
            assert_eq!(result.stop_reason, StopReason::EndTurn);
            assert!(result.raw_response.get("_meta").is_some());

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

            let suggestion_response = session
                .extension("x.ai/suggestPrompt", serde_json::json!({ "generation": 1 }))
                .await
                .expect("prompt suggestion extension");
            let suggestion_requests = suggestion_server.requests();
            let suggestion_request = suggestion_requests[suggestion_request_start..]
                .iter()
                .find(|request| request.path == "/v1/responses")
                .unwrap_or_else(|| {
                    panic!(
                        "prompt suggestion did not use its provider; response: \
                         {suggestion_response}; requests: {suggestion_requests:?}"
                    )
                });
            assert_eq!(
                suggestion_request.header("authorization"),
                Some("Bearer suggestion-secret")
            );
            assert_eq!(
                suggestion_request.header("x-suggestion-tenant"),
                Some("tenant")
            );
            assert_eq!(
                suggestion_request
                    .body
                    .as_ref()
                    .and_then(|body| body["model"].as_str()),
                Some("suggestion-wire-model")
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
                let mut config = SessionConfig::new(workspace.path());
                if replace {
                    config = config.metadata(
                        "systemPromptOverride",
                        serde_json::json!("ADMISSION_ATTACH_OVERRIDE: replacement system prompt"),
                    );
                }
                let attached = agent
                    .resume_session(session_id.clone(), config)
                    .await
                    .expect("attach authored session");
                let start = server.requests().len();
                attached
                    .prompt("verify attached prompt head")
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
            agent.shutdown().await.expect("shutdown agent");
        }
    });
}
