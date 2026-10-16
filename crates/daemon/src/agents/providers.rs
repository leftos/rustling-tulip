//! The provider table: the environment a Claude session gets for the service
//! its `claude` CLI talks to.
//!
//! Anthropic has no entry: its spawns get the daemon's keep-list plus the
//! spawn's own rows, as they always have. A non-Anthropic provider sets its
//! routing variables, removes the variables that would point the CLI at
//! another endpoint or credential, and reads its key at each spawn. The
//! routing variables and the key are built here, after the stored config was
//! captured, so they never reach `meta.json`, history, `state.json` or the
//! wire.

use protocol::{ClaudeProvider, SessionMode};
use tracing::debug;

use crate::spawn_plan::SpawnFailure;
use crate::user_env::{Origin, Secret};

/// One non-Anthropic provider's routing.
struct ProviderEntry {
    /// The provider's name as the user reads it.
    display: &'static str,
    /// The variable the provider's key is read from (see [`crate::user_env`]).
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
    /// The model passed as `--model` when the spawn names none.
    default_model: &'static str,
    /// The start of every model name the provider serves, matched
    /// case-insensitively, so the importer can tell its sessions by `--model`.
    model_prefix: &'static str,
}

const DEEPSEEK_MODEL: &str = "deepseek-flash";

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
    default_model: DEEPSEEK_MODEL,
    model_prefix: "deepseek",
};

/// Every non-Anthropic provider with its table entry.
const TABLE: [(ClaudeProvider, &ProviderEntry); 1] = [(ClaudeProvider::Deepseek, &DEEPSEEK)];

/// The environment a spawn's child gets: `set` on top of what it inherits,
/// with every name in `remove` taken away.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SpawnEnv {
    /// Variables the child gets, in order, each name once.
    pub set: Vec<(String, String)>,
    /// Variables the child must not get, even when the daemon has them.
    pub remove: Vec<String>,
}

/// Shows the variable names only: `set` holds resolved secrets and keys.
impl std::fmt::Debug for SpawnEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set_keys: Vec<&str> = self.set.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("SpawnEnv")
            .field("set_keys", &set_keys)
            .field("remove", &self.remove)
            .finish()
    }
}

/// The table entry for `provider`: none for Anthropic, a refusal for a
/// provider this daemon does not know.
fn entry(provider: ClaudeProvider) -> Result<Option<&'static ProviderEntry>, SpawnFailure> {
    if provider.is_anthropic() {
        return Ok(None);
    }
    TABLE
        .iter()
        .find(|(listed, _)| *listed == provider)
        .map(|(_, entry)| Some(*entry))
        .ok_or_else(|| SpawnFailure {
            title: "Couldn't start the Claude session".to_owned(),
            detail: "This daemon does not know the requested Claude provider.".to_owned(),
            hint: None,
        })
}

/// The model a spawn on `provider` passes as `--model` when it names none.
#[must_use]
pub fn default_model(provider: ClaudeProvider) -> Option<&'static str> {
    entry(provider)
        .ok()
        .flatten()
        .map(|entry| entry.default_model)
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

/// Refuse a spawn request before anything is written or checked out: a plain
/// shell asking for a non-Anthropic provider, an unknown provider, or an
/// environment row setting one of the provider's locked or removed
/// variables. Reads the rows' keys only, so it may run before the rows are
/// sealed.
pub fn check_request(
    mode: SessionMode,
    provider: ClaudeProvider,
    rows: &[(String, String)],
) -> Result<(), SpawnFailure> {
    if mode == SessionMode::PlainShell && !provider.is_anthropic() {
        return Err(SpawnFailure {
            title: "Couldn't start the shell session".to_owned(),
            detail: "Plain shell sessions do not accept a Claude provider.".to_owned(),
            hint: None,
        });
    }
    check_rows(provider, rows)
}

