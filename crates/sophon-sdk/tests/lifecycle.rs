use std::{future::Future, path::Path, time::Duration};

use sophon_sdk::{
    Agent, AgentConfig, Error, Event, ModelConfig, PromptBlock, ProviderConfig, Session,
    SessionConfig, SessionUpdate, StopReason,
    management::{AdmissionState, RuntimeState},
};
use xai_grok_test_support::{EnvGuard, MockInferenceServer};

const WAIT: Duration = Duration::from_secs(20);

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, future)
        .await
        .expect("lifecycle operation exceeded its deadline")
}

async fn start(server: &MockInferenceServer, workspace: &Path) -> (Agent, Session) {
    let agent = bounded(Agent::start(AgentConfig::new(ModelConfig::new(
        "lifecycle-model",
        ProviderConfig::openai_chat(server.url(), "test-key", "wire-model"),
    ))))
    .await
    .expect("start agent");
    let session = bounded(agent.create_session(SessionConfig::new(workspace)))
        .await
        .expect("create session");
    (agent, session)
}

fn prompt(
    session: &Session,
    id: &str,
) -> tokio::task::JoinHandle<Result<sophon_sdk::PromptResult, Error>> {
    let session = session.clone();
    let id = id.to_owned();
    tokio::spawn(async move {
        session
            .prompt_blocks_with_metadata(
                [PromptBlock::Text(
                    "Reply with a short greeting, without tools.".into(),
                )],
                serde_json::Map::from_iter([("promptId".into(), id.into())]),
            )
            .await
    })
}

