//! Session-owned subagent lifecycle. Logical identities survive reactivation;
//! attempt identities are minted only by the native coordinator.
//!
//! Start and resume await the native result (including a possible foreground
//! budget handoff). Keep the request's logical ID to query or cancel concurrently.
//! Resume creates a new logical child from persisted history; reactivate starts a
//! new attempt of the same completed child. Message never reactivates a child.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{Error, Session, SessionId};

macro_rules! identity {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Adopt an opaque identity previously returned by the service.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

identity!(SubagentId);
identity!(AttemptId);

/// An exact activation. Mutations carrying this handle cannot affect a successor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentHandle {
    pub id: SubagentId,
    pub attempt_id: AttemptId,
}

/// Native lifecycle notification. An absent attempt denotes a pre-launch
/// rejection or a historical event without recorded attempt identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentEvent {
    pub parent_session_id: SessionId,
    pub id: SubagentId,
    pub attempt_id: Option<AttemptId>,
    pub kind: SubagentEventKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubagentEventKind {
    Spawned,
    Progress,
    Finished { state: SubagentState },
}

#[derive(Clone, Debug)]
pub struct SubagentStart {
    pub id: SubagentId,
    pub prompt: String,
    pub description: String,
    pub subagent_type: String,
    pub cwd: Option<std::path::PathBuf>,
    pub model: Option<String>,
}

