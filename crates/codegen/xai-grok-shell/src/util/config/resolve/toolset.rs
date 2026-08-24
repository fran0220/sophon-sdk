use crate::util::config::RemoteSettings;
use toml::Value as TomlValue;

/// Resolve whether the bash-harness `find`→`bfs` / `grep`→`ugrep` shadows are
/// enabled. Precedence (highest first): `requirements.toml` (org policy, wins
/// outright) > a truthy `DISABLE_EMBEDDED_SEARCH_TOOLS` master (forces off) > env
/// > `config.toml` `[toolset.bash]` > `managed_config.toml` > default-on. Env uses the shared
/// [`xai_grok_config::env_bool`] parser (`GROK_TOOLS_FIND_BFS` /
/// `GROK_TOOLS_GREP_UGREP`, plus the `GROK_FIND_BFS` / `GROK_GREP_UGREP` aliases).
///
/// Pass the **merged** requirements ([`crate::config::load_merged_requirements`])
/// so an org policy in any requirements layer — not only
/// `~/.grok/requirements.toml` — is honored. Returns `(find_bfs, grep_ugrep)`,
/// which the caller bakes into a
/// [`xai_grok_tools::computer::local::SearchShadowConfig`] on the local terminal
/// backend.
pub(crate) fn resolve_search_tools_enabled(
    requirements: Option<&TomlValue>,
    user: Option<&TomlValue>,
    managed: Option<&TomlValue>,
) -> (bool, bool) {
    let disable = xai_grok_config::env_bool("DISABLE_EMBEDDED_SEARCH_TOOLS");
    fn from_toml(v: Option<&TomlValue>, key: &str) -> Option<bool> {
        v?.get("toolset")?.get("bash")?.get(key)?.as_bool()
    }
    let resolve = |primary: &str, alias: &str, key: &str| -> bool {
        let env = xai_grok_config::env_bool(primary).or_else(|| xai_grok_config::env_bool(alias));
        resolve_search_tool_enabled(
            disable,
            from_toml(requirements, key),
            env,
            from_toml(user, key),
            from_toml(managed, key),
        )
    };
    (
        resolve("GROK_TOOLS_FIND_BFS", "GROK_FIND_BFS", "find_bfs"),
        resolve("GROK_TOOLS_GREP_UGREP", "GROK_GREP_UGREP", "grep_ugrep"),
    )
}

/// Parse `[shell_environment_policy]` from the merged effective config, or `None`
/// when unset or unparseable (the child then inherits the full environment). This
/// is the authoritative parse; the `Config` field of the same name only feeds the
/// unrecognized-key scan.
pub(crate) fn resolve_shell_env_policy(
    effective_cfg: Option<&TomlValue>,
) -> Option<xai_grok_tools::util::ShellEnvironmentPolicy> {
    let value = effective_cfg?.get("shell_environment_policy")?.clone();
    match value.try_into::<xai_grok_tools::util::ShellEnvironmentPolicy>() {
        Ok(policy) => Some(policy),
        Err(error) => {
            tracing::warn!(
                %error,
                "failed to parse [shell_environment_policy]; inheriting the full environment"
            );
            None
        }
    }
}

/// Pure precedence for [`resolve_search_tools_enabled`] (tiers injected so it is
/// unit-testable without env/disk): requirement (org policy) wins outright — even
/// over the user `DISABLE_*` master kill-switch — then the master forces off,
/// then env > config > managed > default-on.
fn resolve_search_tool_enabled(
    disable: Option<bool>,
    requirement: Option<bool>,
    env: Option<bool>,
    config: Option<bool>,
    managed: Option<bool>,
) -> bool {
    // Org policy (requirements.toml) is authoritative, so a user env kill-switch
    // can't override an admin-forced value.
    if let Some(required) = requirement {
        return required;
    }
    if disable == Some(true) {
        return false;
    }
    env.or(config).or(managed).unwrap_or(true)
}

