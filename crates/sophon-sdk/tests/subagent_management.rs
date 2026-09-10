use std::{future::Future, time::Duration};

use sophon_sdk::{
    Agent, AgentConfig, ModelConfig, ProviderConfig, SessionConfig,
    subagent::{SubagentCancelOutcome, SubagentHandle, SubagentStart, SubagentState, Subagents},
};
use xai_grok_test_support::{EnvGuard, MockInferenceServer};

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .expect("subagent lifecycle operation timed out")
}

async fn successor(subagents: &Subagents, previous: &SubagentHandle) -> SubagentHandle {
    bounded(async {
        loop {
            if let Some(snapshot) = subagents.query(&previous.id).await.expect("query child")
                && snapshot.state == SubagentState::Running
                && let Some(handle) = snapshot.handle()
                && handle.attempt_id != previous.attempt_id
            {
                return handle;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
}

#[test]
fn typed_subagent_lifecycle_is_owned_attempt_authoritative_and_fenced() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("GROK_HOME", home.path());
    let _telemetry = EnvGuard::set("GROK_TELEMETRY_ENABLED", "false");
    let _trace = EnvGuard::set("GROK_TRACE_UPLOAD", "false");
    let _feedback = EnvGuard::set("GROK_FEEDBACK_ENABLED", "false");
    let _summary = EnvGuard::set("GROK_TURN_SUMMARY", "false");
    std::fs::write(
        home.path().join("config.toml"),
        "[features]\nsession_recap = false\nactive_agent_messages = true\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join("managed_config.toml"),
        "plugin_auto_update = false\n",
    )
    .unwrap();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let server = MockInferenceServer::start().await.unwrap();
            server.set_response("subagent completed successfully");
            let agent = bounded(Agent::start(AgentConfig::new(ModelConfig::new(
                "subagent-model",
                ProviderConfig::openai_chat(server.url(), "test-key", "wire-model"),
            ))))
            .await
            .unwrap();
            let parent = bounded(agent.create_session(SessionConfig::new(workspace.path())))
                .await
                .unwrap();
            let foreign = bounded(agent.create_session(SessionConfig::new(workspace.path())))
                .await
                .unwrap();
            let children = parent.subagents();
            let first = bounded(children.start(SubagentStart::new(
                "explore",
                "Reply briefly, without tools.",
            )))
            .await
            .unwrap();
            assert_eq!(first.state, SubagentState::Completed, "{first:?}");
            assert!(bounded(children.list_running()).await.unwrap().is_empty());
            let first_handle = first.handle().expect("coordinator attempt identity");
            assert!(
                bounded(foreign.subagents().query(&first.id))
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                bounded(foreign.subagents().cancel(&first_handle))
                    .await
                    .unwrap(),
                SubagentCancelOutcome::NotFound
            );

            let resumed = bounded(children.resume(
                &first.id,
                SubagentStart::new("explore", "Reply briefly again."),
            ))
            .await
            .unwrap();
            assert_eq!(resumed.state, SubagentState::Completed, "{resumed:?}");
            assert_ne!(resumed.id, first.id);
            assert_ne!(resumed.attempt_id, first.attempt_id);

            server.hold_agent_completions();
            let reactivation = tokio::spawn({
                let children = children.clone();
                let first = first_handle.clone();
                async move {
                    children
                        .reactivate(&first, "Continue with a short answer.")
                        .await
                }
            });
            let current = successor(&children, &first_handle).await;
            // A fresh view discovers children without having started or queried them.
            let running = bounded(parent.subagents().list_running()).await.unwrap();
            assert_eq!(running.len(), 1, "{running:?}");
            let child = &running[0];
            assert_eq!(child.handle(), Some(current.clone()));
            let snapshot = bounded(children.query(&child.id)).await.unwrap().unwrap();
            assert_eq!(child.parent_session_id, snapshot.parent_session_id);
            assert_eq!(
                Some(child.child_session_id.clone()),
                snapshot.child_session_id
            );
            assert_eq!(child.subagent_type, snapshot.subagent_type);
            assert_eq!(child.started_at_epoch_ms, snapshot.started_at_epoch_ms);
            assert!(child.context_usage_pct <= 100);
            assert!(
                bounded(foreign.subagents().list_running())
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                bounded(children.cancel(&first_handle)).await.is_err(),
                "stale attempt must conflict"
            );
            assert_eq!(
                bounded(foreign.subagents().cancel(&current)).await.unwrap(),
                SubagentCancelOutcome::NotFound
            );
            assert_eq!(
                bounded(children.query(&first.id))
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                SubagentState::Running
            );
            server.release_agent_completions();
            let completed = bounded(reactivation).await.unwrap().unwrap();
            assert_eq!(completed.state, SubagentState::Completed, "{completed:?}");
            assert_eq!(completed.handle(), Some(current.clone()));
            assert!(bounded(children.list_running()).await.unwrap().is_empty());

            server.hold_agent_completions();
            let cancelled = tokio::spawn({
                let children = children.clone();
                let current = current.clone();
                async move { children.reactivate(&current, "One last reply.").await }
            });
            let last = successor(&children, &current).await;
            let running = bounded(children.list_running()).await.unwrap();
            assert_eq!(running.len(), 1);
            assert_eq!(running[0].handle(), Some(last.clone()));
            assert_eq!(
                bounded(children.cancel(&last)).await.unwrap(),
                SubagentCancelOutcome::Cancelled
            );
            server.release_agent_completions();
            assert_eq!(
                bounded(cancelled).await.unwrap().unwrap().state,
                SubagentState::Cancelled
            );
            assert!(bounded(children.list_running()).await.unwrap().is_empty());

            bounded(agent.quiesce(Duration::from_secs(10)))
                .await
                .unwrap();
            assert!(
                bounded(children.start(SubagentStart::new("explore", "must be rejected")))
                    .await
                    .is_err()
            );
            assert!(
                bounded(children.reactivate(&last, "must also be rejected"))
                    .await
                    .is_err()
            );
            bounded(agent.shutdown()).await.unwrap();
        });
}