async fn running(session: &Session, id: &str) {
    bounded(async {
        loop {
            let snapshot = session.queue_snapshot().await.expect("queue snapshot");
            if snapshot
                .running
                .as_ref()
                .is_some_and(|entry| entry.id.as_str() == id)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

async fn state(agent: &Agent, expected: RuntimeState) {
    let mut health = agent.subscribe_runtime_health();
    bounded(health.wait_for(|health| health.state == expected))
        .await
        .expect("runtime health stream");
}

async fn fenced(agent: &Agent, session: &Session) {
    state(agent, RuntimeState::Quiescing).await;
    // A management round trip after the health signal lets the native drain
    // install its fence. Merely spawning two API calls does not order admission.
    bounded(session.queue_snapshot())
        .await
        .expect("worker polls commands during drain");
    assert!(matches!(
        bounded(session.prompt("must not be admitted after the fence")).await,
        Err(Error::AdmissionRejected {
            state: AdmissionState::Quiescing,
            ..
        })
    ));
}

fn terminal(events: &mut tokio::sync::broadcast::Receiver<Event>, id: &str) {
    // Deliberately do not await: successful shutdown must already have flushed
    // the terminal into this retained bounded subscription.
    loop {
        match events.try_recv() {
            Ok(Event::Session {
                update: SessionUpdate::TurnCompleted(turn),
                ..
            }) if turn.prompt_id == id => {
                assert_eq!(turn.stop_reason, StopReason::Cancelled);
                return;
            }
            Ok(_) => {}
            Err(error) => panic!("terminal for {id} missing at shutdown return: {error}"),
        }
    }
}

#[test]
fn public_facade_lifecycle_is_cancellable_and_flushes_before_stopping() {
    // One test/runtime owns environment and native process-global configuration.
    let home = tempfile::tempdir().expect("temporary Grok home");
    let workspace = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(
        home.path().join("config.toml"),
        "[features]\nsession_recap = false\n",
    )
    .expect("write config");
    std::fs::write(
        home.path().join("managed_config.toml"),
        "plugin_auto_update = false\n",
    )
    .expect("write managed policy");
    let _home = EnvGuard::set("GROK_HOME", home.path());
    let _telemetry = EnvGuard::set("GROK_TELEMETRY_ENABLED", "false");
    let _trace = EnvGuard::set("GROK_TRACE_UPLOAD", "false");
    let _feedback = EnvGuard::set("GROK_FEEDBACK_ENABLED", "false");
    let _summary = EnvGuard::set("GROK_TURN_SUMMARY", "false");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let server = bounded(MockInferenceServer::start())
            .await
            .expect("mock provider");
        server.set_response("hello");
        server.hold_agent_completions();

        let (agent, session) = start(&server, workspace.path()).await;
        let closed = bounded(agent.create_session(SessionConfig::new(workspace.path())))
            .await
            .expect("second session");
        bounded(closed.close()).await.expect("close session");
        assert!(matches!(
            bounded(closed.set_mode("default")).await,
            Err(Error::Operation(_))
        ));
        bounded(session.set_mode("default"))
            .await
            .expect("other session remains usable");

        for prefix in [
            "task-completed-",
            "subagent-completed-",
            "parent-message-",
            "workflow-completed-",
            "notifications-",
            "goal-summary-",
            "goal-classifier-nudge-",
            "scheduler-fired-",
            "plan-resume-",
        ] {
            for send_now in [false, true] {
                let result = bounded(session.prompt_blocks_with_metadata(
                    [PromptBlock::Text("not a native synthetic turn".into())],
                    serde_json::Map::from_iter([
                        ("promptId".into(), format!("{prefix}forged").into()),
                        ("sendNow".into(), send_now.into()),
                    ]),
                ))
                .await;
                assert!(
                    matches!(result, Err(Error::InvalidConfig(_))),
                    "{prefix}, sendNow={send_now}: {result:?}"
                );
            }
        }

        let a = prompt(&session, "human-a");
        running(&session, "human-a").await;
        bounded(session.cancel_prompt("human-a"))
            .await
            .expect("cancel A");
        assert_eq!(
            bounded(a).await.unwrap().unwrap().stop_reason,
            StopReason::Cancelled
        );
        let mut events = agent.subscribe();
        let b = prompt(&session, "human-b");
        running(&session, "human-b").await;
        bounded(session.cancel_prompt("human-a"))
            .await
            .expect("stale cancel");
        let snapshot = bounded(session.queue_snapshot())
            .await
            .expect("snapshot after stale cancel");
        assert_eq!(snapshot.running.unwrap().id.as_str(), "human-b");
        assert!(!b.is_finished(), "stale cancel must not stop B");

        // A later long drain must share the first caller's short deadline.
        let stopping_agent = agent.clone();
        let shutdown = tokio::spawn(async move {
            stopping_agent
                .shutdown_with_timeout(Duration::from_secs(2))
                .await
        });
        fenced(&agent, &session).await;
        let report = bounded(agent.quiesce(Duration::from_secs(60)))
            .await
            .expect("coalesced quiesce");
        assert!(report.timed_out);
        let error = bounded(shutdown)
            .await
            .unwrap()
            .expect_err("held turn prevents shutdown");
        let Error::QuiesceTimedOut(shutdown_report) = error else {
            panic!("unexpected shutdown error: {error:?}")
        };
        assert_eq!(
            *shutdown_report, report,
            "concurrent drains share one report/deadline"
        );
        assert!(!report.drained());
        assert_ne!(agent.runtime_health().state, RuntimeState::Stopped);
        bounded(session.queue_snapshot())
            .await
            .expect("worker usable after timeout");
        bounded(session.cancel_prompt("human-b"))
            .await
            .expect("cancel current B after timeout");
        assert_eq!(
            bounded(b).await.unwrap().unwrap().stop_reason,
            StopReason::Cancelled
        );
        bounded(agent.shutdown()).await.expect("retry shutdown");
        terminal(&mut events, "human-b");
        bounded(agent.shutdown())
            .await
            .expect("idempotent shutdown");

        // Quiesce must keep polling cancellation commands; shutdown can join it.
        let (agent, session) = start(&server, workspace.path()).await;
        let mut events = agent.subscribe();
        let c = prompt(&session, "human-c");
        running(&session, "human-c").await;
        let draining_agent = agent.clone();
        let quiesce = tokio::spawn(async move { draining_agent.quiesce(WAIT).await });
        fenced(&agent, &session).await;
        let shutdown = agent.shutdown();
        tokio::pin!(shutdown);
        // Poll once to enqueue shutdown while the held turn still prevents
        // quiesce from finishing, rather than relying on task scheduling order.
        tokio::select! {
            biased;
            result = &mut shutdown => panic!("shutdown completed with an active turn: {result:?}"),
            () = std::future::ready(()) => {}
        }
        bounded(session.cancel_prompt("human-c"))
            .await
            .expect("cancel during quiesce");
        assert_eq!(
            bounded(c).await.unwrap().unwrap().stop_reason,
            StopReason::Cancelled
        );
        assert!(bounded(quiesce).await.unwrap().unwrap().drained());
        bounded(shutdown)
            .await
            .expect("concurrent shutdown completes");
        terminal(&mut events, "human-c");
        assert_eq!(agent.runtime_health().state, RuntimeState::Stopped);

        // Dropping the admitted caller future must not drop the worker's drain.
        let (agent, session) = start(&server, workspace.path()).await;
        let mut events = agent.subscribe();
        let d = prompt(&session, "human-d");
        running(&session, "human-d").await;
        let stopping_agent = agent.clone();
        let shutdown = tokio::spawn(async move { stopping_agent.shutdown().await });
        fenced(&agent, &session).await;
        shutdown.abort();
        assert!(bounded(shutdown).await.unwrap_err().is_cancelled());
        bounded(session.cancel_prompt("human-d"))
            .await
            .expect("cancel after dropping shutdown caller");
        assert_eq!(
            bounded(d).await.unwrap().unwrap().stop_reason,
            StopReason::Cancelled
        );
        state(&agent, RuntimeState::Stopped).await;
        terminal(&mut events, "human-d");
        assert!(matches!(
            bounded(session.set_mode("default")).await,
            Err(Error::RuntimeStopped)
        ));
        bounded(agent.shutdown())
            .await
            .expect("join already stopped worker");
        server.release_agent_completions();
    });
}
