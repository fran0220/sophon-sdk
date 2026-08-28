use serde_json::Value;

use crate::SessionId;

/// Streaming output emitted by the embedded Grok Build agent.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Event {
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
    Other(Value),
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