const ENV_LOGIN_SHELL_CAPTURE: &str = "GROK_LOGIN_ENV";

fn login_shell_capture_from_toml(v: Option<&TomlValue>) -> Option<bool> {
    v?.get("toolset")?
        .get("bash")?
        .get("login_shell_capture")?
        .as_bool()
}

pub(crate) fn resolve_login_shell_capture(remote: Option<bool>) -> bool {
    let requirements = crate::config::load_merged_requirements();
    let layers = match crate::config::ConfigLayers::load() {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "login_shell_capture: failed to load config layers");
            crate::config::ConfigLayers::default()
        }
    };
    let crate::config::ConfigLayers {
        system_managed,
        managed,
        user,
        env_overlay,
        user_requirements: _,
        system_requirements: _,
        mdm_requirements: _,
        campaigns: _,
    } = &layers;
    resolve_login_shell_capture_tiers(LoginShellCaptureTiers {
        requirements: requirements.as_ref(),
        env_overlay: env_overlay.as_ref(),
        user: Some(user),
        managed: Some(managed),
        system_managed: Some(system_managed),
        remote,
    })
}

/// Config layers for [`resolve_login_shell_capture_tiers`], one `Option` per
/// tier so a positional `None` can't be misread as the wrong layer.
#[derive(Default)]
struct LoginShellCaptureTiers<'a> {
    requirements: Option<&'a TomlValue>,
    env_overlay: Option<&'a TomlValue>,
    user: Option<&'a TomlValue>,
    managed: Option<&'a TomlValue>,
    system_managed: Option<&'a TomlValue>,
    remote: Option<bool>,
}

/// Precedence (highest first): requirements/MDM (clamp, via
/// [`crate::config::load_merged_requirements`]) > `GROK_LOGIN_ENV` env >
/// `GROK_CONFIG` overlay > user `config.toml` > managed layers > remote >
/// default `true`. `login_shell_capture` is a soft key, so the overlay is
/// merged just above user config (mirroring its place in the disk merge: above
/// user, below requirements), letting a `GROK_CONFIG` toggle reach it while
/// requirements/MDM still clamp the value.
fn resolve_login_shell_capture_tiers(tiers: LoginShellCaptureTiers<'_>) -> bool {
    let LoginShellCaptureTiers {
        requirements,
        env_overlay,
        user,
        managed,
        system_managed,
        remote,
    } = tiers;
    use crate::agent::config::BoolFlag;
    BoolFlag::env(ENV_LOGIN_SHELL_CAPTURE)
        .requirement(login_shell_capture_from_toml(requirements))
        .config(
            login_shell_capture_from_toml(env_overlay)
                .or_else(|| login_shell_capture_from_toml(user)),
        )
        .managed(
            login_shell_capture_from_toml(managed)
                .or_else(|| login_shell_capture_from_toml(system_managed)),
        )
        .feature_flag(remote)
        .default(true)
        .resolve()
        .value
}

