//! Whether this launch is a driven run, decided before it touches any
//! folder: compiled in every build, since a build without the `driver`
//! feature must refuse a driven run too.

use daemon_client::isolation::check_isolated;

use crate::offscreen::OFFSCREEN_ENV;

/// Set to `1`, the client opens its driver endpoint; unset or empty, it runs
/// as usual. Any other value refuses the run.
pub const DRIVER_ENV: &str = "RUSTLING_TULIP_DRIVER";

/// What the environment asks of this launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// No driven run: the client runs as usual.
    Off,
    /// A driven run whose every condition holds.
    Drive,
    /// A driven run that must not start, and why, in one line.
    Refuse(String),
}

/// Decides from the variables `get` reads whether this launch is a driven
/// run. `built_with_driver` is whether the build has the `driver` feature,
/// and `override_vars` names each registered agent's binary override.
///
/// A driven run (`RUSTLING_TULIP_DRIVER=1`) needs the driver built in, a
/// cloaked window, isolated folders and every agent override set; any one
/// missing refuses it.
pub fn decide(
    get: impl Fn(&str) -> Option<String>,
    built_with_driver: bool,
    override_vars: &[&str],
) -> GateDecision {
    let set = |variable: &str| get(variable).is_some_and(|value| !value.is_empty());
    match get(DRIVER_ENV).as_deref() {
        None | Some("") => return GateDecision::Off,
        Some("1") => {}
        Some(other) => {
            return GateDecision::Refuse(format!(
                "{DRIVER_ENV} is {other:?}; set it to 1 for a driven run, or leave it unset"
            ));
        }
    }
    if !built_with_driver {
        return GateDecision::Refuse(format!(
            "{DRIVER_ENV}=1, but this build has no driver endpoint (it needs Windows and --features driver)"
        ));
    }
    if !set(OFFSCREEN_ENV) {
        return GateDecision::Refuse(format!(
            "{DRIVER_ENV}=1 needs {OFFSCREEN_ENV} set, so the driven window stays cloaked"
        ));
    }
    if let Err(err) = check_isolated(|variable| get(variable).map(Into::into)) {
        return GateDecision::Refuse(err.to_string());
    }
    if let Some(unset) = override_vars.iter().find(|variable| !set(variable)) {
        return GateDecision::Refuse(format!(
            "{DRIVER_ENV}=1 needs {unset} set, so a driven run never starts a real agent CLI"
        ));
    }
    GateDecision::Drive
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use daemon_client::isolation::ISOLATED_DIR_VARS;

    use super::{DRIVER_ENV, GateDecision, OFFSCREEN_ENV, decide};

    /// A stand-in for a registered agent's binary override variable.
    const AGENT_OVERRIDE_ENV: &str = "RUSTLING_TULIP_TEST_AGENT";

    /// An absolute path made of `parts` below the filesystem root.
    fn absolute(parts: &[&str]) -> String {
        let root = if cfg!(windows) { "C:\\" } else { "/" };
        parts
            .iter()
            .fold(PathBuf::from(root), |path, part| path.join(part))
            .to_string_lossy()
            .into_owned()
    }

    /// Every condition of a driven run met, with `overrides` on top (`None`
    /// unsets the variable).
    fn env(overrides: &[(&str, Option<&str>)]) -> HashMap<String, String> {
        let mut vars: HashMap<String, String> = ISOLATED_DIR_VARS
            .iter()
            .map(|var| ((*var).to_owned(), absolute(&["repo", ".tmp", "drive", var])))
            .collect();
        vars.insert(DRIVER_ENV.to_owned(), "1".to_owned());
        vars.insert(OFFSCREEN_ENV.to_owned(), "1".to_owned());
        vars.insert(
            AGENT_OVERRIDE_ENV.to_owned(),
            absolute(&["repo", "fake.cmd"]),
        );
        for (var, value) in overrides {
            match value {
                Some(value) => vars.insert((*var).to_owned(), (*value).to_owned()),
                None => vars.remove(*var),
            };
        }
        vars
    }

    fn gate(vars: &HashMap<String, String>, built_with_driver: bool) -> GateDecision {
        decide(
            |var| vars.get(var).cloned(),
            built_with_driver,
            &[AGENT_OVERRIDE_ENV],
        )
    }

    fn refusal(decision: GateDecision) -> String {
        match decision {
            GateDecision::Refuse(reason) => reason,
            other => format!("not refused: {other:?}"),
        }
    }

    #[test]
    fn driver_unset_or_empty_is_off() {
        assert_eq!(gate(&env(&[(DRIVER_ENV, None)]), true), GateDecision::Off);
        assert_eq!(
            gate(&env(&[(DRIVER_ENV, Some(""))]), true),
            GateDecision::Off
        );
        assert_eq!(
            gate(&HashMap::new(), false),
            GateDecision::Off,
            "nothing set is an ordinary launch"
        );
    }

    #[test]
    fn driver_with_another_value_is_refused() {
        for value in ["0", "true", " 1", "yes"] {
            let reason = refusal(gate(&env(&[(DRIVER_ENV, Some(value))]), true));
            assert!(reason.contains(DRIVER_ENV), "{value:?}: {reason}");
            assert!(reason.contains("set it to 1"), "{value:?}: {reason}");
        }
    }

    #[test]
    fn driver_without_the_feature_is_refused() {
        let reason = refusal(gate(&env(&[]), false));
        assert!(
            reason.contains("no driver endpoint (it needs Windows and --features driver)"),
            "{reason}"
        );
    }

    #[test]
    fn driver_without_the_offscreen_window_is_refused() {
        for value in [None, Some("")] {
            let reason = refusal(gate(&env(&[(OFFSCREEN_ENV, value)]), true));
            assert!(reason.contains(OFFSCREEN_ENV), "{value:?}: {reason}");
        }
    }

    #[test]
    fn driver_outside_isolated_dirs_is_refused() {
        let outside = absolute(&["Users", "me", "AppData", "rustling-tulip"]);
        let vars = env(&[("RUSTLING_TULIP_CONFIG_DIR", Some(&outside))]);
        let reason = refusal(gate(&vars, true));
        assert!(
            reason.contains("RUSTLING_TULIP_CONFIG_DIR")
                && reason.contains("is not under a .tmp folder"),
            "{reason}"
        );

        let vars = env(&[("RUSTLING_TULIP_WORKTREES_DIR", None)]);
        let reason = refusal(gate(&vars, true));
        assert!(
            reason.contains("RUSTLING_TULIP_WORKTREES_DIR is not set"),
            "{reason}"
        );
    }

    #[test]
    fn driver_without_an_agent_override_is_refused() {
        for value in [None, Some("")] {
            let reason = refusal(gate(&env(&[(AGENT_OVERRIDE_ENV, value)]), true));
            assert!(reason.contains(AGENT_OVERRIDE_ENV), "{value:?}: {reason}");
        }
    }

    #[test]
    fn driver_with_every_condition_drives() {
        assert_eq!(gate(&env(&[]), true), GateDecision::Drive);
    }
}
