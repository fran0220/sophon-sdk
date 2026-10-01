//! Project config-file discovery: locating repo-local `.mcp.json` and `.grok/config.toml` files by walking from `cwd` up to the git root.
//!
//! These pure `git2` and filesystem walks are shared by the shell's config loaders and the folder-trust gate's `repo_configs_present`.

use std::path::{Path, PathBuf};

use crate::repo::RepoDirChain;

/// Filename of the project-local MCP server config.
pub const MCP_JSON_FILENAME: &str = ".mcp.json";

/// Candidate `.mcp.json` paths from repo root to `cwd`, whether or not they exist.
/// Useful for file watching so newly created files are detected after startup.
pub fn mcp_json_candidate_paths(cwd: &Path) -> Vec<PathBuf> {
    if xai_grok_config::hermetic_discovery() {
        return Vec::new();
    }
    mcp_json_candidate_paths_in(&RepoDirChain::resolve(cwd).dirs)
}

/// [`mcp_json_candidate_paths`] over a precomputed cwd-to-git-root dir chain ([`RepoDirChain`]), repo-root-first.
fn mcp_json_candidate_paths_in(chain_dirs: &[PathBuf]) -> Vec<PathBuf> {
    chain_dirs
        .iter()
        .rev()
        .map(|dir| dir.join(MCP_JSON_FILENAME))
        .collect()
}

/// Find existing `.mcp.json` files from `cwd` up to the git root (repo-root-first order).
pub fn find_mcp_json_files(cwd: &Path) -> Vec<PathBuf> {
    find_mcp_json_files_in(&RepoDirChain::resolve(cwd).dirs)
}

/// [`find_mcp_json_files`] over a precomputed dir chain. See [`RepoDirChain`].
/// `pub` so the folder-trust gate's `repo_configs_present` can call it.
pub fn find_mcp_json_files_in(chain_dirs: &[PathBuf]) -> Vec<PathBuf> {
    find_mcp_json_files_in_mode(chain_dirs, xai_grok_config::hermetic_discovery())
}

fn find_mcp_json_files_in_mode(chain_dirs: &[PathBuf], hermetic: bool) -> Vec<PathBuf> {
    if hermetic {
        return Vec::new();
    }
    mcp_json_candidate_paths_in(chain_dirs)
        .into_iter()
        .filter(|path| path.is_file())
        .collect()
}

/// True when `config_path` is `<grok_home>/config.toml` (user tier, not project).
fn is_user_grok_config_file(config_path: &Path, grok_home: Option<&Path>) -> bool {
    let Some(grok_home) = grok_home else {
        return false;
    };
    let user_config = grok_home.join("config.toml");
    if config_path == user_config.as_path() {
        return true;
    }
    let Ok(canonical_config) = dunce::canonicalize(config_path) else {
        return false;
    };
    let canonical_user = dunce::canonicalize(&user_config).unwrap_or(user_config);
    canonical_config == canonical_user
}

/// Find `.grok/config.toml` from `cwd` up to the git repo root, repo-root (lowest) to cwd (highest), matching skills and AGENTS.md discovery.
/// No repo: only `cwd/.grok/config.toml`. Excludes user-global config so `cwd == $HOME` is not a project overlay.
pub fn find_project_configs(cwd: &Path) -> Vec<PathBuf> {
    find_project_configs_under(
        cwd,
        xai_dirs::home_dir().as_deref(),
        xai_grok_config::user_grok_home().as_deref(),
    )
}

pub fn find_project_configs_under(
    cwd: &Path,
    home: Option<&Path>,
    grok_home: Option<&Path>,
) -> Vec<PathBuf> {
    find_project_configs_in(&RepoDirChain::resolve_under_home(cwd, home).dirs, grok_home)
}

/// [`find_project_configs`] over a precomputed [`RepoDirChain`], repo-root-first.
/// Excludes user-global config so `cwd == $HOME` is not a project overlay. `pub` for the folder-trust gate.
pub fn find_project_configs_in(chain_dirs: &[PathBuf], grok_home: Option<&Path>) -> Vec<PathBuf> {
    find_project_configs_in_mode(chain_dirs, grok_home, xai_grok_config::hermetic_discovery())
}

fn find_project_configs_in_mode(
    chain_dirs: &[PathBuf],
    grok_home: Option<&Path>,
    hermetic: bool,
) -> Vec<PathBuf> {
    if hermetic {
        return Vec::new();
    }
    // `dirs` is cwd-first; reverse so repo root comes first (lowest priority)
    // and cwd last (highest), matching skills/AGENTS.md discovery order.
    chain_dirs
        .iter()
        .rev()
        .map(|dir| dir.join(".grok").join("config.toml"))
        .filter(|config_path| {
            config_path.is_file() && !is_user_grok_config_file(config_path, grok_home)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_project_configs_excludes_user_grok_config_file() {
        let home = tempfile::tempdir().unwrap();
        let user_home = home.path().join(".grok");
        std::fs::create_dir_all(&user_home).unwrap();
        let user_config = user_home.join("config.toml");
        std::fs::write(&user_config, "# user\n").unwrap();
        let from_home =
            find_project_configs_under(home.path(), Some(home.path()), Some(&user_home));
        assert!(
            from_home.is_empty(),
            "user config leaked into project configs: {from_home:?}"
        );
        assert!(is_user_grok_config_file(&user_config, Some(&user_home)));

        let project = home.path().join("repo");
        std::fs::create_dir_all(project.join(".grok")).unwrap();
        std::fs::write(project.join(".grok/config.toml"), "# project\n").unwrap();
        let found = find_project_configs_under(&project, Some(home.path()), Some(&user_home));
        assert_eq!(found.len(), 1);
        let Some(first) = found.first() else {
            panic!("expected one project config: {found:?}");
        };
        assert!(!is_user_grok_config_file(first, Some(&user_home)));
    }

    #[test]
    fn hermetic_mode_hides_project_config_and_mcp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(project.join(".grok")).unwrap();
        std::fs::write(project.join(".grok/config.toml"), "# project\n").unwrap();
        std::fs::write(project.join(".mcp.json"), "{}\n").unwrap();
        let chain = vec![project];

        assert_eq!(find_project_configs_in_mode(&chain, None, false).len(), 1);
        assert_eq!(find_mcp_json_files_in_mode(&chain, false).len(), 1);
        assert!(find_project_configs_in_mode(&chain, None, true).is_empty());
        assert!(find_mcp_json_files_in_mode(&chain, true).is_empty());
    }
}
