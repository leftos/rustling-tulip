//! Reading a named environment variable the way the user set it: from the
//! daemon's own process environment first, then (on Windows) from the user's
//! persistent environment under `HKCU\Environment`, then from the machine's
//! under `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment`.
//!
//! The daemon is often started long before the user sets a variable (it runs
//! from the login `Run` entry and outlives terminals), so a value set with
//! `setx` or the System Properties dialog is not in its process environment
//! until the next login. Reading the persistent scopes directly picks it up
//! at once.
//!
//! The value is wrapped in [`Secret`], which never prints its text, because
//! the variables this reads are typically API keys. [`names`] lists the
//! variables that are set, by name only, for the spawn dialog to offer.

use std::collections::HashMap;
use std::fmt;

use protocol::{EnvName, EnvScope};
use tracing::warn;

/// An environment value whose text is never printed: no `Display`, no
/// `Serialize`, and a `Debug` that shows `Secret(<redacted>)`.
pub struct Secret(String);

impl Secret {
    /// Wrap a value read from somewhere other than the environment — the
    /// secret store — so it redacts its text wherever it is printed.
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

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
    /// The machine's environment (`HKLM\...\Session Manager\Environment` on
    /// Windows).
    SystemScope,
}

/// Look `name` up in the daemon's environment, then the user scope, then the
/// system scope. An empty value counts as unset in every scope.
pub fn resolve(name: &str) -> Option<(Secret, Origin)> {
    resolve_with(name, process_value, user_scope_value, system_scope_value)
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
    registry::value(registry::Scope::User, name)
}

#[cfg(windows)]
fn system_scope_value(name: &str) -> Option<String> {
    registry::value(registry::Scope::System, name)
}

/// Outside Windows there is no user scope beyond the process environment.
#[cfg(not(windows))]
fn user_scope_value(_name: &str) -> Option<String> {
    None
}

/// Outside Windows there is no system scope beyond the process environment.
#[cfg(not(windows))]
fn system_scope_value(_name: &str) -> Option<String> {
    None
}

/// [`resolve`] over caller-supplied lookups, so tests never touch the real
/// process environment or registry. The first scope with a non-empty value
/// wins: process, then user, then system.
pub fn resolve_with(
    name: &str,
    process: impl FnOnce(&str) -> Option<String>,
    user_scope: impl FnOnce(&str) -> Option<String>,
    system_scope: impl FnOnce(&str) -> Option<String>,
) -> Option<(Secret, Origin)> {
    if let Some(value) = process(name).filter(|v| !v.is_empty()) {
        return Some((Secret(value), Origin::Process));
    }
    if let Some(value) = user_scope(name).filter(|v| !v.is_empty()) {
        return Some((Secret(value), Origin::UserScope));
    }
    system_scope(name)
        .filter(|v| !v.is_empty())
        .map(|value| (Secret(value), Origin::SystemScope))
}

/// Every variable set to a non-empty value in the daemon's environment, the
/// user scope or the system scope, by name only, merged as [`merge_names`]
/// describes.
pub fn names() -> Vec<EnvName> {
    merge_names([
        (EnvScope::Process, process_names()),
        (EnvScope::User, user_scope_names()),
        (EnvScope::System, system_scope_names()),
    ])
}

/// [`names`] over caller-supplied `(name, value)` lists per scope, skipping a
/// name with an empty value. The values never leave it.
#[cfg(test)]
pub fn names_with(
    process: &[(String, String)],
    user: &[(String, String)],
    system: &[(String, String)],
) -> Vec<EnvName> {
    let set = |vars: &[(String, String)]| -> Vec<String> {
        vars.iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(name, _)| name.clone())
            .collect()
    };
    merge_names([
        (EnvScope::Process, set(process)),
        (EnvScope::User, set(user)),
        (EnvScope::System, set(system)),
    ])
}

