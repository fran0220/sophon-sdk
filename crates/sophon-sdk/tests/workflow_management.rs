use std::time::Duration;

use sophon_sdk::workflow::{WorkflowRunStatus, WorkflowStartRequest, WorkflowStartSource};
use sophon_sdk::{Agent, AgentConfig, ModelConfig, ProviderConfig, SessionConfig};
use xai_grok_test_support::{EnvGuard, MockInferenceServer};

fn request() -> WorkflowStartRequest {
    WorkflowStartRequest {
        source: WorkflowStartSource::Script {
            script: "let meta = #{ name: \"sdk-flow\", description: \"test\" }; let r = agent(\"work\"); complete(r.output);".into(),
        },
        args: Some(serde_json::json!({"objective": "native SDK flow"})),
        agent_budget: Some(8),
    }
}

#[test]
fn native_workflow_authority_flow() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[workflows]\nenabled = true\n",
    )
    .unwrap();
    let _home = EnvGuard::set("GROK_HOME", home.path());
    let _enabled = EnvGuard::set("GROK_WORKFLOWS", "1");
    let _telemetry = EnvGuard::set("GROK_TELEMETRY_ENABLED", "false");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(60), async {
            let server = MockInferenceServer::start().await.unwrap();
            server.set_response("done");
            server.set_chunk_delay(Some(Duration::from_secs(5)));
            let agent = Agent::start(AgentConfig::new(ModelConfig::new(
                "workflow-test",
                ProviderConfig::openai_chat(server.url(), "test-key", "wire-model"),
            )))
            .await
            .unwrap();
            let session = agent
                .create_session(SessionConfig::new(workspace.path()))
                .await
                .unwrap();
            assert!(session.workflow_runs().await.unwrap().runs.is_empty());
            assert!(session.pause_workflow("").await.is_err());
            assert!(session.resume_workflow("missing", None).await.is_err());
            let mut invalid = request();
            invalid.agent_budget = Some(0);
            assert!(session.start_workflow(invalid).await.is_err());
            let started = session.start_workflow(request()).await.unwrap();
            assert!(
                session.pause_workflow(&started.name).await.is_err(),
                "names are not IDs"
            );
            session.pause_workflow(&started.run_id).await.unwrap();
            assert_eq!(
                session.workflow_runs().await.unwrap().runs[0].status,
                WorkflowRunStatus::UserPaused
            );
            // Retirement is native asynchronous work; resume may reject until it drains.
            loop {
                match session.resume_workflow(&started.run_id, None).await {
                    Ok(resumed) => {
                        assert_eq!(resumed.run_id, started.run_id);
                        break;
                    }
                    Err(error) if error.to_string().contains("retir") => {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                    Err(error) => panic!("resume: {error}"),
                }
            }
            session.stop_workflow(&started.run_id).await.unwrap();
            let runs = session.workflow_runs().await.unwrap().runs;
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].status, WorkflowRunStatus::Cancelled);
            assert!(runs[0].status.is_terminal());
            assert!(session.pause_workflow(&started.run_id).await.is_err());
            agent.quiesce(Duration::from_secs(15)).await.unwrap();
            assert!(
                session.start_workflow(request()).await.is_err(),
                "quiesce fences launch"
            );
            assert!(
                session
                    .resume_workflow(&started.run_id, None)
                    .await
                    .is_err(),
                "quiesce fences resume"
            );
            assert_eq!(session.workflow_runs().await.unwrap().runs.len(), 1);
            agent.shutdown().await.unwrap();
        })
        .await
        .expect("workflow management deadline");
    });
    drop(runtime);
    let _disabled = EnvGuard::set("GROK_WORKFLOWS", "0");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(30), async {
            let server = MockInferenceServer::start().await.unwrap();
            let agent = Agent::start(AgentConfig::new(ModelConfig::new(
                "workflow-disabled-test",
                ProviderConfig::openai_chat(server.url(), "test-key", "wire-model"),
            )))
            .await
            .unwrap();
            let session = agent
                .create_session(SessionConfig::new(workspace.path()))
                .await
                .unwrap();
            let error = session.start_workflow(request()).await.unwrap_err();
            assert!(error.to_string().contains("workflows_disabled"), "{error}");
            assert!(session.workflow_runs().await.unwrap().runs.is_empty());
            agent.shutdown().await.unwrap();
        })
        .await
        .expect("disabled workflow deadline");
    });
}

#[test]
fn typed_source_rejects_legacy_and_control_aliases() {
    for value in [
        serde_json::json!({"name": "legacy"}),
        serde_json::json!({"type":"pause", "run_id":"wf_1"}),
    ] {
        assert!(serde_json::from_value::<WorkflowStartSource>(value).is_err());
    }
    assert!(WorkflowRunStatus::Complete.is_terminal());
    assert!(!WorkflowRunStatus::BudgetLimited.is_terminal());
}
