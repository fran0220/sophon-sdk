use std::time::Duration;

use sophon_sdk::{
    Agent, AgentConfig, ModelConfig, ProviderConfig, SessionConfig,
    management::{
        ManagementErrorKind, ManagementEventKind, OperationId, QueueEntrySource, QueueSnapshot,
        ScheduledTaskCreate, ScheduledTaskEvent, ScheduledTaskUpdate, SchedulerMutationResult,
    },
    subagent::SubagentState,
};
use xai_grok_test_support::{EnvGuard, MockInferenceServer};

fn create(recurring: bool, fire_immediately: bool) -> ScheduledTaskCreate {
    ScheduledTaskCreate {
        interval_secs: 3600,
        prompt: "Reply briefly without tools: scheduler child completed.".into(),
        recurring,
        durable: false,
        fire_immediately,
    }
}

// Native completion wakes the parent; execution itself must not use the old
// scheduler foreground ingress. Only native completion IDs are allowed here.
fn assert_native_completion_only(snapshot: &QueueSnapshot) {
    for (id, source) in snapshot
        .running
        .iter()
        .map(|entry| (&entry.id, entry.source))
        .chain(
            snapshot
                .pending
                .iter()
                .map(|entry| (&entry.id, entry.source)),
        )
    {
        assert_eq!(source, QueueEntrySource::Internal);
        assert!(
            id.as_str().starts_with("subagent-completed-"),
            "{snapshot:?}"
        );
    }
}