#[cfg(test)]
mod login_shell_capture_tests {
    use super::{
        ENV_LOGIN_SHELL_CAPTURE, LoginShellCaptureTiers, resolve_login_shell_capture_tiers,
    };
    use toml::Value as TomlValue;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe { std::env::remove_var(ENV_LOGIN_SHELL_CAPTURE) };
        g
    }

    fn cfg(enabled: bool) -> TomlValue {
        toml::from_str(&format!(
            "[toolset.bash]\nlogin_shell_capture = {enabled}\n"
        ))
        .unwrap()
    }

    #[test]
    fn defaults_on() {
        let _g = guard();
        assert!(resolve_login_shell_capture_tiers(
            LoginShellCaptureTiers::default()
        ));
    }

    #[test]
    fn remote_flag_can_disable() {
        let _g = guard();
        assert!(!resolve_login_shell_capture_tiers(LoginShellCaptureTiers {
            remote: Some(false),
            ..Default::default()
        }));
    }

    #[test]
    fn user_config_beats_remote() {
        let _g = guard();
        assert!(resolve_login_shell_capture_tiers(LoginShellCaptureTiers {
            user: Some(&cfg(true)),
            remote: Some(false),
            ..Default::default()
        }));
        assert!(!resolve_login_shell_capture_tiers(LoginShellCaptureTiers {
            user: Some(&cfg(false)),
            remote: Some(true),
            ..Default::default()
        }));
    }

    #[test]
    fn env_beats_config_and_remote() {
        let _g = guard();
        unsafe { std::env::set_var(ENV_LOGIN_SHELL_CAPTURE, "0") };
        let off = resolve_login_shell_capture_tiers(LoginShellCaptureTiers {
            user: Some(&cfg(true)),
            remote: Some(true),
            ..Default::default()
        });
        unsafe { std::env::remove_var(ENV_LOGIN_SHELL_CAPTURE) };
        assert!(!off);
    }

    #[test]
    fn requirements_win_outright() {
        let _g = guard();
        unsafe { std::env::set_var(ENV_LOGIN_SHELL_CAPTURE, "1") };
        let off = resolve_login_shell_capture_tiers(LoginShellCaptureTiers {
            requirements: Some(&cfg(false)),
            user: Some(&cfg(true)),
            remote: Some(true),
            ..Default::default()
        });
        unsafe { std::env::remove_var(ENV_LOGIN_SHELL_CAPTURE) };
        assert!(!off);
    }

    #[test]
    fn overlay_beats_user_and_default() {
        let _g = guard();
        // (a) Overlay `false` is honored over a user-on config...
        assert!(!resolve_login_shell_capture_tiers(LoginShellCaptureTiers {
            env_overlay: Some(&cfg(false)),
            user: Some(&cfg(true)),
            ..Default::default()
        }));
        // ...and over the bare default-on (no disk layer at all).
        assert!(!resolve_login_shell_capture_tiers(LoginShellCaptureTiers {
            env_overlay: Some(&cfg(false)),
            ..Default::default()
        }));
    }

    #[test]
    fn overlay_honored_in_both_directions_over_disk() {
        let _g = guard();
        // (b) Overlay `true` beats a user-`false` disk config...
        assert!(resolve_login_shell_capture_tiers(LoginShellCaptureTiers {
            env_overlay: Some(&cfg(true)),
            user: Some(&cfg(false)),
            ..Default::default()
        }));
        // ...and overlay `false` beats a user-`true` disk config.
        assert!(!resolve_login_shell_capture_tiers(LoginShellCaptureTiers {
            env_overlay: Some(&cfg(false)),
            user: Some(&cfg(true)),
            ..Default::default()
        }));
    }

    #[test]
    fn requirements_clamp_the_overlay() {
        let _g = guard();
        // (c) A requirements value clamps the overlay in both directions.
        assert!(!resolve_login_shell_capture_tiers(LoginShellCaptureTiers {
            requirements: Some(&cfg(false)),
            env_overlay: Some(&cfg(true)),
            ..Default::default()
        }));
        assert!(resolve_login_shell_capture_tiers(LoginShellCaptureTiers {
            requirements: Some(&cfg(true)),
            env_overlay: Some(&cfg(false)),
            ..Default::default()
        }));
    }

    #[test]
    fn env_beats_overlay() {
        let _g = guard();
        // `GROK_LOGIN_ENV` outranks the overlay (env > overlay).
        unsafe { std::env::set_var(ENV_LOGIN_SHELL_CAPTURE, "1") };
        let on = resolve_login_shell_capture_tiers(LoginShellCaptureTiers {
            env_overlay: Some(&cfg(false)),
            ..Default::default()
        });
        unsafe { std::env::remove_var(ENV_LOGIN_SHELL_CAPTURE) };
        assert!(on);
    }
}

