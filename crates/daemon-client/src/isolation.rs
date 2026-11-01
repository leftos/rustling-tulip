//! The check a driven run makes before it starts: every folder the client and
//! its daemon write to must be an isolated one under a `.tmp` folder, so a
//! run can never touch the owner's real config, binaries or worktrees.

use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// The variables naming the folders a driven run writes to, in the order
/// they are checked.
pub const ISOLATED_DIR_VARS: [&str; 3] = [
    "RUSTLING_TULIP_CONFIG_DIR",
    "RUSTLING_TULIP_BINARIES_DIR",
    "RUSTLING_TULIP_WORKTREES_DIR",
];

/// The folder name a driven run's folders must sit under.
const TMP_FOLDER: &str = ".tmp";

/// Why a variable of [`ISOLATED_DIR_VARS`] does not name an isolated folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The variable is not set.
    Unset,
    /// The variable is set to an empty value.
    Empty,
    /// The variable names a relative path.
    Relative(PathBuf),
    /// The variable names an absolute path with no `.tmp` folder above it.
    NotUnderTmp(PathBuf),
}

/// The first variable of [`ISOLATED_DIR_VARS`] that does not name an
/// isolated folder, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotIsolated {
    /// The variable's name.
    pub variable: &'static str,
    /// Why its value fails.
    pub reason: Reason,
}

impl fmt::Display for NotIsolated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variable = self.variable;
        match &self.reason {
            Reason::Unset => write!(f, "{variable} is not set"),
            Reason::Empty => write!(f, "{variable} is empty"),
            Reason::Relative(path) => {
                write!(f, "{variable} is a relative path ({})", path.display())
            }
            Reason::NotUnderTmp(path) => write!(
                f,
                "{variable} ({}) is not under a {TMP_FOLDER} folder",
                path.display()
            ),
        }?;
        write!(
            f,
            "; a driven run needs {} each set to an absolute folder under a {TMP_FOLDER} folder",
            ISOLATED_DIR_VARS.join(", ")
        )
    }
}

impl std::error::Error for NotIsolated {}

/// Passes only when each of [`ISOLATED_DIR_VARS`], read through `get`, is
/// set, non-empty, absolute and has a folder named exactly `.tmp` above it.
/// The check is lexical: nothing is read from disk.
///
/// # Errors
///
/// Returns the first variable that fails, in [`ISOLATED_DIR_VARS`] order,
/// with the reason.
pub fn check_isolated(get: impl Fn(&str) -> Option<OsString>) -> Result<(), NotIsolated> {
    for variable in ISOLATED_DIR_VARS {
        check_dir(get(variable)).map_err(|reason| NotIsolated { variable, reason })?;
    }
    Ok(())
}

/// Why `value` is not an isolated folder, if it is not.
fn check_dir(value: Option<OsString>) -> Result<(), Reason> {
    let value = value.ok_or(Reason::Unset)?;
    if value.is_empty() {
        return Err(Reason::Empty);
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(Reason::Relative(path));
    }
    if !under_tmp(&path) {
        return Err(Reason::NotUnderTmp(path));
    }
    Ok(())
}