/// The names set in each scope, highest priority first, merged: a name set
/// in several scopes is listed once (case-insensitively, as Windows compares
/// names), spelled as the highest-priority scope spells it, with every scope
/// it is set in; the list is sorted case-insensitively by name.
fn merge_names(scopes: [(EnvScope, Vec<String>); 3]) -> Vec<EnvName> {
    let mut merged: Vec<EnvName> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for (scope, names) in scopes {
        for name in names.into_iter().filter(|name| !name.is_empty()) {
            let key = name.to_lowercase();
            if let Some(entry) = by_key.get(&key).and_then(|&i| merged.get_mut(i)) {
                if !entry.scopes.contains(&scope) {
                    entry.scopes.push(scope);
                }
            } else {
                by_key.insert(key, merged.len());
                merged.push(EnvName {
                    name,
                    scopes: vec![scope],
                });
            }
        }
    }
    merged.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    merged
}

/// The names in the daemon's process environment with a non-empty Unicode
/// value (a non-Unicode one reads as unset, as [`process_value`] treats it).
fn process_names() -> Vec<String> {
    std::env::vars_os()
        .filter(|(_, value)| value.to_str().is_some_and(|v| !v.is_empty()))
        .filter_map(|(name, _)| name.into_string().ok())
        .collect()
}

#[cfg(windows)]
fn user_scope_names() -> Vec<String> {
    registry::names(registry::Scope::User)
}

#[cfg(windows)]
fn system_scope_names() -> Vec<String> {
    registry::names(registry::Scope::System)
}

#[cfg(not(windows))]
fn user_scope_names() -> Vec<String> {
    Vec::new()
}

#[cfg(not(windows))]
fn system_scope_names() -> Vec<String> {
    Vec::new()
}