const ENV_SCHEDULER_BACKGROUND_LOOPS: &str = "GROK_SCHEDULER_BACKGROUND_LOOPS";

fn scheduler_background_loops_from_toml(v: Option<&TomlValue>) -> Option<bool> {
    v?.get("scheduler")?.get("background_loops")?.as_bool()
}

/// Resolve whether scheduled task fires run in background loop subagents.
///
/// Precedence: requirements > env (`GROK_SCHEDULER_BACKGROUND_LOOPS`) > user
/// `config.toml` `[scheduler] background_loops` > managed layers > remote
/// settings > default `true`.
pub fn resolve_scheduler_background_loops(remote: Option<bool>) -> bool {
    let requirements = crate::config::load_merged_requirements();
    let layers = match crate::config::ConfigLayers::load() {
        Ok(l) => Some(l),
        Err(e) => {
            tracing::warn!(error = %e, "scheduler_background_loops: failed to load config layers");
            None
        }
    };
    resolve_scheduler_background_loops_tiers(
        requirements.as_ref(),
        layers.as_ref().map(|l| &l.user),
        layers.as_ref().map(|l| &l.managed),
        layers.as_ref().map(|l| &l.system_managed),
        remote,
    )
}

fn resolve_scheduler_background_loops_tiers(
    requirements: Option<&TomlValue>,
    user: Option<&TomlValue>,
    managed: Option<&TomlValue>,
    system_managed: Option<&TomlValue>,
    remote: Option<bool>,
) -> bool {
    use crate::agent::config::BoolFlag;
    BoolFlag::env(ENV_SCHEDULER_BACKGROUND_LOOPS)
        .requirement(scheduler_background_loops_from_toml(requirements))
        .config(scheduler_background_loops_from_toml(user))
        .managed(
            scheduler_background_loops_from_toml(managed)
                .or_else(|| scheduler_background_loops_from_toml(system_managed)),
        )
        .feature_flag(remote)
        .default(true)
        .resolve()
        .value
}

#[cfg(test)]
mod scheduler_background_loops_tests {
    use super::{ENV_SCHEDULER_BACKGROUND_LOOPS, resolve_scheduler_background_loops_tiers};
    use toml::Value as TomlValue;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe { std::env::remove_var(ENV_SCHEDULER_BACKGROUND_LOOPS) };
        g
    }

    fn cfg(enabled: bool) -> TomlValue {
        toml::from_str(&format!("[scheduler]\nbackground_loops = {enabled}\n")).unwrap()
    }

    #[test]
    fn defaults_on() {
        let _g = guard();
        assert!(resolve_scheduler_background_loops_tiers(
            None, None, None, None, None
        ));
    }

    #[test]
    fn remote_flag_can_disable() {
        let _g = guard();
        assert!(!resolve_scheduler_background_loops_tiers(
            None,
            None,
            None,
            None,
            Some(false)
        ));
    }

    #[test]
    fn user_config_beats_remote() {
        let _g = guard();
        assert!(resolve_scheduler_background_loops_tiers(
            None,
            Some(&cfg(true)),
            None,
            None,
            Some(false)
        ));
        assert!(!resolve_scheduler_background_loops_tiers(
            None,
            Some(&cfg(false)),
            None,
            None,
            Some(true)
        ));
    }

    #[test]
    fn env_beats_config_and_remote() {
        let _g = guard();
        unsafe { std::env::set_var(ENV_SCHEDULER_BACKGROUND_LOOPS, "0") };
        let off = resolve_scheduler_background_loops_tiers(
            None,
            Some(&cfg(true)),
            None,
            None,
            Some(true),
        );
        unsafe { std::env::remove_var(ENV_SCHEDULER_BACKGROUND_LOOPS) };
        assert!(!off);
    }

    #[test]
    fn requirements_win_outright() {
        let _g = guard();
        unsafe { std::env::set_var(ENV_SCHEDULER_BACKGROUND_LOOPS, "1") };
        let off = resolve_scheduler_background_loops_tiers(
            Some(&cfg(false)),
            Some(&cfg(true)),
            None,
            None,
            Some(true),
        );
        unsafe { std::env::remove_var(ENV_SCHEDULER_BACKGROUND_LOOPS) };
        assert!(!off);
    }
}