/// Whether a folder named exactly `.tmp` sits above `path`'s last
/// component, with nothing after it that climbs back out (`..`).
fn under_tmp(path: &Path) -> bool {
    let components: Vec<Component<'_>> = path.components().collect();
    let Some((_, ancestors)) = components.split_last() else {
        return false;
    };
    let mut under = false;
    for component in ancestors {
        match component {
            Component::Normal(name) if *name == TMP_FOLDER => under = true,
            Component::ParentDir => under = false,
            _ => {}
        }
    }
    under && !matches!(components.last(), Some(Component::ParentDir))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// An absolute path made of `parts` below the filesystem root.
    fn absolute(parts: &[&str]) -> PathBuf {
        let root = if cfg!(windows) { "C:\\" } else { "/" };
        parts
            .iter()
            .fold(PathBuf::from(root), |path, part| path.join(part))
    }

    /// The three variables under one `.tmp` folder, with `overrides` on top.
    fn env(overrides: &[(&str, Option<OsString>)]) -> HashMap<String, Option<OsString>> {
        let mut vars: HashMap<String, Option<OsString>> = ISOLATED_DIR_VARS
            .iter()
            .map(|var| {
                let dir = absolute(&["repo", ".tmp", "drive", var]);
                ((*var).to_owned(), Some(dir.into_os_string()))
            })
            .collect();
        for (var, value) in overrides {
            vars.insert((*var).to_owned(), value.clone());
        }
        vars
    }

    fn check(vars: &HashMap<String, Option<OsString>>) -> Result<(), NotIsolated> {
        check_isolated(|var| vars.get(var).cloned().flatten())
    }

    fn refused(vars: &HashMap<String, Option<OsString>>) -> NotIsolated {
        check(vars).expect_err("the environment is not isolated")
    }

    #[test]
    fn isolated_env_accepts_three_dirs_under_tmp() {
        assert_eq!(check(&env(&[])), Ok(()));
    }

    #[test]
    fn isolated_env_refuses_a_dir_outside_tmp() {
        let outside = absolute(&["Users", "me", "AppData", "rustling-tulip"]);
        let vars = env(&[(
            "RUSTLING_TULIP_BINARIES_DIR",
            Some(outside.clone().into_os_string()),
        )]);
        let err = refused(&vars);
        assert_eq!(err.variable, "RUSTLING_TULIP_BINARIES_DIR");
        assert_eq!(err.reason, Reason::NotUnderTmp(outside));
        assert!(
            err.to_string().contains("is not under a .tmp folder"),
            "{err}"
        );

        let tmp_itself = absolute(&["repo", ".tmp"]);
        let vars = env(&[(
            "RUSTLING_TULIP_CONFIG_DIR",
            Some(tmp_itself.clone().into_os_string()),
        )]);
        assert_eq!(refused(&vars).reason, Reason::NotUnderTmp(tmp_itself));

        let escaped = absolute(&["repo", ".tmp", "..", "config"]);
        let vars = env(&[(
            "RUSTLING_TULIP_WORKTREES_DIR",
            Some(escaped.clone().into_os_string()),
        )]);
        assert_eq!(refused(&vars).reason, Reason::NotUnderTmp(escaped));
    }

    #[test]
    fn isolated_env_refuses_an_unset_or_empty_dir() {
        let err = refused(&env(&[("RUSTLING_TULIP_WORKTREES_DIR", None)]));
        assert_eq!(err.variable, "RUSTLING_TULIP_WORKTREES_DIR");
        assert_eq!(err.reason, Reason::Unset);

        let err = refused(&env(&[(
            "RUSTLING_TULIP_CONFIG_DIR",
            Some(OsString::new()),
        )]));
        assert_eq!(err.variable, "RUSTLING_TULIP_CONFIG_DIR");
        assert_eq!(err.reason, Reason::Empty);
    }

    #[test]
    fn isolated_env_refuses_a_relative_dir() {
        let relative = PathBuf::from(".tmp").join("drive").join("config");
        let vars = env(&[(
            "RUSTLING_TULIP_CONFIG_DIR",
            Some(relative.clone().into_os_string()),
        )]);
        let err = refused(&vars);
        assert_eq!(err.variable, "RUSTLING_TULIP_CONFIG_DIR");
        assert_eq!(err.reason, Reason::Relative(relative));
    }

    #[test]
    fn a_tmp_suffix_is_not_a_tmp_folder() {
        let lookalike = absolute(&["x", "not.tmp", "cfg"]);
        let vars = env(&[(
            "RUSTLING_TULIP_CONFIG_DIR",
            Some(lookalike.clone().into_os_string()),
        )]);
        assert_eq!(refused(&vars).reason, Reason::NotUnderTmp(lookalike));
    }
}