/// Refuse an unknown `provider`, or a row setting one of its locked
/// variables or a variable it removes from the child. Names the variable,
/// never a row's value.
fn check_rows(provider: ClaudeProvider, rows: &[(String, String)]) -> Result<(), SpawnFailure> {
    let Some(entry) = entry(provider)? else {
        return Ok(());
    };
    let refusal = |detail: String| SpawnFailure {
        title: format!("Couldn't start the {} session", entry.display),
        detail,
        hint: None,
    };
    for (key, _) in rows {
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

/// The environment for a Claude spawn on `provider` whose rows, already
/// resolved, are `extra_env`.
///
/// Anthropic: the keep-list, then the rows, as [`merged_env`] builds it. A
/// table provider: the keep-list, then its tuning defaults, then the rows,
/// then its locked variables with the key read through `resolve`, with its
/// removed variables dropped from `set` and listed in `remove`.
///
/// Refuses an unknown provider, a row setting a locked or removed variable,
/// and a key `resolve` cannot find, naming the variable and never a value.
/// A removed variable can still reach `set` from the keep-list, so it is
/// dropped there.
pub fn spawn_env(
    provider: ClaudeProvider,
    extra_env: &[(String, String)],
    resolve: impl FnOnce(&str) -> Option<(Secret, Origin)>,
) -> Result<SpawnEnv, SpawnFailure> {
    check_rows(provider, extra_env)?;
    let Some(entry) = entry(provider)? else {
        return Ok(SpawnEnv {
            set: merged_env(extra_env),
            remove: Vec::new(),
        });
    };
    let Some((key, origin)) = resolve(entry.key_var) else {
        return Err(missing_key(entry));
    };
    debug!(
        provider = entry.display,
        key = entry.key_var,
        ?origin,
        "provider key resolved"
    );
    let pairs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    };
    let mut set = passthrough_env();
    overlay(&mut set, pairs(entry.tuning));
    overlay(&mut set, extra_env.iter().cloned());
    overlay(&mut set, pairs(entry.fixed));
    overlay(
        &mut set,
        [(entry.token_var.to_owned(), key.expose().to_owned())],
    );
    set.retain(|(k, _)| named(entry.remove, k).is_none());
    Ok(SpawnEnv {
        set,
        remove: entry.remove.iter().map(|name| (*name).to_owned()).collect(),
    })
}

/// Refuse a spawn on `provider` whose key `resolve` cannot find, without
/// holding on to the key when it can.
pub fn check_key(
    provider: ClaudeProvider,
    resolve: impl FnOnce(&str) -> Option<(Secret, Origin)>,
) -> Result<(), SpawnFailure> {
    match entry(provider)? {
        Some(entry) if resolve(entry.key_var).is_none() => Err(missing_key(entry)),
        _ => Ok(()),
    }
}

/// The refusal naming `entry`'s key variable, never a value.
fn missing_key(entry: &ProviderEntry) -> SpawnFailure {
    SpawnFailure {
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

/// The daemon's keep-list: the variables a headless child (which starts from
/// an empty environment) needs to run.
fn passthrough_env() -> Vec<(String, String)> {
    let keep = [
        "PATH",
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "TEMP",
        "TMP",
        "TERM",
        "LANG",
        "LC_ALL",
        "ANTHROPIC_API_KEY",
        "CLAUDE_CONFIG_DIR",
    ];
    let mut out = Vec::new();
    for k in keep {
        if let Ok(v) = std::env::var(k) {
            out.push((k.to_string(), v));
        }
    }
    out.push(("TERM".to_string(), "xterm-256color".to_string()));
    out
}

/// Put each of `pairs` into `env`, replacing the value of a name already
/// there, so each name appears once.
fn overlay(env: &mut Vec<(String, String)>, pairs: impl IntoIterator<Item = (String, String)>) {
    for (k, v) in pairs {
        if let Some(slot) = env.iter_mut().find(|(existing, _)| *existing == k) {
            slot.1 = v;
        } else {
            env.push((k, v));
        }
    }
}

/// The daemon's keep-list plus the spawn's `extra_env` keys, with
/// `extra_env` overriding the keep-list on key collision. `extra` holds
/// values already resolved by `server::resolve_env_refs`, so env references
/// never reach here.
fn merged_env(extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = passthrough_env();
    overlay(&mut out, extra.iter().cloned());
    out
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

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

    fn value<'a>(env: &'a SpawnEnv, key: &str) -> Option<&'a str> {
        env.set
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn failure_text(failure: &SpawnFailure) -> String {
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
        let env = spawn_env(ClaudeProvider::Anthropic, &extra, |_| None).expect("anthropic builds");
        assert_eq!(env.set, merged_env(&extra));
        assert!(env.remove.is_empty(), "{env:?}");
    }

    #[test]
    fn deepseek_sets_routing_and_removes_anthropic_api_key() {
        let extra = rows(&[("RT_ROW", "1")]);
        let env = spawn_env(ClaudeProvider::Deepseek, &extra, with_key).expect("deepseek builds");
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
        let env = spawn_env(
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
                let failure = spawn_env(ClaudeProvider::Deepseek, &extra, with_key)
                    .expect_err("a locked row refuses");
                assert!(
                    failure.detail.contains(&format!("`{locked}`")),
                    "{key}: {}",
                    failure.detail
                );
                assert!(failure.detail.contains("DeepSeek"), "{}", failure.detail);
                assert!(!failure_text(&failure).contains("row-value"));
                assert!(
                    check_rows(ClaudeProvider::Deepseek, &extra).is_err(),
                    "{key}"
                );
                assert!(
                    spawn_env(ClaudeProvider::Anthropic, &extra, |_| None).is_ok(),
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
                let failure = check_rows(ClaudeProvider::Deepseek, &extra)
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
                    spawn_env(ClaudeProvider::Deepseek, &extra, with_key).is_err(),
                    "{key}"
                );
                assert!(
                    check_request(SessionMode::Interactive, ClaudeProvider::Anthropic, &extra)
                        .is_ok(),
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
        let failure = spawn_env(ClaudeProvider::Deepseek, &extra, |_| None)
            .expect_err("a missing key refuses");
        let text = failure_text(&failure);
        assert_eq!(failure.title, "Couldn't start the DeepSeek session");
        assert!(failure.detail.contains("`DEEPSEEK_API_KEY`"), "{text}");
        assert!(failure.detail.contains("system environment"), "{text}");
        assert!(!text.contains("row-sentinel-value"), "{text}");
    }

    #[test]
    fn unknown_provider_refuses() {
        let failure = spawn_env(ClaudeProvider::Unknown, &[], with_key)
            .expect_err("an unknown provider refuses");
        assert!(
            failure.detail.contains("Claude provider"),
            "{}",
            failure.detail
        );
        assert!(check_rows(ClaudeProvider::Unknown, &[]).is_err());
        assert_eq!(default_model(ClaudeProvider::Unknown), None);
    }

    #[test]
    fn debug_of_deepseek_spawn_env_holds_no_key_text() {
        let env = spawn_env(ClaudeProvider::Deepseek, &[], with_key).expect("deepseek builds");
        assert_eq!(value(&env, "ANTHROPIC_AUTH_TOKEN"), Some(KEY_SENTINEL));
        let shown = format!("{env:?}");
        assert!(shown.contains("ANTHROPIC_AUTH_TOKEN"), "{shown}");
        assert!(!shown.contains(KEY_SENTINEL), "{shown}");
    }
}
