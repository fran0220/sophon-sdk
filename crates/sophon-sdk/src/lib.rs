// Copyright 2026 Sophon SDK contributors
// Licensed under the Apache License, Version 2.0.

//! Thin provider-aware embedding facade over the pinned Grok Build source.
//!
//! Provider routing and an async Agent/Session API are SDK-owned. Agent
//! execution, tools, persistence, skills, plugins, MCP, and model behavior stay
//! upstream-owned. ACP is a private adapter and the TUI is not part of this API.

mod client;
mod config;
mod event;
mod runtime;

use std::fmt;
use std::path::PathBuf;

pub use client::{
    ClientHandler, PermissionDecision, PermissionOption, PermissionOptionKind, PermissionRequest,
};
pub use config::{
    AgentConfig, MediaConfig, MediaProviderConfig, ModelConfig, PermissionPolicy, ProviderConfig,
    ProviderProtocol,
};
pub use event::{Event, PlanEntry, SessionUpdate, ToolCall, ToolCallUpdate};
pub use runtime::Agent;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid SDK configuration: {0}")]
    InvalidConfig(String),
    #[error("failed to start Grok Build: {0}")]
    Start(String),
    #[error("Grok Build operation failed: {0}")]
    Operation(String),
    #[error("the embedding does not handle Grok Build client request: {0}")]
    UnsupportedClientRequest(String),
    #[error("Grok Build runtime stopped")]
    RuntimeStopped,
}

impl Error {
    pub(crate) fn invalid_config(message: impl Into<String>) -> Self {
        Self::InvalidConfig(message.into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(pub(crate) String);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub(crate) cwd: PathBuf,
    pub(crate) model: Option<String>,
    pub(crate) metadata: serde_json::Map<String, serde_json::Value>,
    pub(crate) mcp_servers: Vec<serde_json::Value>,
}

#[derive(Clone)]
pub struct Session {
    pub(crate) agent: Agent,
    pub(crate) id: SessionId,
    pub(crate) initial_response: serde_json::Value,
}

/// Persisted Grok Build session metadata returned by [`Agent::list_sessions`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: SessionId,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub updated_at: Option<String>,
}

/// One page of persisted sessions and the opaque cursor for the next page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPage {
    pub sessions: Vec<SessionInfo>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PromptBlock {
    Text(String),
    Image {
        data: String,
        mime_type: String,
    },
    Audio {
        data: String,
        mime_type: String,
    },
    ResourceLink {
        name: String,
        uri: String,
    },
    EmbeddedText {
        uri: String,
        text: String,
        mime_type: Option<String>,
    },
    EmbeddedBlob {
        uri: String,
        blob: String,
        mime_type: Option<String>,
    },
    /// Forward-compatible ACP content block represented only as JSON.
    Raw(serde_json::Value),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PromptResult {
    pub stop_reason: StopReason,
    /// Complete upstream prompt response, including usage, prompt identity,
    /// structured output, cancellation context, and future response fields.
    pub raw_response: serde_json::Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceProvenance {
    pub upstream_release: &'static str,
    pub upstream_grok_build_commit: &'static str,
    pub upstream_source_rev: &'static str,
    pub facade_version: &'static str,
}

pub fn source_provenance() -> SourceProvenance {
    SourceProvenance {
        upstream_release: "1.0.10",
        upstream_grok_build_commit: include_str!("../../../UPSTREAM_GROK_BUILD_COMMIT").trim(),
        upstream_source_rev: include_str!("../../../SOURCE_REV").trim(),
        facade_version: env!("CARGO_PKG_VERSION"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_matches_the_pinned_upstream_snapshot() {
        let provenance = source_provenance();
        assert_eq!(provenance.upstream_release, "1.0.10");
        assert_eq!(
            provenance.upstream_grok_build_commit,
            "9684fa3cdbf2995e30ea8b9b637f1db008f144fc"
        );
        assert_eq!(
            provenance.upstream_source_rev,
            "70ec060ec3d28e77b9c4593be43c2ab0128bcd21"
        );
        assert_eq!(provenance.facade_version, env!("CARGO_PKG_VERSION"));
    }
}
