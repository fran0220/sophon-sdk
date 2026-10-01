use crate::SessionId;

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
    pub tool_call: crate::ToolCallUpdate,
    pub options: Vec<PermissionOption>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Select(String),
    Cancel,
}

/// Host callbacks for capabilities that require an embedding-side response.
///
/// Product tools use NativeToolHandler. MCP callbacks fail closed by default.
#[async_trait::async_trait]
pub trait ClientHandler: Send + Sync + 'static {
    /// The future is dropped when its requesting turn abandons the permission
    /// response. Keep this callback cancellation-safe. Independently spawned
    /// host tasks are not cancelled by dropping the callback future.
    async fn request_permission(&self, _request: PermissionRequest) -> PermissionDecision {
        PermissionDecision::Cancel
    }

    async fn elicit(
        &self,
        _request: crate::protocol::ElicitationRequest,
        _context: crate::protocol::McpCallbackContext,
    ) -> crate::protocol::ElicitResult {
        crate::protocol::ElicitResult::Cancel
    }

    async fn authorize_mcp(
        &self,
        _request: crate::protocol::OAuthAuthorizationRequest,
        _context: crate::protocol::McpCallbackContext,
    ) -> crate::protocol::OAuthAuthorizationResult {
        crate::protocol::OAuthAuthorizationResult { opened: false }
    }
}
