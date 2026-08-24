use sophon_sdk::{
    ScheduledTaskOccurrence, ScheduledTaskOccurrenceReceipt, ScheduledTaskRequest,
    ScheduledTaskSummary, ScheduledWakeSourceRequest, ScheduledWakeSourceSummary,
};

#[test]
fn scheduler_facade_carries_all_wake_sources_and_idempotency_identity() {
    let requests = [
        ScheduledWakeSourceRequest::Recurrence {
            interval: "5m".into(),
            recurring: true,
            fire_immediately: false,
        },
        ScheduledWakeSourceRequest::ExternalEvent {
            service: "github".into(),
            event: "pull_request.updated".into(),
            recurring: true,
        },
        ScheduledWakeSourceRequest::ProcessSettlement {
            process_id: "process-7".into(),
            command: "cargo test".into(),
        },
    ];
    for wake_source in requests {
        let request = ScheduledTaskRequest {
            task_id: None,
            prompt: Some("inspect wake source".into()),
            wake_source: Some(wake_source),
            durable: Some(true),
            foreground: Some(false),
        };
        let encoded = serde_json::to_value(&request).expect("scheduler request serializes");
        assert!(encoded.get("wakeSource").is_some());
        assert_eq!(
            serde_json::from_value::<ScheduledTaskRequest>(encoded)
                .expect("scheduler request round-trips"),
            request
        );
    }

    let summaries = [
        ScheduledWakeSourceSummary::Recurrence {
            interval_seconds: 300,
            recurring: true,
        },
        ScheduledWakeSourceSummary::ExternalEvent {
            service: "github".into(),
            event: "issue.updated".into(),
            recurring: true,
        },
        ScheduledWakeSourceSummary::ProcessSettlement {
            process_id: "process-8".into(),
            command: "cargo clippy".into(),
        },
    ];
    for wake_source in summaries {
        let encoded = serde_json::to_value(&wake_source).expect("wake summary serializes");
        assert_eq!(
            serde_json::from_value::<ScheduledWakeSourceSummary>(encoded)
                .expect("wake summary round-trips"),
            wake_source
        );
    }

    let summary = ScheduledTaskSummary {
        id: "task-event".into(),
        prompt: "inspect event".into(),
        wake_source: ScheduledWakeSourceSummary::ExternalEvent {
            service: "github".into(),
            event: "issue.updated".into(),
            recurring: true,
        },
        durable: true,
        foreground: false,
        created_at: "2026-08-24T00:00:00Z".into(),
        last_fired_at: None,
        expires_at: Some("2026-08-31T00:00:00Z".into()),
        last_subagent: None,
        next_fire_at: None,
    };
    assert_eq!(
        serde_json::from_value::<ScheduledTaskSummary>(
            serde_json::to_value(&summary).expect("task summary serializes")
        )
        .expect("task summary round-trips"),
        summary
    );

    let occurrence = ScheduledTaskOccurrence {
        task_id: "task-1".into(),
        occurrence: "provider-delivery-42".into(),
        detail: "pull request #42 changed".into(),
    };
    assert_eq!(
        serde_json::from_value::<ScheduledTaskOccurrence>(
            serde_json::to_value(&occurrence).expect("occurrence serializes")
        )
        .expect("occurrence round-trips"),
        occurrence
    );

    let receipt = ScheduledTaskOccurrenceReceipt {
        task_id: "task-1".into(),
        occurrence: "provider-delivery-42".into(),
        accepted: true,
    };
    assert_eq!(
        serde_json::from_value::<ScheduledTaskOccurrenceReceipt>(
            serde_json::to_value(&receipt).expect("occurrence receipt serializes")
        )
        .expect("occurrence receipt round-trips"),
        receipt
    );
}