/// `[toolset.web_search]` domain keys. Read here from the raw config layers, not
/// through `ShellToolsetConfig::web_search` (a `SamplerConfig`, which has no such
/// fields), so they must be exempt from the unrecognized-key scan.
pub const WEB_SEARCH_DOMAIN_CONFIG_PATHS: &[&str] = &[
    "toolset.web_search.allowed_domains",
    "toolset.web_search.excluded_domains",
];

/// Read a `[toolset.web_search] <key>` string array from the already-merged
/// section. Empty or absent yields `None` (unbounded). For a security-flavored
/// setting a silent typo must not disable the policy, so a present-but-malformed
/// value is warned (not just dropped): a non-array value yields `None` with a
/// warning, and any non-string array entry is skipped with a warning.
fn read_domain_array(section: &TomlValue, key: &str) -> Option<Vec<String>> {
    let value = section.get(key)?;
    let Some(arr) = value.as_array() else {
        tracing::warn!(
            key,
            "[toolset.web_search] {key} must be an array of strings; ignoring it"
        );
        return None;
    };
    let mut domains = Vec::with_capacity(arr.len());
    for entry in arr {
        match entry.as_str() {
            Some(s) => domains.push(s.to_owned()),
            None => tracing::warn!(
                key,
                "[toolset.web_search] {key} entry is not a string; skipping it"
            ),
        }
    }
    (!domains.is_empty()).then_some(domains)
}

/// Cap a config-sourced domain list to the web-search API's maximum, logging a
/// warning on truncation. Config degrades rather than failing the session (the
/// repo convention for `[toolset.*]`); authored inputs (frontmatter / per-turn)
/// instead hard-error via `WebSearchOptions`'s deserialize validation.
fn cap_web_search_domains(list: Option<Vec<String>>, field: &str) -> Option<Vec<String>> {
    const MAX: usize = xai_grok_sampling_types::MAX_WEB_SEARCH_DOMAINS;
    list.map(|mut domains| {
        if domains.len() > MAX {
            tracing::warn!(
                field,
                count = domains.len(),
                max = MAX,
                "[toolset.web_search] {field} has {} domains but the web-search API allows at \
                 most {MAX}; using the first {MAX}",
                domains.len()
            );
            domains.truncate(MAX);
        }
        domains
    })
}

/// Resolve `[toolset.web_search]` domain filters into a [`WebSearchOptions`].
///
/// Layer precedence and the allow/exclude atomicity are handled **upstream** by
/// `ConfigLayers` (per-layer normalization couples the two keys, then the normal
/// `deep_merge_toml` picks the whole policy from the winning layer). So this just
/// reads the already-merged `[toolset.web_search]` section and shapes it: caps
/// each list to the API max, and defensively drops `excluded_domains` if a
/// single layer set both (warns). Returns `None` when neither filter is set.
///
/// [`WebSearchOptions`]: xai_grok_sampling_types::WebSearchOptions
pub(crate) fn resolve_web_search_domains_from_disk()
-> Option<xai_grok_sampling_types::WebSearchOptions> {
    let effective = match crate::config::load_effective_config() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "web_search: failed to load effective config");
            return None;
        }
    };
    let section = effective.get("toolset")?.get("web_search")?;
    web_search_options_from_section(section)
}

