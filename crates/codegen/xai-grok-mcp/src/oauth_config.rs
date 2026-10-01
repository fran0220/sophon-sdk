//! The host's TOML parsing (`McpServerConfig::oauth_config`) builds these types; [`crate::oauth`] consumes them.

pub use xai_grok_config::{McpOAuthConfig, McpOAuthConfigMap};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HostOAuthMeta {
    client_id: Option<String>,
    client_secret_env_var: Option<String>,
    scopes: Option<Vec<String>>,
    callback_port: Option<u16>,
}

/// Parse SDK-owned OAuth settings attached directly to an ACP HTTP/SSE server.
/// Secret values are resolved only here, at spawn time, and are never logged.
pub fn from_acp_server(
    server: &agent_client_protocol::McpServer,
) -> Result<Option<McpOAuthConfig>, String> {
    let meta = match server {
        agent_client_protocol::McpServer::Http(server) => server.meta.as_ref(),
        agent_client_protocol::McpServer::Sse(server) => server.meta.as_ref(),
        _ => None,
    };
    let Some(value) = meta.and_then(|m| m.get("x.sophon/oauth")) else {
        return Ok(None);
    };
    let parsed: HostOAuthMeta = serde_json::from_value(value.clone())
        .map_err(|e| format!("invalid x.sophon/oauth metadata: {e}"))?;
    let client_secret = match parsed.client_secret_env_var {
        Some(name) if name.trim().is_empty() => {
            return Err("x.sophon/oauth clientSecretEnvVar is empty".into());
        }
        Some(name) => Some(std::env::var(&name).map_err(|_| {
            format!("x.sophon/oauth client secret environment variable '{name}' is missing")
        })?),
        None => None,
    };
    Ok(Some(McpOAuthConfig {
        client_id: parsed.client_id,
        client_secret,
        scopes: parsed.scopes,
        callback_port: parsed.callback_port,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_oauth_metadata() {
        let mut server =
            agent_client_protocol::McpServerHttp::new("host", "https://example.invalid");
        server.meta = serde_json::json!({
            "x.sophon/oauth": {"clientId":"id", "scopes":["a","b"], "callbackPort":3210}
        })
        .as_object()
        .cloned();
        let parsed = from_acp_server(&agent_client_protocol::McpServer::Http(server))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.client_id.as_deref(), Some("id"));
        assert_eq!(
            parsed.scopes.as_deref(),
            Some(["a".into(), "b".into()].as_slice())
        );
        assert_eq!(parsed.callback_port, Some(3210));
    }

    #[test]
    fn rejects_missing_secret_environment_variable() {
        let mut server =
            agent_client_protocol::McpServerHttp::new("host", "https://example.invalid");
        server.meta = serde_json::json!({
            "x.sophon/oauth": {"clientId":"id", "clientSecretEnvVar":"SOPHON_TEST_DEFINITELY_MISSING_SECRET"}
        }).as_object().cloned();
        let error = from_acp_server(&agent_client_protocol::McpServer::Http(server)).unwrap_err();
        assert!(error.contains("is missing"));
        assert!(!error.contains("clientSecret\":"));
    }
}