impl SubagentStart {
    pub fn new(subagent_type: impl Into<String>, prompt: impl Into<String>) -> Self {
        let subagent_type = subagent_type.into();
        Self {
            id: SubagentId::new(uuid::Uuid::now_v7().to_string()),
            prompt: prompt.into(),
            description: subagent_type.clone(),
            subagent_type,
            cwd: None,
            model: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    Initializing,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct SubagentResult {
    pub id: SubagentId,
    /// None if rejected before launch or handed off while still queued.
    pub attempt_id: Option<AttemptId>,
    pub child_session_id: SessionId,
    pub state: SubagentState,
    pub output: String,
    pub error: Option<String>,
    pub turns: u32,
    pub tool_calls: u32,
}

impl SubagentResult {
    pub fn handle(&self) -> Option<SubagentHandle> {
        Some(SubagentHandle {
            id: self.id.clone(),
            attempt_id: self.attempt_id.clone()?,
        })
    }
}

#[derive(Clone, Debug)]
pub struct SubagentSnapshot {
    pub id: SubagentId,
    pub attempt_id: Option<AttemptId>,
    pub parent_session_id: SessionId,
    pub child_session_id: Option<SessionId>,
    pub subagent_type: String,
    pub description: String,
    pub state: SubagentState,
    pub started_at_epoch_ms: u64,
    pub duration_ms: u64,
    pub output: Option<String>,
    pub error: Option<String>,
    pub resumed_from: Option<SubagentId>,
}

impl SubagentSnapshot {
    pub fn handle(&self) -> Option<SubagentHandle> {
        Some(SubagentHandle {
            id: self.id.clone(),
            attempt_id: self.attempt_id.clone()?,
        })
    }
}

/// A running child discovered from the native coordinator, with live progress.
/// Identity and progress belong to the same observed attempt; the child may
/// finish or reactivate after this snapshot is taken.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunningSubagent {
    #[serde(rename = "subagentId")]
    pub id: SubagentId,
    pub attempt_id: Option<AttemptId>,
    #[serde(deserialize_with = "deserialize_session_id")]
    pub parent_session_id: SessionId,
    #[serde(deserialize_with = "deserialize_session_id")]
    pub child_session_id: SessionId,
    pub subagent_type: String,
    pub description: String,
    pub started_at_epoch_ms: u64,
    pub duration_ms: u64,
    pub turn_count: u32,
    pub tool_call_count: u32,
    pub tokens_used: u64,
    pub context_window_tokens: u64,
    pub context_usage_pct: u8,
    pub tools_used: Vec<String>,
    pub error_count: u32,
}

impl RunningSubagent {
    pub fn handle(&self) -> Option<SubagentHandle> {
        Some(SubagentHandle {
            id: self.id.clone(),
            attempt_id: self.attempt_id.clone()?,
        })
    }
}

fn deserialize_session_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<SessionId, D::Error> {
    String::deserialize(deserializer).map(SessionId)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubagentMessageMode {
    Queue,
    Steer,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SubagentMessageOutcome {
    Accepted {
        message_id: String,
    },
    Rejected,
    NotActive,
    UnsupportedContent,
    Limit {
        max_bytes: usize,
        observed_bytes: usize,
    },
    AdmissionUncertain,
    NotAcceptedBeforeDeadline,
    Saturated {
        max_in_flight: usize,
    },
    ChannelClosed,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubagentCancelOutcome {
    Cancelled,
    AlreadyFinished { status: SubagentState },
    NotFound,
}

#[derive(Debug, thiserror::Error)]
pub enum SubagentError {
    #[error(transparent)]
    Runtime(#[from] Error),
    #[error("subagent operation rejected: {0}")]
    Rejected(String),
    #[error("invalid subagent response: {0}")]
    InvalidResponse(#[from] serde_json::Error),
}

/// A view over the native coordinator, not an SDK registry.
#[derive(Clone)]
pub struct Subagents {
    session: Session,
}

impl Session {
    pub fn subagents(&self) -> Subagents {
        Subagents {
            session: self.clone(),
        }
    }
}

impl Subagents {
    /// Discover this session's running children without knowing their IDs.
    /// Terminal children are omitted. No SDK-side registry is consulted.
    pub async fn list_running(&self) -> Result<Vec<RunningSubagent>, SubagentError> {
        #[derive(Deserialize)]
        struct Response {
            subagents: Vec<RunningSubagent>,
        }
        let response: Response = self.call("list_running", json!({})).await?;
        Ok(response.subagents)
    }

    pub async fn start(&self, request: SubagentStart) -> Result<SubagentResult, SubagentError> {
        self.spawn("start", request, None, None).await
    }

    /// Create a new logical child from the source's persisted conversation.
    pub async fn resume(
        &self,
        source: &SubagentId,
        request: SubagentStart,
    ) -> Result<SubagentResult, SubagentError> {
        if source == &request.id {
            return Err(SubagentError::Rejected(
                "resume needs a new logical ID; use reactivate for the same child".into(),
            ));
        }
        self.spawn("resume", request, Some(source), None).await
    }

    /// Replace exactly this completed attempt; fails closed without persisted state.
    pub async fn reactivate(
        &self,
        previous: &SubagentHandle,
        prompt: impl Into<String>,
    ) -> Result<SubagentResult, SubagentError> {
        let request = SubagentStart {
            id: previous.id.clone(),
            prompt: prompt.into(),
            description: String::new(),
            subagent_type: String::new(),
            cwd: None,
            model: None,
        };
        self.spawn(
            "reactivate",
            request,
            Some(&previous.id),
            Some(&previous.attempt_id),
        )
        .await
    }

    pub async fn query(&self, id: &SubagentId) -> Result<Option<SubagentSnapshot>, SubagentError> {
        let wire: Option<QueryWire> = self.call("query", json!({"subagentId": id})).await?;
        Ok(wire.map(|wire| {
            let snapshot = wire.snapshot;
            SubagentSnapshot {
                id: snapshot.subagent_id,
                attempt_id: wire.attempt_id,
                parent_session_id: SessionId(snapshot.parent_session_id),
                child_session_id: (!snapshot.child_session_id.is_empty())
                    .then_some(SessionId(snapshot.child_session_id)),
                subagent_type: snapshot.subagent_type,
                description: snapshot.description,
                state: snapshot.status,
                started_at_epoch_ms: snapshot.started_at_epoch_ms,
                duration_ms: snapshot.duration_ms,
                output: snapshot.output,
                error: snapshot.failure_error.or(snapshot.cancel_reason),
                resumed_from: snapshot.resumed_from,
            }
        }))
    }

    /// Admit text to exactly this active attempt. Does not resume or reactivate.
    pub async fn message(
        &self,
        target: &SubagentHandle,
        text: impl Into<String>,
        mode: SubagentMessageMode,
    ) -> Result<SubagentMessageOutcome, SubagentError> {
        self.call(
            "message_attempt",
            json!({
                "subagentId": target.id, "expectedAttemptId": target.attempt_id,
                "text": text.into(), "queue": mode == SubagentMessageMode::Queue,
            }),
        )
        .await
    }

    pub async fn cancel(
        &self,
        target: &SubagentHandle,
    ) -> Result<SubagentCancelOutcome, SubagentError> {
        self.call(
            "cancel_attempt",
            json!({"subagentId": target.id, "expectedAttemptId": target.attempt_id}),
        )
        .await
    }

    async fn spawn(
        &self,
        operation: &str,
        request: SubagentStart,
        source: Option<&SubagentId>,
        expected: Option<&AttemptId>,
    ) -> Result<SubagentResult, SubagentError> {
        let wire: ResultWire = self
            .call(
                operation,
                json!({
                    "subagentId": request.id, "prompt": request.prompt,
                    "description": request.description, "subagentType": request.subagent_type,
                    "cwd": request.cwd, "model": request.model,
                    "resumeFrom": source, "expectedAttemptId": expected,
                }),
            )
            .await?;
        Ok(SubagentResult {
            id: wire.subagent_id,
            attempt_id: wire.attempt_id,
            child_session_id: SessionId(wire.child_session_id),
            state: wire.status,
            output: wire.output,
            error: wire.error,
            turns: wire.turns,
            tool_calls: wire.tool_calls,
        })
    }

    async fn call<T: DeserializeOwned>(
        &self,
        operation: &str,
        params: Value,
    ) -> Result<T, SubagentError> {
        let response = self
            .session
            .extension(format!("x.ai/subagent/{operation}"), params)
            .await?;
        decode(response)
    }
}

fn decode<T: DeserializeOwned>(response: Value) -> Result<T, SubagentError> {
    if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
        return Err(SubagentError::Rejected(
            error
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| error.to_string()),
        ));
    }
    // Null is a valid owned-query result, not a failed extension.
    Ok(serde_json::from_value(
        response.get("result").cloned().unwrap_or(response),
    )?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResultWire {
    subagent_id: SubagentId,
    attempt_id: Option<AttemptId>,
    child_session_id: String,
    status: SubagentState,
    output: String,
    error: Option<String>,
    turns: u32,
    tool_calls: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryWire {
    attempt_id: Option<AttemptId>,
    snapshot: SnapshotWire,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotWire {
    subagent_id: SubagentId,
    parent_session_id: String,
    child_session_id: String,
    subagent_type: String,
    description: String,
    status: SubagentState,
    started_at_epoch_ms: u64,
    duration_ms: u64,
    output: Option<String>,
    failure_error: Option<String>,
    cancel_reason: Option<String>,
    resumed_from: Option<SubagentId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_query_can_return_no_child() {
        assert!(
            decode::<Option<QueryWire>>(json!({"result": null}))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            decode::<Option<QueryWire>>(json!({"result": null, "error": "closed"})),
            Err(SubagentError::Rejected(_))
        ));
    }

    #[test]
    fn logical_and_attempt_identities_are_distinct() {
        let request = SubagentStart::new("general-purpose", "hello");
        let first = SubagentHandle {
            id: request.id.clone(),
            attempt_id: AttemptId::new("attempt_first"),
        };
        let successor = SubagentHandle {
            id: request.id,
            attempt_id: AttemptId::new("attempt_second"),
        };
        assert_eq!(first.id, successor.id);
        assert_ne!(first, successor);
    }
}
