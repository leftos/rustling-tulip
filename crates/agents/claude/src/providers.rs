//! The provider table: the environment a Claude session gets for the service
//! its `claude` CLI talks to.
//!
//! Anthropic has no entry: its spawns get the app's keep-list, Claude's
//! passthrough variables and the spawn's own rows, as they always have. A
//! non-Anthropic provider sets its routing variables, removes the variables
//! that would point the CLI at another endpoint or credential, and reads its
//! key at each spawn. The routing variables and the key are built after the
//! stored config was captured, so they never reach `meta.json`, history,
//! `state.json` or the wire.

use agent_api::{EnvPlan, KeyLookup, Refusal, ResolvedRows, SpawnCheck};
use tracing::debug;

use crate::DEEPSEEK_MODEL;
use crate::options::ClaudeProvider;

/// One non-Anthropic provider's routing.
struct ProviderEntry {
    /// The provider's name as the user reads it.
    display: &'static str,
    /// The variable the provider's key is read from (through the app's
    /// [`KeyLookup`]).
    key_var: &'static str,
    /// The variable the key is passed to the child under.
    token_var: &'static str,
    /// Routing variables set to fixed values. A row may not set them.
    fixed: &'static [(&'static str, &'static str)],
    /// Variables an environment row may not set: they choose the endpoint
    /// or the credential.
    locked: &'static [&'static str],
    /// Variables the child must not get, from the keep-list or the daemon's
    /// own environment the tracer inherits. A row may not set them.
    remove: &'static [&'static str],
    /// Defaults an environment row overrides.
    tuning: &'static [(&'static str, &'static str)],
    /// The start of every model name the provider serves, matched
    /// case-insensitively, so the importer can tell its sessions by `--model`.
    model_prefix: &'static str,
}

const DEEPSEEK: ProviderEntry = ProviderEntry {
    display: "DeepSeek",
    key_var: "DEEPSEEK_API_KEY",
    token_var: "ANTHROPIC_AUTH_TOKEN",
    fixed: &[
        ("ANTHROPIC_BASE_URL", "https://api.deepseek.com/anthropic"),
        ("CLAUDE_DEEPSEEK_SESSION", "1"),
    ],
    locked: &[
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "CLAUDE_DEEPSEEK_SESSION",
        "DEEPSEEK_API_KEY",
    ],
    remove: &[
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "ANTHROPIC_CUSTOM_HEADERS",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ],
    tuning: &[
        ("ANTHROPIC_MODEL", DEEPSEEK_MODEL),
        ("ANTHROPIC_DEFAULT_FABLE_MODEL", DEEPSEEK_MODEL),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", DEEPSEEK_MODEL),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", DEEPSEEK_MODEL),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", DEEPSEEK_MODEL),
        ("CLAUDE_CODE_MAX_CONTEXT_TOKENS", "1000000"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ],
    model_prefix: "deepseek",
};

/// Every non-Anthropic provider with its table entry.
const TABLE: [(ClaudeProvider, &ProviderEntry); 1] = [(ClaudeProvider::Deepseek, &DEEPSEEK)];

/// The table entry for `provider`: none for Anthropic, a refusal for a
/// provider this daemon does not know.
fn entry(provider: ClaudeProvider) -> Result<Option<&'static ProviderEntry>, Refusal> {
    if provider.is_anthropic() {
        return Ok(None);
    }
    TABLE
        .iter()
        .find(|(listed, _)| *listed == provider)
        .map(|(_, entry)| Some(*entry))
        .ok_or_else(|| Refusal {
            title: "Couldn't start the Claude session".to_owned(),
            detail: "This daemon does not know the requested Claude provider.".to_owned(),
            hint: None,
        })
}

/// The name the user reads for `provider`: none for Anthropic and for a
/// provider this daemon does not know.
#[must_use]
pub fn display_name(provider: ClaudeProvider) -> Option<&'static str> {
    entry(provider).ok().flatten().map(|entry| entry.display)
}

/// The first provider in the table whose model prefix `model` starts with,
/// case-insensitively, else Anthropic. The tracer-log importer reads a
/// session's provider this way from its `--model`.
#[must_use]
pub fn provider_for_model(model: &str) -> ClaudeProvider {
    TABLE
        .iter()
        .find(|(_, entry)| {
            model
                .get(..entry.model_prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(entry.model_prefix))
        })
        .map_or(ClaudeProvider::Anthropic, |(provider, _)| *provider)
}