#[test]
fn public_scheduler_runs_native_children_with_native_completion_wakes() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("GROK_HOME", home.path());
    let _telemetry = EnvGuard::set("GROK_TELEMETRY_ENABLED", "false");
    let _trace = EnvGuard::set("GROK_TRACE_UPLOAD", "false");
    let _feedback = EnvGuard::set("GROK_FEEDBACK_ENABLED", "false");
    let _summary = EnvGuard::set("GROK_TURN_SUMMARY", "false");
    std::fs::write(
        home.path().join("config.toml"),
        "[features]\nsession_recap = false\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join("managed_config.toml"),
        "plugin_auto_update = false\n",
    )
    .unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(90), async {
                let server = MockInferenceServer::start().await.unwrap();
                server.set_response("scheduler child completed");
                let agent = Agent::start(AgentConfig::new(ModelConfig::new(
                    "scheduler-model",
                    ProviderConfig::openai_chat(server.url(), "test-key", "wire-model"),
                )))
                .await
                .unwrap();
                let parent = agent
                    .create_session(SessionConfig::new(workspace.path()))
                    .await
                    .unwrap();
                let mut events = agent.subscribe_management();
                let initial = parent.scheduler_snapshot().await.unwrap();
                assert!(initial.tasks.is_empty());
                let fifo = parent.queue_snapshot().await.unwrap();
                assert!(fifo.running.is_none() && fifo.pending.is_empty());

                // Zero is rejected, not silently promoted to sixty seconds.
                let mut invalid = create(false, false);
                invalid.interval_secs = 0;
                let error = parent
                    .create_scheduled_task(
                        OperationId::new("invalid-create"),
                        initial.version.clone(),
                        invalid,
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.kind, ManagementErrorKind::InvalidRequest);
                assert_eq!(parent.scheduler_snapshot().await.unwrap(), initial);

                // A future occurrence keeps CAS/idempotency checks independent of firing.
                let operation = OperationId::new("create-future");
                let request = create(true, false);
                let result = parent
                    .create_scheduled_task(operation.clone(), initial.version.clone(), request.clone())
                    .await
                    .unwrap();
                let SchedulerMutationResult::Committed { value: task, version, replayed: false, .. } = result else {
                    panic!("expected committed create: {result:?}");
                };
                let replay = parent
                    .create_scheduled_task(operation.clone(), initial.version.clone(), request.clone())
                    .await
                    .unwrap();
                assert!(matches!(replay, SchedulerMutationResult::Committed { value, version: replay_version, replayed: true, .. } if value == task && replay_version == version));
                let mut different = request;
                different.prompt.push_str(" changed");
                assert_eq!(parent.create_scheduled_task(operation, initial.version.clone(), different).await.unwrap_err().kind, ManagementErrorKind::OperationIdReused);
                let conflict = parent.delete_scheduled_task(OperationId::new("stale-delete"), initial.version, task.id.clone()).await.unwrap();
                assert!(matches!(conflict, SchedulerMutationResult::Conflict { snapshot, .. } if snapshot.version == version && snapshot.tasks == vec![task.clone()]));
                let before = parent.scheduler_snapshot().await.unwrap();
                let error = parent.update_scheduled_task(OperationId::new("invalid-update"), version.clone(), ScheduledTaskUpdate { id: task.id.clone(), prompt: None, interval_secs: Some(0) }).await.unwrap_err();
                assert_eq!(error.kind, ManagementErrorKind::InvalidRequest);
                assert_eq!(parent.scheduler_snapshot().await.unwrap(), before);
                // Native numeric intervals below sixty are legal and must remain exact.
                let updated = parent.update_scheduled_task(OperationId::new("short-interval"), version, ScheduledTaskUpdate { id: task.id.clone(), prompt: None, interval_secs: Some(59) }).await.unwrap();
                let SchedulerMutationResult::Committed { value, version, .. } = updated else { panic!("update: {updated:?}"); };
                assert_eq!(value.interval_secs, 59);
                assert!(matches!(parent.delete_scheduled_task(OperationId::new("delete-future"), version, task.id).await.unwrap(), SchedulerMutationResult::Committed { value: true, .. }));

                for recurring in [false, true] {
                    let before = parent.scheduler_snapshot().await.unwrap();
                    assert!(before.tasks.is_empty());
                    let result = parent.create_scheduled_task(OperationId::new(format!("immediate-{recurring}")), before.version, create(recurring, true)).await.unwrap();
                    let SchedulerMutationResult::Committed { value: task, .. } = result else { panic!("create: {result:?}"); };
                    assert_eq!(task.recurring, recurring);
                    let child_id = loop {
                        let event = events.recv().await.expect("management stream must not lag");
                        match event.kind {
                            ManagementEventKind::Queue(snapshot) if snapshot.session_id == fifo.session_id => {
                                assert_native_completion_only(&snapshot);
                            }
                            ManagementEventKind::Scheduler { session_id, task_id, occurrence: ScheduledTaskEvent::Fired { subagent_id }, .. }
                                if session_id == fifo.session_id && task_id == task.id => {
                                    break subagent_id.expect("immediate occurrence must launch a native child");
                                }
                            _ => {}
                        }
                    };
                    let child = loop {
                        if let Some(child) = parent.subagents().query(&child_id).await.unwrap() {
                            match child.state {
                                SubagentState::Completed => break child,
                                SubagentState::Initializing | SubagentState::Running => {}
                                _ => panic!("scheduled child did not complete: {child:?}"),
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    };
                    assert_eq!(child.parent_session_id, fifo.session_id);
                    assert!(child.attempt_id.is_some());
                    assert_ne!(child.child_session_id.as_ref().unwrap(), &fifo.session_id);
                    assert!(child.output.as_deref().unwrap_or_default().contains("scheduler child completed"), "{child:?}");
                    let after = parent.scheduler_snapshot().await.unwrap();
                    if recurring {
                        assert_eq!(after.tasks.len(), 1);
                        let retained = &after.tasks[0];
                        assert_eq!(retained.id, task.id);
                        assert!(retained.recurring);
                        assert_eq!(retained.last_subagent_id.as_ref(), Some(&child_id));
                        assert!(retained.last_fired_at.is_some());
                        assert!(retained.next_fire_at.is_some());
                        assert!(matches!(parent.delete_scheduled_task(OperationId::new("delete-recurring"), after.version, task.id).await.unwrap(), SchedulerMutationResult::Committed { value: true, .. }));
                    } else {
                        assert!(after.tasks.is_empty(), "one-shot must retire after firing");
                    }
                    assert_native_completion_only(&parent.queue_snapshot().await.unwrap());
                }
                agent.quiesce(Duration::from_secs(10)).await.unwrap();
                while let Ok(event) = events.try_recv() {
                    if let ManagementEventKind::Queue(snapshot) = event.kind
                        && snapshot.session_id == fifo.session_id
                    {
                        assert_native_completion_only(&snapshot);
                    }
                }
                let drained = parent.queue_snapshot().await.unwrap();
                assert!(drained.running.is_none() && drained.pending.is_empty());
                agent.shutdown().await.unwrap();
            })
            .await
            .expect("scheduler management deadline");
        });
}
