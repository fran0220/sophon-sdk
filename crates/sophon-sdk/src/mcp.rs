//! Typed, session-scoped MCP management through the native MCP authority.
//!
//! Mutations acknowledge native application, not readiness. Use `wait_ready` to
//! observe one generation; replacement, timeout and failed initialization are
//! explicit outcomes. No method creates a second SDK server registry. Configuration
//! writes use the native host-owned `$GROK_HOME`; SDK hermetic discovery remains in effect.
use std::{collections::BTreeMap, fmt, time::Duration};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

/// Values that can contain credentials or server-supplied data deliberately have
/// opaque Debug output. Serialization is not redaction: treat serialized data as sensitive.
macro_rules! private_debug {
    ($($ty:ty),+ $(,)?) => {$(
        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($ty), " { .. }") )
            }
        }
    )+};
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Transport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    Http {
        url: String,
        #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
        transport_type: Option<String>,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        bearer_token_env_var: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        oauth_client_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        oauth_client_secret_env_var: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        oauth_scopes: Option<Vec<String>>,
    },
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OAuthConfig {
    pub client_id: Option<String>,
    pub client_secret_env_var: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub callback_port: Option<u16>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerConfig {
    #[serde(flatten)]
    pub transport: Transport,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup: Option<SetupSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_timeout_sec: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_timeout_sec: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_timeouts: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expose_image_base64: Option<bool>,
}

impl ServerConfig {
    pub fn new(transport: Transport) -> Self {
        Self {
            transport,
            enabled: true,
            oauth: None,
            setup: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            tool_timeouts: None,
            expose_image_base64: None,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetupSchema {
    #[serde(default)]
    pub fields: Vec<SetupField>,
    #[serde(default, alias = "values")]
    pub variables: BTreeMap<String, DerivedValue>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetupField {
    pub id: String,
    pub label: String,
    #[serde(rename = "type")]
    pub field_type: SetupFieldType,
    #[serde(default)]
    pub required: bool,
    pub default: Option<String>,
    #[serde(default)]
    pub options: Vec<SetupOption>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SetupFieldType {
    Select,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetupOption {
    pub label: String,
    pub value: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DerivedValue {
    pub from: String,
    pub map: BTreeMap<String, String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct UpsertRequest {
    pub server_name: String,
    #[serde(flatten)]
    pub config: ServerConfig,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupRequest {
    pub server_name: String,
    pub values: BTreeMap<String, String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ToggleRequest {
    pub server_name: String,
    pub enabled: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ToggleToolRequest {
    pub server_name: String,
    pub tool_name: String,
    pub enabled: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ReadResourceRequest {
    pub server: String,
    pub uri: String,
}

/// Reduced inventory. Native command arguments, URLs, environment values,
/// setup values, icons and free-form metadata are deliberately not projected here.
/// Setup schema defaults and labels remain server-provided, potentially sensitive data.
#[derive(Clone, Deserialize)]
pub struct Inventory {
    pub servers: Vec<Server>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Server {
    pub name: String,
    pub display_name: Option<String>,
    pub source: ServerSource,
    pub source_label: Option<String>,
    #[serde(rename = "type")]
    pub transport: TransportKind,
    pub setup: Option<SetupSchema>,
    pub session: Option<ServerState>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServerSource {
    Managed,
    Local,
    #[serde(other)]
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TransportKind {
    Stdio,
    Http,
    ManagedGateway,
    #[serde(other)]
    Other,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerState {
    pub enabled: bool,
    pub status: Option<ServerStatus>,
    #[serde(default)]
    pub tools: Vec<Tool>,
    #[serde(default)]
    pub auth_required: bool,
    #[serde(default)]
    pub setup_required: bool,
    pub blocked_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServerStatus {
    Ready,
    Initializing,
    SetupRequired,
    Unavailable,
    #[serde(other)]
    Other,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

/// Native auth_status lists only servers currently needing auth. Absence is not
/// proof of successful authentication or readiness.
#[derive(Clone, Deserialize)]
pub struct AuthStatus {
    pub servers: Vec<AuthEntry>,
}

#[derive(Clone, Deserialize)]
pub struct AuthEntry {
    pub server_name: String,
    pub status: AuthState,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthState {
    NeedsAuth,
    #[serde(other)]
    Other,
}

#[derive(Clone, Deserialize)]
pub struct AuthResult {
    pub status: AuthOutcome,
    pub setup: Option<SetupSchema>,
    /// Server-provided diagnostic. Not included in Debug.
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthOutcome {
    Authenticated,
    SetupRequired,
    Failed,
    #[serde(other)]
    Other,
}

#[derive(Clone, Deserialize)]
pub struct Resource {
    pub contents: Vec<ResourceContent>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceContent {
    pub uri: String,
    pub mime_type: Option<String>,
    pub text: Option<String>,
    /// Base64-encoded native resource bytes.
    pub blob: Option<String>,
    #[serde(rename = "_meta")]
    pub metadata: Option<Value>,
}

/// Observation of the generation acquired by this wait, not a durable promise
/// about later tool calls. Replaced never silently restarts against a new server set.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Readiness {
    Ready,
    Failed,
    Replaced,
    TimedOut,
    NotStarted,
    Abandoned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum McpError {
    #[error("MCP runtime unavailable")]
    Unavailable,
    #[error("MCP operation rejected by native authority")]
    Rejected,
    #[error("invalid MCP request")]
    InvalidRequest,
    #[error("invalid native MCP response")]
    InvalidResponse,
    #[error("agent admission is closed")]
    AdmissionClosed,
}

private_debug!(
    Transport,
    OAuthConfig,
    ServerConfig,
    SetupSchema,
    SetupField,
    SetupOption,
    DerivedValue,
    UpsertRequest,
    SetupRequest,
    ToggleRequest,
    ToggleToolRequest,
    ReadResourceRequest,
    Inventory,
    Server,
    ServerState,
    Tool,
    AuthStatus,
    AuthEntry,
    AuthResult,
    Resource,
    ResourceContent
);

/// Borrowed session management facade. Methods reuse the native dispatcher.
pub struct Mcp<'a> {
    session: &'a crate::Session,
}

impl crate::Session {
    pub fn mcp(&self) -> Mcp<'_> {
        Mcp { session: self }
    }
}

impl Mcp<'_> {
    async fn request<R: DeserializeOwned>(
        &self,
        method: &str,
        request: impl Serialize,
    ) -> Result<R, McpError> {
        let value = payload(method, self.session.id().as_str(), request)?;
        let response = self
            .session
            .extension(format!("x.ai/mcp/{method}"), value)
            .await
            .map_err(|error| match error {
                crate::Error::RuntimeStopped => McpError::Unavailable,
                crate::Error::InvalidConfig(_) => McpError::InvalidRequest,
                crate::Error::AdmissionRejected { .. } => McpError::AdmissionClosed,
                _ => McpError::Rejected,
            })?;
        decode(response)
    }

    async fn mutate(&self, method: &str, request: impl Serialize) -> Result<(), McpError> {
        #[derive(Deserialize)]
        struct Ack {
            ok: bool,
        }
        if self.request::<Ack>(method, request).await?.ok {
            Ok(())
        } else {
            Err(McpError::Rejected)
        }
    }

    pub async fn list(&self, cache: bool) -> Result<Inventory, McpError> {
        self.request("list", json!({"cache": cache})).await
    }
    pub async fn auth_status(&self) -> Result<AuthStatus, McpError> {
        self.request("auth_status", json!({})).await
    }
    pub async fn trigger_auth(&self, server_name: &str) -> Result<AuthResult, McpError> {
        self.request("auth_trigger", json!({"server_name": server_name}))
            .await
    }
    pub async fn setup(&self, request: SetupRequest) -> Result<(), McpError> {
        self.mutate("setup", request).await
    }
    pub async fn set_enabled(&self, request: ToggleRequest) -> Result<(), McpError> {
        self.mutate("toggle", request).await
    }
    pub async fn set_tool_enabled(&self, request: ToggleToolRequest) -> Result<(), McpError> {
        self.mutate("toggle_tool", request).await
    }
    /// Persist and enable a local native config. Disabled configs and unresolved
    /// setup are rejected by the native upsert route; use `set_enabled` and
    /// `setup` for existing discovered definitions instead.
    pub async fn upsert(&self, request: UpsertRequest) -> Result<(), McpError> {
        self.mutate("upsert", request).await
    }
    pub async fn delete(&self, server_name: &str) -> Result<(), McpError> {
        self.mutate("delete", json!({"server_name": server_name}))
            .await
    }
    pub async fn read_resource(&self, request: ReadResourceRequest) -> Result<Resource, McpError> {
        self.request("read_resource", request).await
    }
    /// Bounded to 1..=60000ms, including SDK dispatch and native actor snapshot acquisition.
    /// This observes initialization; it neither initiates nor retries it.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<Readiness, McpError> {
        let millis = timeout_millis(timeout)?;
        #[derive(Deserialize)]
        struct Response {
            outcome: Readiness,
        }
        match tokio::time::timeout(
            timeout,
            self.request::<Response>("wait_ready", json!({"timeoutMs": millis})),
        )
        .await
        {
            Ok(response) => Ok(response?.outcome),
            Err(_) => Ok(Readiness::TimedOut),
        }
    }
}

fn decode<R: DeserializeOwned>(response: Value) -> Result<R, McpError> {
    // Every native MCP route uses ExtMethodResult. Check only the envelope's
    // error: auth_trigger also carries a diagnostic inside a successful result.
    // Do not expose native diagnostics (which may contain credentials), or
    // silently accept partial results as successful mutations.
    if response.get("error").is_some_and(|error| !error.is_null()) {
        return Err(McpError::Rejected);
    }
    let result = response.get("result").ok_or(McpError::InvalidResponse)?;
    serde_json::from_value(result.clone()).map_err(|_| McpError::InvalidResponse)
}

fn timeout_millis(timeout: Duration) -> Result<u64, McpError> {
    if !(Duration::from_millis(1)..=Duration::from_secs(60)).contains(&timeout) {
        return Err(McpError::InvalidRequest);
    }
    Ok(timeout.as_millis() as u64)
}

fn payload(method: &str, session: &str, request: impl Serialize) -> Result<Value, McpError> {
    let mut value = serde_json::to_value(request).map_err(|_| McpError::InvalidRequest)?;
    let object = value.as_object_mut().ok_or(McpError::InvalidRequest)?;
    // These private native DTOs predate camelCase session extensions. Sending
    // only Session::extension's injected sessionId silently misses their field.
    let key = match method {
        "auth_status" | "auth_trigger" | "toggle" | "toggle_tool" | "upsert" | "delete" => {
            "session_id"
        }
        _ => "sessionId",
    };
    object.insert(key.into(), Value::String(session.into()));
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_envelopes_distinguish_rejection_payload_and_invalid_response() {
        let inventory: Inventory = decode(json!({"result": {"servers": []}})).unwrap();
        assert!(inventory.servers.is_empty());
        let auth: AuthResult = decode(json!({
            "result": {"status": "failed", "error": "secret-diagnostic"},
            "error": null
        }))
        .unwrap();
        assert_eq!(auth.status, AuthOutcome::Failed);
        assert_eq!(auth.error.as_deref(), Some("secret-diagnostic"));
        for response in [
            json!({"result": null, "error": "secret-diagnostic"}),
            json!({"result": null, "error": {"code": "failure", "message": "secret"}}),
            json!({"result": {"servers": []}, "error": "partial failure"}),
        ] {
            assert!(matches!(
                decode::<Inventory>(response),
                Err(McpError::Rejected)
            ));
        }
        for response in [
            json!({"servers": []}),
            json!({"result": null}),
            json!({"result": {"unexpected": []}}),
            json!(null),
        ] {
            assert!(matches!(
                decode::<Inventory>(response),
                Err(McpError::InvalidResponse)
            ));
        }
    }

    #[test]
    fn readiness_timeout_bounds_are_checked_before_millisecond_conversion() {
        for duration in [
            Duration::ZERO,
            Duration::from_nanos(999_999),
            Duration::from_secs(60) + Duration::from_nanos(1),
            Duration::MAX,
        ] {
            assert_eq!(timeout_millis(duration), Err(McpError::InvalidRequest));
        }
        assert_eq!(timeout_millis(Duration::from_millis(1)), Ok(1));
        assert_eq!(timeout_millis(Duration::from_secs(60)), Ok(60_000));
    }

    #[test]
    fn authoritative_session_and_config_payload_shapes() {
        for method in [
            "auth_status",
            "auth_trigger",
            "toggle",
            "toggle_tool",
            "upsert",
            "delete",
        ] {
            assert_eq!(payload(method, "s", json!({})).unwrap()["session_id"], "s");
        }
        for method in ["list", "setup", "read_resource", "wait_ready"] {
            assert_eq!(payload(method, "s", json!({})).unwrap()["sessionId"], "s");
        }
        let config = ServerConfig::new(Transport::Stdio {
            command: "server".into(),
            args: vec![],
            env: BTreeMap::from([("TOKEN".into(), "secret".into())]),
            cwd: None,
        });
        let upsert = UpsertRequest {
            server_name: "local".into(),
            config,
        };
        let value = payload("upsert", "s", &upsert).unwrap();
        assert_eq!(value["command"], "server");
        assert_eq!(value["env"]["TOKEN"], "secret");
        assert!(value.get("config").is_none());
        assert!(!format!("{upsert:?}").contains("secret"));
    }
}
