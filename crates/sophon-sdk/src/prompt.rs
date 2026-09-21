//! Neutral configuration and prompt inputs. Native wire conversion stays private.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpServer {
    Stdio {
        name: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Http {
        name: String,
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    Sse {
        name: String,
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigCandidate {
    pub revision: String,
    pub instructions: String,
    pub skill_directories: Vec<String>,
    pub external_mcp_servers: Vec<McpServer>,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub subagent_briefs: Vec<SubagentBrief>,
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubagentBrief {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub model: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct PromptOptions {
    pub prompt_id: Option<String>,
    pub config_candidate: Option<ConfigCandidate>,
    pub send_now: bool,
}
