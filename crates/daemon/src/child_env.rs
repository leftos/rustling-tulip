//! The environment a spawned child gets, composed from the app's keep-list,
//! the agent's passthrough variables, the spawn's resolved rows and the
//! agent's [`EnvPlan`].
//!
//! Only a headless child starts from an empty environment and so gets
//! nothing else; an interactive or plain-shell child runs under the tracer,
//! which inherits the daemon's whole environment, so there the list only
//! sets values on top of what the child inherits.

use agent_api::{EnvPlan, ResolvedRows};

/// The variables every child gets from the daemon's environment when it has
/// them, whatever its agent.
const KEEP: [&str; 10] = [
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
];

/// The terminal type every child gets.
const TERM: (&str, &str) = ("TERM", "xterm-256color");

/// The child's environment: the keep-list and the agent's `passthrough`
/// variables, each as `read` finds it in the daemon's environment, with
/// `TERM` set to `xterm-256color`; then the resolved `rows`; then the plan's
/// `set`; without any name in the plan's `remove`. Each name appears once,
/// matched case-insensitively, a later value replacing an earlier one.
pub(crate) fn compose(
    passthrough: &[&str],
    rows: &ResolvedRows,
    plan: &EnvPlan,
    read: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = KEEP
        .iter()
        .chain(passthrough)
        .filter_map(|name| read(name).map(|value| ((*name).to_owned(), value)))
        .collect();
    overlay(&mut env, [(TERM.0.to_owned(), TERM.1.to_owned())]);
    overlay(
        &mut env,
        rows.iter().map(|(k, v)| (k.to_owned(), v.to_owned())),
    );
    overlay(&mut env, plan.set.iter().cloned());
    env.retain(|(name, _)| {
        !plan
            .remove
            .iter()
            .any(|removed| removed.eq_ignore_ascii_case(name))
    });
    env
}

/// Put each of `pairs` into `env`, replacing the value of a name already
/// there (matched case-insensitively), else appending it.
fn overlay(env: &mut Vec<(String, String)>, pairs: impl IntoIterator<Item = (String, String)>) {
    for (name, value) in pairs {
        if let Some(slot) = env
            .iter_mut()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(&name))
        {
            slot.1 = value;
        } else {
            env.push((name, value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_PASSTHROUGH: &[&str] = &["ANTHROPIC_API_KEY", "CLAUDE_CONFIG_DIR"];

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// A daemon environment holding `TERM`, `PATH` and an Anthropic key.
    fn daemon_env(name: &str) -> Option<String> {
        match name {
            "TERM" => Some("dumb".to_owned()),
            "PATH" => Some("C:/bin".to_owned()),
            "ANTHROPIC_API_KEY" => Some("sk-ant-daemon".to_owned()),
            _ => None,
        }
    }

    fn value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    fn no_rows() -> ResolvedRows {
        ResolvedRows::new(Vec::new())
    }

    #[test]
    fn term_is_set_once() {
        for read in [daemon_env, |_: &str| None] {
            let env = compose(&[], &no_rows(), &EnvPlan::default(), read);
            let terms: Vec<&str> = env
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("TERM"))
                .map(|(_, v)| v.as_str())
                .collect();
            assert_eq!(terms, ["xterm-256color"], "{env:?}");
        }
    }

    #[test]
    fn a_plugin_without_passthrough_gets_no_anthropic_api_key() {
        let env = compose(&[], &no_rows(), &EnvPlan::default(), daemon_env);
        assert_eq!(value(&env, "ANTHROPIC_API_KEY"), None, "{env:?}");
        assert_eq!(value(&env, "PATH"), Some("C:/bin"));
        let env = compose(
            CLAUDE_PASSTHROUGH,
            &no_rows(),
            &EnvPlan::default(),
            daemon_env,
        );
        assert_eq!(value(&env, "ANTHROPIC_API_KEY"), Some("sk-ant-daemon"));
        assert_eq!(
            value(&env, "CLAUDE_CONFIG_DIR"),
            None,
            "unset in the daemon"
        );
    }

    #[test]
    fn remove_drops_a_name_the_rows_set() {
        let rows = ResolvedRows::new(pairs(&[("anthropic_api_key", "row"), ("RT_ROW", "1")]));
        let plan = EnvPlan {
            set: Vec::new(),
            remove: vec!["ANTHROPIC_API_KEY".to_owned()],
        };
        let env = compose(CLAUDE_PASSTHROUGH, &rows, &plan, daemon_env);
        assert_eq!(value(&env, "ANTHROPIC_API_KEY"), None, "{env:?}");
        assert_eq!(value(&env, "RT_ROW"), Some("1"));
    }

    #[test]
    fn plan_set_overrides_a_row() {
        let rows = ResolvedRows::new(pairs(&[
            ("Anthropic_Model", "row-model"),
            ("PATH", "X:/row"),
        ]));
        let plan = EnvPlan {
            set: pairs(&[("ANTHROPIC_MODEL", "plan-model")]),
            remove: Vec::new(),
        };
        let env = compose(&[], &rows, &plan, daemon_env);
        let models: Vec<&str> = env
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("ANTHROPIC_MODEL"))
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(models, ["plan-model"], "{env:?}");
        assert_eq!(
            value(&env, "PATH"),
            Some("X:/row"),
            "a row overrides the keep-list"
        );
    }
}
