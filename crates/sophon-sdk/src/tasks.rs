//! Durable, last-wins background-task event projections; not a task registry.
//!
//! Replace the previous snapshot for the session, including when `tasks` is
//! empty. A truncated snapshot also replaces the previous view, but is not a
//! complete inventory. Neither replay nor live recorded `Running` status proves
//! current process custody. Use the native session background-task list/kill
//! APIs for live operations; these rows intentionally contain no stdout.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use crate::SessionId;
use crate::management::{BackgroundTaskId, BackgroundTaskKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Live,
    Replay,
}

/// Status recorded by the native registry at snapshot creation, not a lease
/// on a running process. In particular, replay must not enable a kill action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordedStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: BackgroundTaskId,
    pub command: String,
    pub display_command: Option<String>,
    pub description: Option<String>,
    pub cwd: PathBuf,
    pub kind: BackgroundTaskKind,
    pub recorded_status: RecordedStatus,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub output_file: Option<PathBuf>,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub session_id: SessionId,
    pub tasks: Vec<Row>,
    pub truncated: bool,
    pub delivery: Delivery,
    /// Native envelope metadata, retained without inventing missing event,
    /// session or attempt identities. This is not process-custody evidence.
    pub metadata: Option<serde_json::Map<String, Value>>,
}

/// Decode native `SessionNotification` params (not a JSON-RPC wrapper).
/// Unknown updates and malformed snapshots return `None`, never an empty clear.
/// Decode all rows atomically: dropping an invalid row could erase a task.
pub(crate) fn decode_snapshot(event: &Value) -> Option<Snapshot> {
    use xai_grok_shell::extensions::notification::{BackgroundTaskRow, BackgroundTaskStatus};
    use xai_grok_tools::computer::types::TaskKind;

    #[derive(Deserialize)]
    struct Update {
        tasks: Vec<BackgroundTaskRow>,
        #[serde(default)]
        truncated: bool,
    }

    let session_id = SessionId::from(event.get("sessionId")?.as_str()?);
    let update = event.get("update")?;
    if update.get("sessionUpdate")?.as_str()? != "background_tasks" {
        return None;
    }
    let metadata = match event.get("_meta") {
        None | Some(Value::Null) => None,
        Some(Value::Object(meta)) => Some(meta.clone()),
        _ => return None,
    };
    let delivery = match metadata.as_ref().and_then(|meta| meta.get("isReplay")) {
        None | Some(Value::Bool(false)) => Delivery::Live,
        Some(Value::Bool(true)) => Delivery::Replay,
        _ => return None,
    };
    let update: Update = serde_json::from_value(update.clone()).ok()?;
    Some(Snapshot {
        session_id,
        tasks: update
            .tasks
            .into_iter()
            .map(|row| Row {
                id: BackgroundTaskId::new(row.task_id),
                command: row.command,
                display_command: row.display_command,
                description: row.description,
                cwd: PathBuf::from(row.cwd),
                kind: match row.kind {
                    TaskKind::Bash => BackgroundTaskKind::Command,
                    TaskKind::Monitor => BackgroundTaskKind::Monitor,
                },
                recorded_status: match row.status {
                    BackgroundTaskStatus::Running => RecordedStatus::Running,
                    BackgroundTaskStatus::Completed => RecordedStatus::Completed,
                    BackgroundTaskStatus::Failed => RecordedStatus::Failed,
                },
                started_at: row.started_at,
                ended_at: row.ended_at,
                output_file: row.output_file.map(PathBuf::from),
                exit_code: row.exit_code,
                signal: row.signal,
            })
            .collect(),
        truncated: update.truncated,
        delivery,
        metadata,
    })
}
