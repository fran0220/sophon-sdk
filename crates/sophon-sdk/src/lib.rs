// Copyright 2026 Sophon SDK contributors
// Licensed under the Apache License, Version 2.0.

//! Thin compatibility layer over the pinned public Grok Build source.
//!
//! This crate owns no runtime, protocol mirror, storage authority, scheduler,
//! or orchestration state. It only re-exports Grok Build's public Rust and ACP
//! surfaces under one versioned dependency and reports the exact source pin.

pub use agent_client_protocol as acp;
pub use xai_acp_lib as transport;
pub use xai_grok_shell as grok_build;
pub use xai_grok_shell::agent::config::Config;
pub use xai_grok_shell::agent::mvp_agent::MvpAgent;
pub use xai_grok_shell::auth::AuthManager;

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
