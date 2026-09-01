//! Process-level hermetic discovery switch.
//!
//! Hermetic discovery restricts harness capability discovery — skills,
//! agents, rules, MCP and LSP servers, hooks, plugins, workflows, subprocess
//! environment and project config overlays — to `$GROK_HOME` and explicitly
//! configured paths. Ambient sources (the literal home directory, vendor
//! config dirs such as `~/.claude`/`~/.cursor`, and `.grok`/`.agents` walks
//! from the working directory toward the git root) contribute nothing.
//! Ordinary project content and `AGENTS.md` instructions remain visible.
//!
//! This is a property of the embedding process, not of one config load: an
//! application that links the harness as a library owns its whole config
//! surface, so every discovery site in the process must agree. The switch is
//! therefore process-global, parallel to the `GROK_HOME` redirection that
//! such hosts already perform. It resolves once, first-wins:
//!
//! 1. [`set_hermetic_discovery`], called by an embedding host before any
//!    config load or discovery;
//! 2. otherwise the [`HERMETIC_DISCOVERY_ENV`] environment variable, which
//!    lets a launcher apply the same mode to child processes through their
//!    inherited environment.

use std::sync::OnceLock;

/// Environment variable supplying the process default.
pub const HERMETIC_DISCOVERY_ENV: &str = "GROK_HERMETIC_DISCOVERY";

static HERMETIC_DISCOVERY: OnceLock<bool> = OnceLock::new();

/// Whether this process restricts discovery to `$GROK_HOME` and explicitly
/// configured paths. Resolves on first call and never changes afterwards.
pub fn hermetic_discovery() -> bool {
    *HERMETIC_DISCOVERY.get_or_init(|| super::env_bool(HERMETIC_DISCOVERY_ENV).unwrap_or(false))
}

/// Fix the process switch programmatically and return its effective value.
///
/// First-wins against the environment read in [`hermetic_discovery`]: an
/// embedding host must call this before any config load or discovery. The
/// returned value lets a host fail closed if another caller already resolved
/// the process to a conflicting mode.
pub fn set_hermetic_discovery(value: bool) -> bool {
    *HERMETIC_DISCOVERY.get_or_init(|| value)
}
