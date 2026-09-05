use serde_json::Value;

use crate::{Error, SessionId};

/// One permission choice offered by Grok Build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    Other,
}

/// A tool execution decision requested by Grok Build.
#[derive(Clone, Debug, PartialEq)]
pub struct PermissionRequest {
    pub session_id: SessionId,
    pub tool_call: Value,
    pub options: Vec<PermissionOption>,
    pub metadata: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Select(String),
    Cancel,
}

/// Host callbacks for capabilities that require an embedding-side response.
///
/// This keeps ACP private while preserving permission prompts and Grok Build's
/// agent-to-client extension requests (for example ask-user and SDK MCP calls).
#[async_trait::async_trait]
pub trait ClientHandler: Send + Sync + 'static {
    /// The future is dropped when its requesting turn abandons the permission
    /// response. Keep this callback cancellation-safe. Independently spawned
    /// host tasks are not cancelled by dropping the callback future.
    async fn request_permission(&self, _request: PermissionRequest) -> PermissionDecision {
        PermissionDecision::Cancel
    }

    async fn extension(&self, method: &str, _params: Value) -> Result<Value, Error> {
        Err(Error::UnsupportedClientRequest(method.to_owned()))
    }
}