/// The name in `list` that `key` matches. Windows environment names are
/// case-insensitive, so the match is too.
fn named(list: &[&'static str], key: &str) -> Option<&'static str> {
    list.iter()
        .find(|name| name.eq_ignore_ascii_case(key))
        .copied()
}

/// Refuse a spawn request on `provider` before anything is written or
/// checked out: an unknown provider, an environment row setting one of the
/// provider's locked or removed variables, then a key `req.keys` cannot
/// find. Reads the rows' names only, so it may run before the rows are
/// sealed, and drops the key when it finds it.
pub(crate) fn check_request(provider: ClaudeProvider, req: &SpawnCheck<'_>) -> Result<(), Refusal> {
    check_rows(provider, req.rows.iter().map(|(key, _)| key.as_str()))?;
    match entry(provider)? {
        Some(entry) if req.keys.key(entry.key_var).is_none() => Err(missing_key(entry)),
        _ => Ok(()),
    }
}

/// Refuse an unknown `provider`, or a row named in `names` setting one of
/// its locked variables or a variable it removes from the child. Names the
/// variable, never a row's value.
fn check_rows<'r>(
    provider: ClaudeProvider,
    names: impl IntoIterator<Item = &'r str>,
) -> Result<(), Refusal> {
    let Some(entry) = entry(provider)? else {
        return Ok(());
    };
    let refusal = |detail: String| Refusal {
        title: format!("Couldn't start the {} session", entry.display),
        detail,
        hint: None,
    };
    for key in names {
        if let Some(locked) = named(entry.locked, key) {
            return Err(refusal(format!(
                "`{locked}` is set by the {} provider; remove it from the environment rows.",
                entry.display
            )));
        }
        if let Some(removed) = named(entry.remove, key) {
            return Err(refusal(format!(
                "`{removed}` is removed from {} sessions; remove it from the environment rows.",
                entry.display
            )));
        }
    }
    Ok(())
}

/// Claude's part of the environment for a spawn on `provider` whose rows,
/// already resolved, are `rows`.
///
/// Anthropic: nothing; the app's keep-list, Claude's passthrough variables
/// and the rows are the whole environment. A table provider: its tuning
/// defaults a row does not name (so a row overrides a default), then its
/// fixed routing variables, then its key read through `keys` under the
/// token variable; its removed variables are listed in `remove`.
///
/// Refuses an unknown provider, a row setting a locked or removed variable,
/// and a key `keys` cannot find, naming the variable and never a value.
pub(crate) fn spawn_env(
    provider: ClaudeProvider,
    rows: &ResolvedRows,
    keys: &dyn KeyLookup,
) -> Result<EnvPlan, Refusal> {
    check_rows(provider, rows.iter().map(|(key, _)| key))?;
    let Some(entry) = entry(provider)? else {
        return Ok(EnvPlan::default());
    };
    let Some((key, origin)) = keys.key(entry.key_var) else {
        return Err(missing_key(entry));
    };
    debug!(
        provider = entry.display,
        key = entry.key_var,
        ?origin,
        "provider key resolved"
    );
    let pair = |(k, v): &(&str, &str)| ((*k).to_owned(), (*v).to_owned());
    let mut set: Vec<(String, String)> = entry
        .tuning
        .iter()
        .filter(|(name, _)| !rows.contains(name))
        .map(pair)
        .collect();
    set.extend(entry.fixed.iter().map(pair));
    set.push((entry.token_var.to_owned(), key.expose().to_owned()));
    Ok(EnvPlan {
        set,
        remove: entry.remove.iter().map(|name| (*name).to_owned()).collect(),
    })
}

/// The refusal naming `entry`'s key variable, never a value.
fn missing_key(entry: &ProviderEntry) -> Refusal {
    Refusal {
        title: format!("Couldn't start the {} session", entry.display),
        detail: format!(
            "`{}` isn't set in the daemon's environment, your user environment or the system environment.",
            entry.key_var
        ),
        hint: Some(format!(
            "Set it with `setx {} <key>`; the next spawn reads it, no restart needed.",
            entry.key_var
        )),
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use agent_api::{Origin, Secret};
    use protocol::SessionMode;

    /// A key value no output may ever show.
    const KEY_SENTINEL: &str = "sk-deepseek-sentinel-5e17c2";

    fn rows(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn with_key(name: &str) -> Option<(Secret, Origin)> {
        (name == "DEEPSEEK_API_KEY")
            .then(|| (Secret::new(KEY_SENTINEL.to_owned()), Origin::UserScope))
    }

    fn names(rows: &[(String, String)]) -> impl Iterator<Item = &str> {
        rows.iter().map(|(key, _)| key.as_str())
    }

    /// [`check_request`] on an interactive spawn whose rows are `extra`.
    fn check(provider: ClaudeProvider, extra: &[(String, String)]) -> Result<(), Refusal> {
        check_request(
            provider,
            &SpawnCheck {
                mode: SessionMode::Interactive,
                rows: extra,
                keys: &with_key,
            },
        )
    }

    /// The plan [`spawn_env`] builds for `extra`, composed over the rows as
    /// the app composes it (the rows, then the plan's `set`, without the
    /// plan's `remove` names) but without the app's keep-list.
    fn child_env(
        provider: ClaudeProvider,
        extra: &[(String, String)],
        keys: impl Fn(&str) -> Option<(Secret, Origin)>,
    ) -> Result<EnvPlan, Refusal> {
        let plan = spawn_env(provider, &ResolvedRows::new(extra.to_vec()), &keys)?;
        let mut set = extra.to_vec();
        for (k, v) in plan.set {
            if let Some(slot) = set
                .iter_mut()
                .find(|(existing, _)| existing.eq_ignore_ascii_case(&k))
            {
                slot.1 = v;
            } else {
                set.push((k, v));
            }
        }
        set.retain(|(k, _)| !plan.remove.iter().any(|r| r.eq_ignore_ascii_case(k)));
        Ok(EnvPlan {
            set,
            remove: plan.remove,
        })
    }

    fn value<'a>(env: &'a EnvPlan, key: &str) -> Option<&'a str> {
        env.set
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn failure_text(failure: &Refusal) -> String {
        format!(
            "{} {} {}",
            failure.title,
            failure.detail,
            failure.hint.as_deref().unwrap_or_default()
        )
    }

    #[test]
    fn anthropic_set_equals_today_merged_env() {
        let extra = rows(&[
            ("PATH", "X:/override"),
            ("RT_ROW", "1"),
            ("ANTHROPIC_BASE_URL", "u"),
        ]);
        let plan = spawn_env(
            ClaudeProvider::Anthropic,
            &ResolvedRows::new(extra.clone()),
            &|_: &str| None,
        )
        .expect("anthropic builds");
        assert_eq!(plan, EnvPlan::default());
        let env = child_env(ClaudeProvider::Anthropic, &extra, |_| None).expect("anthropic builds");
        assert_eq!(env.set, extra);
        assert!(env.remove.is_empty(), "{env:?}");
    }

    #[test]
    fn deepseek_sets_routing_and_removes_anthropic_api_key() {
        let extra = rows(&[("RT_ROW", "1")]);
        let env = child_env(ClaudeProvider::Deepseek, &extra, with_key).expect("deepseek builds");
        for (key, expected) in DEEPSEEK.fixed.iter().chain(DEEPSEEK.tuning) {
            assert_eq!(value(&env, key), Some(*expected), "{key}");
        }
        assert_eq!(value(&env, "ANTHROPIC_AUTH_TOKEN"), Some(KEY_SENTINEL));
        assert_eq!(value(&env, "RT_ROW"), Some("1"));
        let holding_key: Vec<&str> = env
            .set
            .iter()
            .filter(|(_, v)| v.contains(KEY_SENTINEL))
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(holding_key, ["ANTHROPIC_AUTH_TOKEN"]);
        assert_eq!(value(&env, "ANTHROPIC_API_KEY"), None);
        for name in DEEPSEEK.remove {
            assert!(env.remove.iter().any(|r| r == name), "{name} in {env:?}");
            assert_eq!(value(&env, name), None, "{name}");
        }
        assert!(env.remove.iter().any(|r| r == "ANTHROPIC_API_KEY"));
    }

    #[test]
    fn extra_env_overrides_tuning_default() {
        let env = child_env(
            ClaudeProvider::Deepseek,
            &rows(&[("ANTHROPIC_MODEL", "deepseek-pro")]),
            with_key,
        )
        .expect("deepseek builds");
        assert_eq!(value(&env, "ANTHROPIC_MODEL"), Some("deepseek-pro"));
        assert_eq!(
            env.set
                .iter()
                .filter(|(k, _)| k == "ANTHROPIC_MODEL")
                .count(),
            1
        );
    }

    #[test]
    fn locked_key_in_extra_env_refuses() {
        for locked in DEEPSEEK.locked {
            let mixed: String = locked
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    if i % 2 == 0 {
                        c.to_ascii_lowercase()
                    } else {
                        c
                    }
                })
                .collect();
            for key in [(*locked).to_owned(), locked.to_ascii_lowercase(), mixed] {
                let extra = rows(&[(key.as_str(), "row-value")]);
                let failure = child_env(ClaudeProvider::Deepseek, &extra, with_key)
                    .expect_err("a locked row refuses");
                assert!(
                    failure.detail.contains(&format!("`{locked}`")),
                    "{key}: {}",
                    failure.detail
                );
                assert!(failure.detail.contains("DeepSeek"), "{}", failure.detail);
                assert!(!failure_text(&failure).contains("row-value"));
                assert!(
                    check_rows(ClaudeProvider::Deepseek, names(&extra)).is_err(),
                    "{key}"
                );
                assert!(
                    child_env(ClaudeProvider::Anthropic, &extra, |_| None).is_ok(),
                    "an Anthropic spawn keeps today's rows: {key}"
                );
            }
        }
    }

    #[test]
    fn removed_key_in_extra_env_refuses() {
        for removed in DEEPSEEK.remove {
            for key in [(*removed).to_owned(), removed.to_ascii_lowercase()] {
                let extra = rows(&[(key.as_str(), "row-value-sentinel")]);
                let failure = check_rows(ClaudeProvider::Deepseek, names(&extra))
                    .expect_err("a removed row refuses");
                assert_eq!(
                    failure.title, "Couldn't start the DeepSeek session",
                    "{key}"
                );
                assert!(
                    failure.detail.contains(&format!("`{removed}`")),
                    "{key}: {}",
                    failure.detail
                );
                if named(DEEPSEEK.locked, removed).is_none() {
                    assert_eq!(
                        failure.detail,
                        format!(
                            "`{removed}` is removed from DeepSeek sessions; remove it from the environment rows."
                        )
                    );
                }
                assert!(
                    !failure_text(&failure).contains("row-value-sentinel"),
                    "{key}"
                );
                assert!(
                    child_env(ClaudeProvider::Deepseek, &extra, with_key).is_err(),
                    "{key}"
                );
                assert!(
                    check(ClaudeProvider::Anthropic, &extra).is_ok(),
                    "an Anthropic spawn keeps today's rows: {key}"
                );
            }
        }
    }

    #[test]
    fn a_model_is_matched_to_its_provider_by_prefix() {
        for model in ["deepseek-flash", "deepseek-pro", "DeepSeek-Flash"] {
            assert_eq!(
                provider_for_model(model),
                ClaudeProvider::Deepseek,
                "{model}"
            );
        }
        for model in ["opus", "claude-sonnet-4-5", "", "deep"] {
            assert_eq!(
                provider_for_model(model),
                ClaudeProvider::Anthropic,
                "{model}"
            );
        }
    }

    #[test]
    fn missing_key_refuses_naming_the_variable_not_a_value() {
        let extra = rows(&[("RT_ROW", "row-sentinel-value")]);
        let failure = child_env(ClaudeProvider::Deepseek, &extra, |_| None)
            .expect_err("a missing key refuses");
        let text = failure_text(&failure);
        assert_eq!(failure.title, "Couldn't start the DeepSeek session");
        assert!(failure.detail.contains("`DEEPSEEK_API_KEY`"), "{text}");
        assert!(failure.detail.contains("system environment"), "{text}");
        assert!(!text.contains("row-sentinel-value"), "{text}");
    }

    #[test]
    fn unknown_provider_refuses() {
        let failure = child_env(ClaudeProvider::Unknown, &[], with_key)
            .expect_err("an unknown provider refuses");
        assert!(
            failure.detail.contains("Claude provider"),
            "{}",
            failure.detail
        );
        assert!(check_rows(ClaudeProvider::Unknown, names(&[])).is_err());
    }

    #[test]
    fn debug_of_deepseek_spawn_env_holds_no_key_text() {
        let env = child_env(ClaudeProvider::Deepseek, &[], with_key).expect("deepseek builds");
        assert_eq!(value(&env, "ANTHROPIC_AUTH_TOKEN"), Some(KEY_SENTINEL));
        let shown = format!("{env:?}");
        assert!(shown.contains("ANTHROPIC_AUTH_TOKEN"), "{shown}");
        assert!(!shown.contains(KEY_SENTINEL), "{shown}");
    }
}
