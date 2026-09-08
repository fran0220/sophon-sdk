use serde_json::Value;

use crate::{SessionId, StopReason};

/// Streaming output emitted by the embedded Grok Build agent.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Event {
    /// Display-only history/live projection. Never drive prompt settlement from this variant.
    HistoryRecord(Box<HistoryRecord>),
    /// Ordered handoff delivered under the native capture admission fence.
    HistoryBoundary {
        session_id: SessionId,
        boundary_id: String,
    },
    /// Stable typed management state published in causal order with the raw
    /// Session or extension notification that produced it.
    Management(crate::management::ManagementEvent),
    Session {
        session_id: SessionId,
        update: SessionUpdate,
        /// Opaque metadata attached to the upstream notification envelope.
        metadata: Option<Value>,
    },
    /// Upstream xAI extension notification, preserved without an SDK mirror.
    Extension { method: String, payload: Value },
}

/// Stable projection of common session updates.
///
/// Updates added by upstream remain available through `Other` until callers
/// need a dedicated convenience variant.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum SessionUpdate {
    UserText(String),
    AssistantText(String),
    ThoughtText(String),
    ToolCall(Box<ToolCall>),
    ToolCallUpdate(Box<ToolCallUpdate>),
    Plan(Vec<PlanEntry>),
    /// Durable terminal for a prompt admitted without a [`crate::Session`]
    /// prompt future, such as a scheduler-owned foreground occurrence.
    TurnCompleted(TurnCompletion),
    Other(Value),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnCompletion {
    pub prompt_id: String,
    pub stop_reason: StopReason,
    pub agent_result: Option<String>,
    pub error_kind: Option<String>,
    pub usage: Option<TurnUsage>,
    pub elapsed_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub reasoning_tokens: u64,
    pub model_calls: u64,
    pub api_duration_ms: u64,
    pub cost_usd_ticks: Option<i64>,
    pub cost_is_partial: bool,
    pub turns: u64,
    pub incomplete: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub raw_input: Option<Value>,
    pub raw_output: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCallUpdate {
    pub id: String,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanEntry {
    pub content: String,
    pub priority: String,
    pub status: String,
}

/// One native record. Missing identity stays missing; no synthetic turns or IDs.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryRecord {
    pub session_id: SessionId,
    pub event_id: Option<String>,
    pub prompt_id: Option<String>,
    pub prompt_index: Option<u64>,
    pub hide_from_scrollback: bool,
    pub model: Option<String>,
    pub is_replay: bool,
    pub update: SessionUpdate,
    pub envelope_metadata: Option<Value>,
    pub chunk_metadata: Option<Value>,
}

/// Complete projection at an idle persistence cut, not a list of live turns.
/// Subscribe before requesting. Replace display from records, discard buffered
/// projection events through the matching boundary_id, then consume subsequent
/// records. Broadcast lag invalidates the handoff: resubscribe and resnapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct HistorySnapshot {
    pub session_id: SessionId,
    pub revision: String,
    pub boundary_id: String,
    /// Unfiltered native persistence order, including rewind markers.
    pub records: Vec<HistoryRecord>,
}