#[cfg(windows)]
mod registry {
    use tracing::warn;
    use windows::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS,
    };
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, REG_EXPAND_SZ, REG_SZ,
        RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegCloseKey, RegEnumValueW, RegGetValueW,
        RegOpenKeyExW,
    };
    use windows::core::{HSTRING, PCWSTR, PWSTR, w};

    /// Attempts at reading a value whose expanded size can grow between the
    /// size query and the read.
    const ATTEMPTS: usize = 3;

    /// The longest registry value name, in UTF-16 units, plus its NUL.
    const NAME_CAPACITY: usize = 16_384;

    /// A value of at most this many bytes is read to tell whether it holds
    /// any text; a larger one does (its size counts the NUL terminator).
    const SMALL_VALUE_BYTES: u32 = 4;

    /// A registry key holding persistent environment variables.
    #[derive(Debug, Clone, Copy)]
    pub enum Scope {
        /// `HKCU\Environment`.
        User,
        /// `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment`.
        System,
    }

    impl Scope {
        fn root(self) -> HKEY {
            match self {
                Self::User => HKEY_CURRENT_USER,
                Self::System => HKEY_LOCAL_MACHINE,
            }
        }

        fn path(self) -> PCWSTR {
            match self {
                Self::User => w!("Environment"),
                Self::System => {
                    w!("SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment")
                }
            }
        }

        fn label(self) -> &'static str {
            match self {
                Self::User => "user",
                Self::System => "system",
            }
        }
    }

    /// `name` under `scope`'s key, with `REG_EXPAND_SZ` values expanded.
    /// `None` when the value is absent or can't be read (logged).
    pub fn value(scope: Scope, name: &str) -> Option<String> {
        let value_name = HSTRING::from(name);
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
        let mut size: u32 = 0;
        // SAFETY: the key and value names are valid null-terminated wide
        // strings that outlive the call; with no data buffer, the call only
        // writes the required size into `size`.
        let status = unsafe {
            RegGetValueW(
                scope.root(),
                scope.path(),
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
                scope = scope.label(),
                "user_env: sizing a registry variable failed"
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
                    scope.root(),
                    scope.path(),
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
                    scope = scope.label(),
                    "user_env: reading a registry variable failed"
                );
                return None;
            }
            let chars = (written as usize / 2).min(buf.len());
            let text = &buf[..chars];
            let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
            return Some(String::from_utf16_lossy(&text[..end]));
        }
        warn!(
            scope = scope.label(),
            "user_env: a registry variable kept growing while being read"
        );
        None
    }

    /// The names of the string values under `scope`'s key that hold text.
    /// Empty when the key is absent or can't be opened (logged).
    pub fn names(scope: Scope) -> Vec<String> {
        let mut key = HKEY(std::ptr::null_mut());
        // SAFETY: the subkey path is a valid null-terminated wide string that
        // outlives the call; the call writes the opened handle into `key`.
        let status = unsafe {
            RegOpenKeyExW(
                scope.root(),
                scope.path(),
                None,
                KEY_QUERY_VALUE,
                &raw mut key,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Vec::new();
        }
        if status != ERROR_SUCCESS {
            warn!(
                code = status.0,
                scope = scope.label(),
                "user_env: opening a registry environment failed"
            );
            return Vec::new();
        }
        let names = enumerate(key, scope);
        // SAFETY: `key` was opened above and is closed exactly once, here.
        let status = unsafe { RegCloseKey(key) };
        if status != ERROR_SUCCESS {
            warn!(
                code = status.0,
                scope = scope.label(),
                "user_env: closing a registry environment failed"
            );
        }
        names
    }

    /// Every string value's name under the open `key` whose value holds
    /// text, reading each value's size and, for a tiny one, its text.
    fn enumerate(key: HKEY, scope: Scope) -> Vec<String> {
        let mut names = Vec::new();
        let mut buf = vec![0u16; NAME_CAPACITY];
        for index in 0u32.. {
            let mut name_len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
            let mut kind: u32 = 0;
            let mut size: u32 = 0;
            // SAFETY: `key` is an open handle; `buf` holds `name_len` UTF-16
            // units and outlives the call, which writes at most that many
            // (with the NUL) and the count without the NUL into `name_len`;
            // with no data buffer it only writes the value's type into
            // `kind` and its byte size into `size`.
            let status = unsafe {
                RegEnumValueW(
                    key,
                    index,
                    Some(PWSTR(buf.as_mut_ptr())),
                    &raw mut name_len,
                    None,
                    Some(&raw mut kind),
                    None,
                    Some(&raw mut size),
                )
            };
            if status == ERROR_NO_MORE_ITEMS {
                break;
            }
            if status != ERROR_SUCCESS {
                warn!(
                    code = status.0,
                    scope = scope.label(),
                    "user_env: listing a registry environment failed"
                );
                break;
            }
            let units = buf.get(..name_len as usize).unwrap_or_default();
            let name = String::from_utf16_lossy(units);
            let is_string = kind == REG_SZ.0 || kind == REG_EXPAND_SZ.0;
            if is_string && holds_text(scope, &name, size) {
                names.push(name);
            }
        }
        names
    }

    /// Whether string value `name` of `size` bytes holds text: a value with
    /// room for more than a character and its NUL does; a smaller one is read.
    fn holds_text(scope: Scope, name: &str, size: u32) -> bool {
        match size {
            0 => false,
            1..=SMALL_VALUE_BYTES => value(scope, name).is_some_and(|v| !v.is_empty()),
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn a_name_no_one_sets_reads_none_from_the_real_user_scope() {
        assert_eq!(
            registry::value(registry::Scope::User, "RT_TEST_NO_ONE_SETS_THIS_9F3E1A"),
            None
        );
    }

    /// `TEMP` is a `REG_EXPAND_SZ` (`%USERPROFILE%\AppData\Local\Temp`) that
    /// Windows creates in every user profile's `HKCU\Environment`.
    #[cfg(windows)]
    #[test]
    fn user_scope_temp_reads_back_expanded_without_a_trailing_nul() {
        let temp = registry::value(registry::Scope::User, "TEMP").unwrap_or_default();
        assert!(!temp.is_empty(), "TEMP is in HKCU\\Environment");
        assert!(!temp.contains('%'), "expanded: {temp}");
        assert!(!temp.contains('\0'), "no NUL: {temp:?}");
    }

    /// `windir` is a `REG_EXPAND_SZ` (`%SystemRoot%`) in every machine's
    /// system environment.
    #[cfg(windows)]
    #[test]
    #[expect(
        clippy::expect_used,
        reason = "a missing windir fails the test with the precondition it lost"
    )]
    fn windir_resolves_and_is_listed_with_the_system_scope() {
        let system = registry::value(registry::Scope::System, "windir").unwrap_or_default();
        assert!(!system.is_empty(), "windir is in the system environment");
        assert!(!system.contains('%'), "expanded: {system}");
        let (value, origin) = resolve("windir").expect("windir resolves");
        assert!(!value.expose().is_empty());
        assert!(
            matches!(origin, Origin::Process | Origin::SystemScope),
            "{origin:?}"
        );
        let listed = names();
        let windir = listed
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case("windir"))
            .expect("windir is listed");
        assert!(windir.scopes.contains(&EnvScope::System), "{windir:?}");
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

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn process_value_wins_over_user_scope() {
        let r = resolve_with("K", some("proc"), some("user"), some("system"));
        assert_eq!(resolved(r), Some(("proc".to_owned(), Origin::Process)));
    }

    #[test]
    fn user_scope_is_used_when_the_process_value_is_unset() {
        let r = resolve_with("K", none, some("user"), none);
        assert_eq!(resolved(r), Some(("user".to_owned(), Origin::UserScope)));
    }

    #[test]
    fn an_empty_process_value_falls_through_to_user_scope() {
        let r = resolve_with("K", some(""), some("user"), none);
        assert_eq!(resolved(r), Some(("user".to_owned(), Origin::UserScope)));
    }

    #[test]
    fn a_name_only_in_the_system_scope_resolves_from_it() {
        let r = resolve_with("K", none, none, some("system"));
        assert_eq!(
            resolved(r),
            Some(("system".to_owned(), Origin::SystemScope))
        );
    }

    #[test]
    fn the_user_scope_wins_over_the_system_scope() {
        let r = resolve_with("K", none, some("user"), some("system"));
        assert_eq!(resolved(r), Some(("user".to_owned(), Origin::UserScope)));
        let r = resolve_with("K", some(""), some(""), some("system"));
        assert_eq!(
            resolved(r),
            Some(("system".to_owned(), Origin::SystemScope)),
            "empty values fall through to the system scope"
        );
    }

    #[test]
    fn an_empty_value_in_every_scope_is_unset() {
        assert!(resolve_with("K", none, none, none).is_none());
        assert!(resolve_with("K", some(""), some(""), some("")).is_none());
        assert!(resolve_with("K", none, some(""), none).is_none());
        assert!(resolve_with("K", none, none, some("")).is_none());
    }

    #[test]
    fn the_lookups_receive_the_name() {
        let r = resolve_with(
            "WANTED",
            |n| (n == "WANTED").then(String::new),
            |n| (n == "WANTED").then(String::new),
            |n| (n == "WANTED").then(|| "v".to_owned()),
        );
        assert_eq!(resolved(r), Some(("v".to_owned(), Origin::SystemScope)));
    }

    #[test]
    fn names_merge_scopes_case_insensitively_and_sort() {
        let listed = names_with(
            &vars(&[("PATH", "p"), ("zeta", "z")]),
            &vars(&[("Path", "u"), ("DEEPSEEK_API_KEY", "sk-user")]),
            &vars(&[("path", "s"), ("Alpha", "a"), ("ZETA", "z")]),
        );
        assert_eq!(
            listed,
            [
                EnvName {
                    name: "Alpha".to_owned(),
                    scopes: vec![EnvScope::System],
                },
                EnvName {
                    name: "DEEPSEEK_API_KEY".to_owned(),
                    scopes: vec![EnvScope::User],
                },
                EnvName {
                    name: "PATH".to_owned(),
                    scopes: vec![EnvScope::Process, EnvScope::User, EnvScope::System],
                },
                EnvName {
                    name: "zeta".to_owned(),
                    scopes: vec![EnvScope::Process, EnvScope::System],
                },
            ]
        );
    }

    #[test]
    fn names_skip_empty_values() {
        let listed = names_with(
            &vars(&[("EMPTY_EVERYWHERE", ""), ("SET_LATER", "")]),
            &vars(&[("EMPTY_EVERYWHERE", ""), ("", "no name")]),
            &vars(&[("EMPTY_EVERYWHERE", ""), ("SET_LATER", "v")]),
        );
        assert_eq!(
            listed,
            [EnvName {
                name: "SET_LATER".to_owned(),
                scopes: vec![EnvScope::System],
            }]
        );
    }

    #[test]
    fn debug_never_prints_the_value() {
        let secret = Secret("sk-ant-hunter2".to_owned());
        let shown = format!("{secret:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert_eq!(shown, "Secret(<redacted>)");
    }
}
