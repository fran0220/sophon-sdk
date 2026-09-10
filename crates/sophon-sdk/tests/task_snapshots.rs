// Include the projection directly so this focused suite also runs before the
// parent change wires the module into lib.rs/runtime.rs.
pub use sophon_sdk::{SessionId, management};
#[path = "../src/tasks.rs"]
mod tasks;

use serde_json::{Value, json};
use tasks::{Delivery, RecordedStatus, decode_snapshot};
use xai_grok_shell::extensions::notification::{
    BackgroundTaskRow, BackgroundTaskStatus, SessionNotification, SessionUpdate,
};
use xai_grok_tools::computer::types::TaskKind;

fn native_fixture(status: BackgroundTaskStatus, kind: TaskKind) -> Value {
    serde_json::to_value(SessionNotification {
        session_id: agent_client_protocol::SessionId::new("session-one"),
        update: SessionUpdate::BackgroundTasks {
            tasks: vec![BackgroundTaskRow {
                task_id: "task-one".into(),
                command: "sleep 10".into(),
                display_command: Some("wait".into()),
                description: Some("wait for service".into()),
                cwd: "/workspace".into(),
                kind,
                status,
                started_at: "2026-09-10T00:00:00Z".into(),
                ended_at: Some("2026-09-10T00:00:10Z".into()),
                output_file: Some("/tmp/task.log".into()),
                exit_code: Some(7),
                signal: Some("SIGTERM".into()),
            }],
            truncated: false,
        },
        meta: None,
    })
    .unwrap()
}

#[test]
fn native_serialization_projects_every_row_field_without_recomputing_status() {
    for (native, expected) in [
        (BackgroundTaskStatus::Running, RecordedStatus::Running),
        (BackgroundTaskStatus::Completed, RecordedStatus::Completed),
        (BackgroundTaskStatus::Failed, RecordedStatus::Failed),
    ] {
        let mut event = native_fixture(native, TaskKind::Monitor);
        event["update"]["tasks"][0]["stdout"] = json!("must not leak");
        let snapshot = decode_snapshot(&event).unwrap();
        assert_eq!(snapshot.session_id, SessionId::from("session-one"));
        assert_eq!(snapshot.delivery, Delivery::Live);
        assert!(!snapshot.truncated);
        assert_eq!(snapshot.metadata, None);
        assert_eq!(
            snapshot.tasks,
            vec![tasks::Row {
                id: management::BackgroundTaskId::new("task-one"),
                command: "sleep 10".into(),
                display_command: Some("wait".into()),
                description: Some("wait for service".into()),
                cwd: "/workspace".into(),
                kind: management::BackgroundTaskKind::Monitor,
                recorded_status: expected,
                started_at: "2026-09-10T00:00:00Z".into(),
                ended_at: Some("2026-09-10T00:00:10Z".into()),
                output_file: Some("/tmp/task.log".into()),
                exit_code: Some(7),
                signal: Some("SIGTERM".into()),
            }]
        );
        assert!(!format!("{snapshot:?}").contains("must not leak"));
    }
}

#[test]
fn replay_and_empty_full_snapshots_replace_instead_of_merge() {
    let mut event = native_fixture(BackgroundTaskStatus::Running, TaskKind::Bash);
    event["_meta"] = json!({"isReplay": true, "eventId": "event-one",
        "attemptId": "optional-attempt", "sessionId": "optional-session"});
    event["update"]["truncated"] = json!(true);
    let mut view = decode_snapshot(&event).unwrap();
    assert_eq!(view.delivery, Delivery::Replay);
    assert_eq!(view.tasks[0].recorded_status, RecordedStatus::Running);
    assert_eq!(view.tasks[0].kind, management::BackgroundTaskKind::Command);
    assert!(view.truncated);
    assert_eq!(
        view.metadata.as_ref().unwrap()["attemptId"],
        "optional-attempt"
    );
    assert_eq!(
        view.metadata.as_ref().unwrap()["sessionId"],
        "optional-session"
    );
    for replay in [true, false] {
        let empty = serde_json::to_value(SessionNotification {
            session_id: agent_client_protocol::SessionId::new("session-one"),
            update: SessionUpdate::BackgroundTasks {
                tasks: vec![],
                truncated: false,
            },
            meta: Some(json!({"isReplay": replay})),
        })
        .unwrap();
        view = decode_snapshot(&empty).unwrap();
        assert!(view.tasks.is_empty());
        assert!(!view.truncated);
        assert_eq!(
            view.delivery,
            if replay {
                Delivery::Replay
            } else {
                Delivery::Live
            }
        );
    }
}

#[test]
fn malformed_or_unknown_events_do_not_clear_or_partially_replace() {
    let good = native_fixture(BackgroundTaskStatus::Running, TaskKind::Bash);
    for (pointer, value) in [
        ("/update/sessionUpdate", json!("future_snapshot")),
        ("/update/tasks", Value::Null),
        (
            "/update/tasks",
            json!([good["update"]["tasks"][0].clone(), {}]),
        ),
        ("/update/tasks/0/status", json!("future_status")),
        ("/update/tasks/0/kind", json!("future_kind")),
        ("/update/tasks/0/exit_code", json!("bad")),
        ("/sessionId", json!(3)),
    ] {
        let mut event = good.clone();
        *event.pointer_mut(pointer).unwrap() = value;
        assert!(decode_snapshot(&event).is_none(), "{pointer}");
    }
    for event in [
        json!({"sessionId":"s", "update":{"sessionUpdate":"background_tasks"}}),
        json!({"sessionId":"s", "update":{"sessionUpdate":"background_tasks","tasks":[],"truncated":"false"}}),
        json!({"sessionId":"s", "_meta":{"isReplay":"true"}, "update":{"sessionUpdate":"background_tasks","tasks":[]}}),
        json!({"sessionId":"s", "_meta":[], "update":{"sessionUpdate":"background_tasks","tasks":[]}}),
    ] {
        assert!(decode_snapshot(&event).is_none());
    }
}

#[test]
fn historical_optional_fields_can_be_absent() {
    let mut event = native_fixture(BackgroundTaskStatus::Completed, TaskKind::Bash);
    let row = event["update"]["tasks"][0].as_object_mut().unwrap();
    for key in [
        "display_command",
        "description",
        "ended_at",
        "output_file",
        "exit_code",
        "signal",
    ] {
        row.remove(key);
    }
    let snapshot = decode_snapshot(&event).unwrap();
    let row = &snapshot.tasks[0];
    assert_eq!(
        (
            &row.display_command,
            &row.description,
            &row.ended_at,
            &row.output_file,
            &row.exit_code,
            &row.signal
        ),
        (&None, &None, &None, &None, &None, &None)
    );
    assert!(snapshot.metadata.is_none());
}
