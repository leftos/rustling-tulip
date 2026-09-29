//! Reading a named environment variable the way the user set it: from the
//! daemon's own process environment first, then (on Windows) from the user's
//! persistent environment under `HKCU\Environment`.
//!
//! The daemon is often started long before the user sets a variable (it runs
//! from the login `Run` entry and outlives terminals), so a value set with
//! `setx` or the System Properties dialog is not in its process environment
//! until the next login. Reading the user scope directly picks it up at once.
//!
//! The value is wrapped in [`Secret`], which never prints its text, because
//! the variables this reads are typically API keys.

use std::fmt;
use tracing::warn;

/// An environment value whose text is never printed: no `Display`, no
/// `Serialize`, and a `Debug` that shows `Secret(<redacted>)`.
pub struct Secret(String);

impl Secret {
    /// The value's text, for the one place that needs it: the environment a
    /// child process is spawned with.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Where a resolved value came from, so a log line can say so without the
/// value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The daemon's own process environment.
    Process,
    /// The user's persistent environment (`HKCU\Environment` on Windows).
    UserScope,
}

/// Look `name` up in the daemon's environment, then the user scope. An empty
/// value counts as unset in both.
pub fn resolve(name: &str) -> Option<(Secret, Origin)> {
    resolve_with(name, process_value, user_scope_value)
}

/// `name` from the daemon's process environment. A value that isn't valid
/// Unicode is logged (by name only) and treated as unset.
fn process_value(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            warn!(
                name,
                "user_env: process variable isn't valid Unicode; treating it as unset"
            );
            None
        }
    }
}

#[cfg(windows)]
fn user_scope_value(name: &str) -> Option<String> {
    registry::user_scope_value(name)
}

/// Outside Windows there is no user scope beyond the process environment.
#[cfg(not(windows))]
fn user_scope_value(_name: &str) -> Option<String> {
    None
}

/// [`resolve`] over caller-supplied lookups, so tests never touch the real
/// process environment or registry.
pub fn resolve_with(
    name: &str,
    process: impl FnOnce(&str) -> Option<String>,
    user_scope: impl FnOnce(&str) -> Option<String>,
) -> Option<(Secret, Origin)> {
    if let Some(value) = process(name).filter(|v| !v.is_empty()) {
        return Some((Secret(value), Origin::Process));
    }
    user_scope(name)
        .filter(|v| !v.is_empty())
        .map(|value| (Secret(value), Origin::UserScope))
}

#[cfg(windows)]
mod registry {
    use tracing::warn;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW,
    };
    use windows::core::{HSTRING, w};

    /// Attempts at reading a value whose expanded size can grow between the
    /// size query and the read.
    const ATTEMPTS: usize = 3;

    /// `name` under `HKCU\Environment`, with `REG_EXPAND_SZ` values expanded.
    /// `None` when the value is absent or can't be read (logged).
    pub fn user_scope_value(name: &str) -> Option<String> {
        let value_name = HSTRING::from(name);
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
        let mut size: u32 = 0;
        // SAFETY: the key and value names are valid null-terminated wide
        // strings that outlive the call; with no data buffer, the call only
        // writes the required size into `size`.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                w!("Environment"),
                &value_name,
                flags,
                None,
                None,
                Some(&raw mut size),
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return None;
        }
        if status != ERROR_SUCCESS {
            warn!(
                code = status.0,
                "user_env: sizing a user-scope variable failed"
            );
            return None;
        }
        for _ in 0..ATTEMPTS {
            let mut buf = vec![0u16; (size as usize).div_ceil(2)];
            let mut written = u32::try_from(buf.len() * 2).unwrap_or(u32::MAX);
            // SAFETY: `buf` holds `written` bytes and outlives the call; the
            // call writes at most `written` bytes into it and the byte count
            // it wrote into `written`.
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    w!("Environment"),
                    &value_name,
                    flags,
                    None,
                    Some(buf.as_mut_ptr().cast()),
                    Some(&raw mut written),
                )
            };
            if status == ERROR_MORE_DATA {
                size = written;
                continue;
            }
            if status == ERROR_FILE_NOT_FOUND {
                return None;
            }
            if status != ERROR_SUCCESS {
                warn!(
                    code = status.0,
                    "user_env: reading a user-scope variable failed"
                );
                return None;
            }
            let chars = (written as usize / 2).min(buf.len());
            let text = &buf[..chars];
            let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
            return Some(String::from_utf16_lossy(&text[..end]));
        }
        warn!("user_env: a user-scope variable kept growing while being read");
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn a_name_no_one_sets_reads_none_from_the_real_user_scope() {
        assert_eq!(
            registry::user_scope_value("RT_TEST_NO_ONE_SETS_THIS_9F3E1A"),
            None
        );
    }

    /// `TEMP` is a `REG_EXPAND_SZ` (`%USERPROFILE%\AppData\Local\Temp`) that
    /// Windows creates in every user profile's `HKCU\Environment`.
    #[cfg(windows)]
    #[test]
    fn user_scope_temp_reads_back_expanded_without_a_trailing_nul() {
        let temp = registry::user_scope_value("TEMP").unwrap_or_default();
        assert!(!temp.is_empty(), "TEMP is in HKCU\\Environment");
        assert!(!temp.contains('%'), "expanded: {temp}");
        assert!(!temp.contains('\0'), "no NUL: {temp:?}");
    }

    fn some(v: &str) -> impl FnOnce(&str) -> Option<String> {
        let v = v.to_owned();
        move |_| Some(v)
    }

    fn none(_: &str) -> Option<String> {
        None
    }

    fn resolved(r: Option<(Secret, Origin)>) -> Option<(String, Origin)> {
        r.map(|(s, o)| (s.expose().to_owned(), o))
    }

    #[test]
    fn process_value_wins_over_user_scope() {
        let r = resolve_with("K", some("proc"), some("user"));
        assert_eq!(resolved(r), Some(("proc".to_owned(), Origin::Process)));
    }

    #[test]
    fn user_scope_is_used_when_the_process_value_is_unset() {
        let r = resolve_with("K", none, some("user"));
        assert_eq!(resolved(r), Some(("user".to_owned(), Origin::UserScope)));
    }

    #[test]
    fn an_empty_process_value_falls_through_to_user_scope() {
        let r = resolve_with("K", some(""), some("user"));
        assert_eq!(resolved(r), Some(("user".to_owned(), Origin::UserScope)));
    }

    #[test]
    fn empty_or_unset_in_both_scopes_is_none() {
        assert!(resolve_with("K", none, none).is_none());
        assert!(resolve_with("K", some(""), some("")).is_none());
        assert!(resolve_with("K", none, some("")).is_none());
    }

    #[test]
    fn the_lookups_receive_the_name() {
        let r = resolve_with(
            "WANTED",
            |n| (n == "WANTED").then(String::new),
            |n| (n == "WANTED").then(|| "v".to_owned()),
        );
        assert_eq!(resolved(r), Some(("v".to_owned(), Origin::UserScope)));
    }

    #[test]
    fn debug_never_prints_the_value() {
        let secret = Secret("sk-ant-hunter2".to_owned());
        let shown = format!("{secret:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert_eq!(shown, "Secret(<redacted>)");
    }
}