/// Shape a merged `[toolset.web_search]` section into [`WebSearchOptions`]
/// (unit-testable core of [`resolve_web_search_domains_from_disk`]).
fn web_search_options_from_section(
    section: &TomlValue,
) -> Option<xai_grok_sampling_types::WebSearchOptions> {
    let allowed = cap_web_search_domains(
        read_domain_array(section, "allowed_domains"),
        "allowed_domains",
    );
    let excluded = cap_web_search_domains(
        read_domain_array(section, "excluded_domains"),
        "excluded_domains",
    );
    let opts = xai_grok_sampling_types::WebSearchOptions {
        allowed_domains: allowed,
        excluded_domains: excluded,
    };
    if opts.is_empty() {
        return None;
    }
    match opts.validate() {
        Ok(()) => Some(opts),
        // Defensive: normalization should make a both-set section impossible, but
        // a single layer that set both survives the merge, so degrade here too.
        Err(e) => {
            tracing::warn!(
                error = %e,
                "[toolset.web_search] sets both allowed_domains and excluded_domains; \
                 dropping excluded_domains (allowlist wins)"
            );
            Some(xai_grok_sampling_types::WebSearchOptions {
                allowed_domains: opts.allowed_domains,
                excluded_domains: None,
            })
        }
    }
}

#[cfg(test)]
mod web_search_domains_tests {
    use super::*;

    // Cross-layer precedence + allow/exclude atomicity live in ConfigLayers
    // (see `xai_grok_config::loader` normalization tests). These cover only the
    // section-shaping this module still owns: extraction, the max-5 cap, and the
    // defensive both-set degrade.
    fn section(body: &str) -> TomlValue {
        let full: TomlValue = toml::from_str(&format!("[toolset.web_search]\n{body}\n")).unwrap();
        full.get("toolset")
            .unwrap()
            .get("web_search")
            .unwrap()
            .clone()
    }

    #[test]
    fn none_when_unset_or_empty() {
        let empty: TomlValue = toml::from_str("").unwrap();
        assert!(web_search_options_from_section(&empty).is_none());
        assert!(web_search_options_from_section(&section("allowed_domains = []")).is_none());
    }

    #[test]
    fn allowlist_parsed() {
        let got = web_search_options_from_section(&section(
            r#"allowed_domains = ["docs.x.ai", "arxiv.org"]"#,
        ))
        .unwrap();
        assert_eq!(
            got.allowed_domains,
            Some(vec!["docs.x.ai".to_string(), "arxiv.org".to_string()])
        );
        assert!(got.excluded_domains.is_none());
    }

