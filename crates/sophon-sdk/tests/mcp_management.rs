use std::{collections::BTreeMap, time::Duration};

use sophon_sdk::mcp::{
    AuthOutcome, AuthResult, Inventory, ReadResourceRequest, Readiness, Resource, ServerConfig,
    ServerStatus, SetupRequest, ToggleRequest, ToggleToolRequest, Transport, UpsertRequest,
};
use sophon_sdk::{Agent, AgentConfig, ModelConfig, ProviderConfig, SessionConfig};
use xai_grok_test_support::EnvGuard;

#[test]
fn native_response_fixtures_preserve_status_and_redact_debug() {
    for (wire, expected) in [
        ("ready", Readiness::Ready),
        ("failed", Readiness::Failed),
        ("not_started", Readiness::NotStarted),
        ("abandoned", Readiness::Abandoned),
        ("replaced", Readiness::Replaced),
        ("timed_out", Readiness::TimedOut),
    ] {
        assert_eq!(
            serde_json::from_value::<Readiness>(serde_json::json!(wire)).unwrap(),
            expected
        );
    }
    let auth: AuthResult = serde_json::from_value(serde_json::json!({
        "status": "failed", "error": "secret-oauth-diagnostic"
    }))
    .unwrap();
    assert_eq!(auth.status, AuthOutcome::Failed);
    assert!(!format!("{auth:?}").contains("secret"));
    let resource: Resource = serde_json::from_value(serde_json::json!({"contents": [{
        "uri": "secret://path", "mimeType": "text/plain", "text": "secret-resource",
        "_meta": {"secret": "metadata"}
    }]}))
    .unwrap();
    assert_eq!(
        resource.contents[0].text.as_deref(),
        Some("secret-resource")
    );
    assert!(!format!("{resource:?}").contains("secret"));
    let inventory: Inventory = serde_json::from_value(serde_json::json!({"servers": [{
        "name": "local", "source": "local", "type": "stdio",
        "command": "secret-command", "env": [{"name": "TOKEN", "value": "secret"}],
        "setupValues": {"key": "secret"},
        "session": {"enabled": true, "status": "initializing"}
    }]}))
    .unwrap();
    assert_eq!(
        inventory.servers[0].session.as_ref().unwrap().status,
        Some(ServerStatus::Initializing)
    );
    assert!(!format!("{inventory:?}").contains("secret"));
    let setup = serde_json::to_value(SetupRequest {
        server_name: "local".into(),
        values: BTreeMap::from([("site".into(), "us".into())]),
    })
    .unwrap();
    assert_eq!(setup["serverName"], "local");
    assert!(setup.get("server_name").is_none());
}

// A real newline-delimited stdio MCP server; no network, OAuth provider, or model is needed.
const MCP_SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request:
        continue
    method = request['method']
    if method == 'initialize':
        result = {'protocolVersion': request['params']['protocolVersion'], 'capabilities': {'tools': {}, 'resources': {}}, 'serverInfo': {'name': 'sdk-test', 'version': '1'}}
    elif method == 'tools/list':
        result = {'tools': [{'name': 'ping', 'description': 'Ping', 'inputSchema': {'type': 'object', 'properties': {}}}]}
    elif method == 'resources/list':
        result = {'resources': []}
    elif method == 'resources/read':
        result = {'contents': [{'uri': request['params']['uri'], 'mimeType': 'text/plain', 'text': 'local-resource'}]}
    elif method == 'tools/call':
        result = {'content': [{'type': 'text', 'text': 'pong'}]}
    else:
        result = {}
    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
"#;

#[test]
fn local_mcp_crud_auth_toggle_resource_and_readiness() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let script = workspace.path().join("mcp_server.py");
    std::fs::write(&script, MCP_SERVER).unwrap();
    std::fs::create_dir(workspace.path().join(".cursor")).unwrap();
    std::fs::write(
        workspace.path().join(".cursor/mcp.json"),
        r#"{"mcpServers":{"ambient-forbidden":{"command":"false"}}}"#,
    )
    .unwrap();
    let _home = EnvGuard::set("GROK_HOME", home.path());
    let _telemetry = EnvGuard::set("GROK_TELEMETRY_ENABLED", "false");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(90), async {
            let agent = Agent::start(AgentConfig::new(ModelConfig::new(
                "mcp-test",
                ProviderConfig::openai_chat("http://127.0.0.1:1/v1", "unused", "unused"),
            )))
            .await
            .unwrap();
            let session = agent
                .create_session(SessionConfig::new(workspace.path()))
                .await
                .unwrap();
            let mcp = session.mcp();
            assert!(
                !mcp.list(true)
                    .await
                    .unwrap()
                    .servers
                    .iter()
                    .any(|s| s.name == "ambient-forbidden")
            );
            mcp.upsert(UpsertRequest {
                server_name: "local".into(),
                config: ServerConfig::new(Transport::Stdio {
                    command: "python3".into(),
                    args: vec![script.to_string_lossy().into_owned()],
                    env: BTreeMap::new(),
                    cwd: None,
                }),
            })
            .await
            .unwrap();
            assert_eq!(
                mcp.wait_ready(Duration::from_secs(20)).await.unwrap(),
                Readiness::Ready
            );
            assert!(home.path().join("config.toml").exists());
            let inventory = mcp.list(true).await.unwrap();
            let local = inventory
                .servers
                .iter()
                .find(|s| s.name == "local")
                .unwrap();
            assert_eq!(
                local.session.as_ref().unwrap().status,
                Some(ServerStatus::Ready)
            );
            assert!(mcp.auth_status().await.unwrap().servers.is_empty());
            assert_eq!(
                mcp.trigger_auth("local").await.unwrap().status,
                AuthOutcome::Failed
            );
            let resource = mcp
                .read_resource(ReadResourceRequest {
                    server: "local".into(),
                    uri: "test://resource".into(),
                })
                .await
                .unwrap();
            assert_eq!(resource.contents[0].text.as_deref(), Some("local-resource"));
            for enabled in [false, true] {
                mcp.set_tool_enabled(ToggleToolRequest {
                    server_name: "local".into(),
                    tool_name: "ping".into(),
                    enabled,
                })
                .await
                .unwrap();
            }
            for enabled in [false, true] {
                mcp.set_enabled(ToggleRequest {
                    server_name: "local".into(),
                    enabled,
                })
                .await
                .unwrap();
            }
            assert_eq!(
                mcp.wait_ready(Duration::from_secs(20)).await.unwrap(),
                Readiness::Ready
            );
            mcp.delete("local").await.unwrap();
            assert!(
                mcp.read_resource(ReadResourceRequest {
                    server: "local".into(),
                    uri: "test://resource".into()
                })
                .await
                .is_err()
            );
            // Invalid actor handles cannot alter host-owned persistent config.
            session.close().await.unwrap();
            let before = std::fs::read(home.path().join("config.toml")).unwrap();
            assert!(mcp.delete("local").await.is_err());
            assert_eq!(
                before,
                std::fs::read(home.path().join("config.toml")).unwrap()
            );
            agent.shutdown().await.unwrap();
        })
        .await
        .expect("MCP management deadline");
    });
}
