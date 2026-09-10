//! Per-entry rollback through native config writers (never snapshots another registry).
use super::*;

pub(super) struct PreviousConfig {
    config: Option<crate::util::config::McpServerConfig>,
    disabled: bool,
    disabled_tools: Vec<String>,
}

impl PreviousConfig {
    pub(super) async fn capture(name: &str) -> Result<Self, acp::Error> {
        let path = crate::util::config::user_config_path();
        let root: toml::Value = match tokio::fs::read_to_string(path).await {
            Ok(text) => toml::from_str(&text).map_err(|_| {
                acp::Error::internal_error().data("MCP config is not writable TOML")
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                toml::Value::Table(Default::default())
            }
            Err(_) => return Err(acp::Error::internal_error().data("MCP config is unreadable")),
        };
        let entry = root.get("mcp_servers").and_then(|s| s.get(name)).cloned();
        let config = entry
            .map(|value| value.try_into())
            .transpose()
            .map_err(|_| acp::Error::internal_error().data("existing MCP config is invalid"))?;
        let disabled = root
            .get("disabled_mcp_servers")
            .and_then(toml::Value::as_array)
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(name)));
        let disabled_tools = root
            .get("disabled_mcp_tools")
            .and_then(|s| s.get(name))
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .map(str::to_owned)
            .collect();
        Ok(Self {
            config,
            disabled,
            disabled_tools,
        })
    }

    pub(super) async fn restore(self, name: &str) -> Result<(), acp::Error> {
        let result = async {
            match self.config {
                Some(config) => crate::util::config::save_mcp_server_config(name, &config).await?,
                None => {
                    crate::util::config::delete_mcp_server_config(name).await?;
                }
            }
            crate::util::config::save_user_mcp_server_enabled(name, !self.disabled).await?;
            crate::util::config::save_mcp_disabled_tools(name, &self.disabled_tools).await
        }
        .await;
        result.map_err(|_| {
            acp::Error::internal_error().data("MCP mutation failed and config rollback failed")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_restores_native_config_and_preferences_without_replacing_other_entries() {
        // Native home resolution is process-cached. A serial environment guard
        // cannot reset a home already resolved by another test.
        const CHILD: &str = "SOPHON_MCP_ROLLBACK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "extensions::mcp::persistence::tests::rollback_restores_native_config_and_preferences_without_replacing_other_entries",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let _home = xai_grok_test_support::EnvGuard::set("GROK_HOME", home.path());
        let path = home.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
disabled_mcp_servers = ["local"]
[mcp_servers.local]
command = "old-server"
[disabled_mcp_tools]
local = ["old-tool"]
[unrelated]
value = "preserve"
"#,
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let previous = PreviousConfig::capture("local").await.unwrap();
            crate::util::config::delete_mcp_server_config("local")
                .await
                .unwrap();
            previous.restore("local").await.unwrap();
            let restored: toml::Value =
                toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(
                restored["mcp_servers"]["local"]["command"].as_str(),
                Some("old-server")
            );
            assert_eq!(restored["disabled_mcp_servers"][0].as_str(), Some("local"));
            assert_eq!(
                restored["disabled_mcp_tools"]["local"][0].as_str(),
                Some("old-tool")
            );
            assert_eq!(restored["unrelated"]["value"].as_str(), Some("preserve"));
            std::fs::write(&path, "not = [ valid TOML").unwrap();
            assert!(PreviousConfig::capture("local").await.is_err());
        });
    }
}