    #[test]
    fn blocklist_parsed() {
        let got = web_search_options_from_section(&section(r#"excluded_domains = ["reddit.com"]"#))
            .unwrap();
        assert!(got.allowed_domains.is_none());
        assert_eq!(got.excluded_domains, Some(vec!["reddit.com".to_string()]));
    }

    #[test]
    fn caps_to_the_max_instead_of_failing() {
        let got = web_search_options_from_section(&section(
            r#"allowed_domains = ["1", "2", "3", "4", "5", "6", "7"]"#,
        ))
        .unwrap();
        assert_eq!(
            got.allowed_domains.map(|d| d.len()),
            Some(5),
            "config should degrade (cap to 5), not fail"
        );
    }

    #[test]
    fn both_set_degrades_to_allowlist() {
        // Normalization prevents this across layers; this is the in-one-layer
        // defensive path.
        let got = web_search_options_from_section(&section(
            "allowed_domains = [\"docs.x.ai\"]\nexcluded_domains = [\"reddit.com\"]",
        ))
        .unwrap();
        assert_eq!(got.allowed_domains, Some(vec!["docs.x.ai".to_string()]));
        assert!(got.excluded_domains.is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Assumes GROK_TOOLS_* / DISABLE_EMBEDDED_SEARCH_TOOLS are unset in the test env.
    #[test]
    fn resolve_search_tools_enabled_layers_and_precedence() {
        // Default-on when nothing is set.
        assert_eq!(resolve_search_tools_enabled(None, None, None), (true, true));

        // config.toml [toolset.bash] disables per tool.
        let user: TomlValue =
            toml::from_str("[toolset.bash]\nfind_bfs = false\ngrep_ugrep = true\n").unwrap();
        assert_eq!(
            resolve_search_tools_enabled(None, Some(&user), None),
            (false, true)
        );

        // Precedence: requirements > config.toml > managed.
        let req: TomlValue = toml::from_str("[toolset.bash]\nfind_bfs = true\n").unwrap();
        let managed: TomlValue = toml::from_str("[toolset.bash]\ngrep_ugrep = false\n").unwrap();
        let (find, grep) = resolve_search_tools_enabled(Some(&req), Some(&user), Some(&managed));
        assert!(find); // requirements `true` beats user `false`
        assert!(grep); // user `true` beats managed `false`
    }

    #[test]
    fn resolve_search_tool_enabled_precedence() {
        // args: disable, requirement, env, config, managed
        assert!(resolve_search_tool_enabled(None, None, None, None, None)); // default on
        // Org requirement wins outright — even over the user DISABLE kill-switch.
        assert!(resolve_search_tool_enabled(
            Some(true),
            Some(true),
            Some(false),
            Some(false),
            Some(false)
        ));
        // With no requirement, a truthy DISABLE master forces off over env/config.
        assert!(!resolve_search_tool_enabled(
            Some(true),
            None,
            Some(true),
            Some(true),
            Some(true)
        ));
        // falsey DISABLE is ignored.
        assert!(resolve_search_tool_enabled(
            Some(false),
            None,
            None,
            None,
            None
        ));
        // requirement(false) forces off even when lower tiers say on.
        assert!(!resolve_search_tool_enabled(
            None,
            Some(false),
            Some(true),
            Some(true),
            Some(true)
        ));
        // env beats config/managed.
        assert!(!resolve_search_tool_enabled(
            None,
            None,
            Some(false),
            Some(true),
            Some(true)
        ));
        // config beats managed; managed is last before default.
        assert!(!resolve_search_tool_enabled(
            None,
            None,
            None,
            Some(false),
            Some(true)
        ));
        assert!(!resolve_search_tool_enabled(
            None,
            None,
            None,
            None,
            Some(false)
        ));
    }
}

#[cfg(test)]
mod shell_env_policy_tests {
    use super::*;
    use xai_grok_tools::util::{EnvironmentVariablePattern, ShellEnvironmentPolicyInherit};

    #[test]
    fn resolve_shell_env_policy_absent_parsed_typo_and_typed_error() {
        // Absent table → None (child inherits the full environment).
        let empty: TomlValue = toml::from_str("").unwrap();
        assert!(resolve_shell_env_policy(Some(&empty)).is_none());
        assert!(resolve_shell_env_policy(None).is_none());

        // A well-formed table parses through.
        let cfg: TomlValue =
            toml::from_str("[shell_environment_policy]\ninherit = \"core\"\nexclude = [\"FOO\"]\n")
                .unwrap();
        let policy = resolve_shell_env_policy(Some(&cfg)).expect("policy parses");
        assert_eq!(policy.inherit, ShellEnvironmentPolicyInherit::Core);
        assert_eq!(
            policy.exclude,
            vec![EnvironmentVariablePattern::new_case_insensitive("FOO")]
        );

        // An unknown sub-key is ignored; the known keys still apply (the
        // load-time scan warns on the typo).
        let typo: TomlValue =
            toml::from_str("[shell_environment_policy]\ninherit = \"none\"\ninhert = \"core\"\n")
                .unwrap();
        let policy = resolve_shell_env_policy(Some(&typo)).expect("known keys still parse");
        assert_eq!(policy.inherit, ShellEnvironmentPolicyInherit::None);

        // A wrong-typed known key fails to parse → None (full environment,
        // logged), not a spawn abort.
        let bad: TomlValue =
            toml::from_str("[shell_environment_policy]\nexclude = \"not-an-array\"\n").unwrap();
        assert!(resolve_shell_env_policy(Some(&bad)).is_none());
    }
}
