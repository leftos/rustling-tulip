//! Claude Code's folder trust for the extra folders a spawn opens with
//! `--add-dir`: Claude Code asks about an untrusted one on every start and
//! records nothing when the user answers, so the plugin records it, under
//! Claude Code's own config lock, before the spawn.

use crate::transcripts::CONFIG_DIR_VAR;
use agent_api::{AgentError, ResolvedRows};
use claude_accounts::{
    ClaudeCodeLocks, ClaudePaths, file_has_content, resolve_claude_paths, update_global_config,
};
use directories::UserDirs;
use serde_json::{Map, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The field of a `projects` entry that records the trust dialog's Yes.
const TRUST_FIELD: &str = "hasTrustDialogAccepted";

/// Records Claude Code's folder trust for each of `folders` in the global
/// config of the home `rows` select ([`trust_paths`]). Writes nothing, and takes
/// no lock, when `folders` is empty or every one is already trusted.
///
/// # Errors
///
/// No home found, a relative `CLAUDE_CONFIG_DIR`, the config lock not taken, the
/// config unreadable or not written, or a `projects` value or entry that is not an
/// object (nothing written).
pub(crate) fn record_trust(rows: &ResolvedRows, folders: &[PathBuf]) -> Result<(), AgentError> {
    if folders.is_empty() {
        return Ok(());
    }
    let paths = trust_paths(rows)?;
    let keys: Vec<String> = folders.iter().map(|folder| trust_key(folder)).collect();
    // An advisory read without the lock: it never writes, and a config that
    // already trusts every folder needs no lock. Any other outcome goes on to
    // the locked update, which reads the file again.
    if std::fs::read(paths.global_config()).is_ok_and(|bytes| all_trusted(&bytes, &keys)) {
        return Ok(());
    }
    let config = paths.global_config().display().to_string();
    let guard = ClaudeCodeLocks::config(&paths).map_err(|err| {
        AgentError::new(format!(
            "Claude Code's config lock for {config} could not be taken: {err}"
        ))
    })?;
    let mut malformed = false;
    update_global_config(&paths, &guard, |doc| {
        trust_all(doc, &keys).unwrap_or_else(|| {
            malformed = true;
            false
        })
    })
    .map_err(|err| {
        AgentError::new(format!(
            "Claude Code's folder trust could not be recorded in {config}: {err}"
        ))
    })?;
    if malformed {
        return Err(AgentError::new(
            "Claude Code's config has a projects value or entry that is not an object",
        ));
    }
    Ok(())
}

/// Whether the config `bytes` already trust every one of `keys`: a JSON object
/// whose `projects` entry for each key is an object with the trust field `true`.
fn all_trusted(bytes: &[u8], keys: &[String]) -> bool {
    let Ok(doc) = serde_json::from_slice::<Map<String, Value>>(bytes) else {
        return false;
    };
    let Some(projects) = doc.get("projects").and_then(Value::as_object) else {
        return false;
    };
    keys.iter().all(|key| {
        projects
            .get(key)
            .and_then(Value::as_object)
            .and_then(|entry| entry.get(TRUST_FIELD))
            == Some(&Value::Bool(true))
    })
}

/// Where a spawn's Claude Code keeps its home, by the child's own rule.
#[derive(Debug, PartialEq, Eq)]
enum ConfigDir {
    /// `CLAUDE_CONFIG_DIR` unset or empty: `<user home>/.claude`, with the config at
    /// `<user home>/.claude.json` by default ([`resolve_claude_paths`]).
    Default,
    /// An absolute `CLAUDE_CONFIG_DIR`, holding the config (`.claude.json` by default).
    Dir(PathBuf),
    /// A relative `CLAUDE_CONFIG_DIR`, which the daemon does not resolve.
    Relative,
}

/// The `CLAUDE_CONFIG_DIR` the child sees: the spawn's `row` when there is one,
/// since a row replaces the daemon's variable in the child's environment even
/// when it is empty, else the daemon's own `process` value. Empty means unset.
fn config_dir(row: Option<&str>, process: Option<OsString>) -> ConfigDir {
    let value = match (row, process) {
        (Some(row), _) => OsString::from(row),
        (None, Some(process)) => process,
        (None, None) => return ConfigDir::Default,
    };
    if value.is_empty() {
        return ConfigDir::Default;
    }
    let dir = PathBuf::from(value);
    if dir.is_absolute() {
        ConfigDir::Dir(dir)
    } else {
        ConfigDir::Relative
    }
}

/// Claude Code's files for a spawn with resolved rows `rows`, as
/// [`resolve_claude_paths`] finds them in the child's environment: its
/// `CLAUDE_CONFIG_DIR` by [`config_dir`], and every other variable the
/// spawn's row when there is one, else the daemon's own.
fn trust_paths(rows: &ResolvedRows) -> Result<ClaudePaths, AgentError> {
    let dir = match config_dir(rows.get(CONFIG_DIR_VAR), std::env::var_os(CONFIG_DIR_VAR)) {
        ConfigDir::Dir(dir) => Some(dir.into_os_string()),
        ConfigDir::Relative => {
            return Err(AgentError::new(
                "CLAUDE_CONFIG_DIR is relative; Claude Code's folder trust was not recorded",
            ));
        }
        ConfigDir::Default => None,
    };
    let env = |name: &str| {
        if name == CONFIG_DIR_VAR {
            return dir.clone();
        }
        rows.get(name)
            .map(OsString::from)
            .or_else(|| std::env::var_os(name))
    };
    let os_home = || UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    resolve_claude_paths(env, os_home, file_has_content)
        .ok_or_else(|| AgentError::new("Claude Code's home folder could not be found"))
}

/// `folder` as Claude Code spells its `projects` key: on Windows every `\` as
/// `/`, and on every platform no trailing `/` except on a root (`/`, `X:/`).
fn trust_key(folder: &Path) -> String {
    let mut key = folder.to_string_lossy().into_owned();
    if cfg!(windows) {
        key = key.replace('\\', "/");
    }
    while key.ends_with('/') && !is_root(&key) {
        key.pop();
    }
    key
}

/// Whether `key` is a root that keeps its slash: `/` or a bare drive root.
fn is_root(key: &str) -> bool {
    key == "/" || (key.len() == 3 && key.as_bytes().get(1) == Some(&b':'))
}

/// Marks each of `keys` trusted in `doc`'s `projects`, made `{}` when absent,
/// keeping an entry's other fields. `Some(changed)`, or `None` when
/// `projects` or one of the entries is not an object.
fn trust_all(doc: &mut Map<String, Value>, keys: &[String]) -> Option<bool> {
    let projects = doc
        .entry("projects")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()?;
    if keys
        .iter()
        .any(|key| projects.get(key).is_some_and(|entry| !entry.is_object()))
    {
        return None;
    }
    let mut changed = false;
    for key in keys {
        let entry = projects
            .entry(key.clone())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()?;
        if entry.get(TRUST_FIELD) != Some(&Value::Bool(true)) {
            entry.insert(TRUST_FIELD.to_owned(), Value::Bool(true));
            changed = true;
        }
    }
    Some(changed)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly on a broken scratch dir"
)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    /// A scratch Claude Code home under the temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("rt-trust-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).expect("create the scratch dir");
            Self(dir)
        }

        fn rows(&self) -> ResolvedRows {
            ResolvedRows::new(vec![(
                "CLAUDE_CONFIG_DIR".into(),
                self.0.to_string_lossy().into_owned(),
            )])
        }

        fn config(&self) -> PathBuf {
            self.0.join(".claude.json")
        }

        fn seed(&self, text: &str) {
            fs::write(self.config(), text).expect("seed the config");
        }

        fn bytes(&self) -> Vec<u8> {
            fs::read(self.config()).expect("read the config")
        }

        fn json(&self) -> Value {
            serde_json::from_slice(&self.bytes()).expect("the config is JSON")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The folder whose key is `X:/dev/yaat-server`: its Windows path on
    /// Windows, the key itself elsewhere.
    fn yaat_server() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"X:\dev\yaat-server")
        } else {
            PathBuf::from("X:/dev/yaat-server")
        }
    }

    /// An absolute folder on this platform.
    fn absolute(name: &str) -> PathBuf {
        let root = if cfg!(windows) { r"X:\" } else { "/" };
        Path::new(root).join(name)
    }

    #[test]
    fn config_dir_follows_the_childs_rule() {
        let row = absolute("row");
        let process = absolute("process");
        let row_str = row.to_string_lossy().into_owned();
        let cases: [(Option<&str>, Option<OsString>, ConfigDir); 6] = [
            (
                Some(&row_str),
                Some(process.clone().into()),
                ConfigDir::Dir(row.clone()),
            ),
            (Some(""), Some(process.clone().into()), ConfigDir::Default),
            (
                None,
                Some(process.clone().into()),
                ConfigDir::Dir(process.clone()),
            ),
            (None, None, ConfigDir::Default),
            (Some("relative"), None, ConfigDir::Relative),
            (None, Some(OsString::from("relative")), ConfigDir::Relative),
        ];
        for (row, process, expected) in cases {
            assert_eq!(
                config_dir(row, process.clone()),
                expected,
                "{row:?} {process:?}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn trust_key_cases() {
        for (folder, key) in [
            (r"X:\dev\a\", "X:/dev/a"),
            (r"X:\", "X:/"),
            (r"X:\dev\a", "X:/dev/a"),
        ] {
            assert_eq!(trust_key(Path::new(folder)), key, "{folder}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn trust_key_cases_off_windows() {
        for (folder, key) in [("/Users/me/a/", "/Users/me/a"), ("/", "/")] {
            assert_eq!(trust_key(Path::new(folder)), key, "{folder}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn records_trust_for_a_new_folder_under_its_forward_slash_key() {
        let scratch = Scratch::new();
        record_trust(&scratch.rows(), &[PathBuf::from(r"X:\dev\yaat-server")])
            .expect("trust is recorded");
        assert_eq!(
            scratch.json()["projects"]["X:/dev/yaat-server"],
            json!({ "hasTrustDialogAccepted": true })
        );
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn keeps_an_existing_entrys_fields_and_other_keys() {
        let scratch = Scratch::new();
        scratch.seed(
            &json!({
                "numStartups": 42,
                "projects": {
                    "X:/dev/yaat-server": { "allowedTools": ["x"], "hasTrustDialogAccepted": false }
                }
            })
            .to_string(),
        );
        record_trust(&scratch.rows(), &[yaat_server()]).expect("trust is recorded");
        let config = scratch.json();
        assert_eq!(config["numStartups"], json!(42));
        assert_eq!(
            config["projects"]["X:/dev/yaat-server"],
            json!({ "allowedTools": ["x"], "hasTrustDialogAccepted": true })
        );
    }

    #[test]
    fn an_already_trusted_config_is_not_rewritten() {
        let scratch = Scratch::new();
        let compact = r#"{"projects":{"X:/dev/yaat-server":{"hasTrustDialogAccepted":true}},"numStartups":1}"#;
        scratch.seed(compact);
        record_trust(&scratch.rows(), &[yaat_server()]).expect("an already trusted folder is fine");
        assert_eq!(scratch.bytes(), compact.as_bytes());
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn trusts_the_untrusted_folder_beside_a_trusted_one() {
        let scratch = Scratch::new();
        scratch.seed(r#"{"projects":{"X:/dev/yaat":{"hasTrustDialogAccepted":true}}}"#);
        let yaat = if cfg!(windows) {
            PathBuf::from(r"X:\dev\yaat")
        } else {
            PathBuf::from("X:/dev/yaat")
        };
        record_trust(&scratch.rows(), &[yaat, yaat_server()]).expect("trust is recorded");
        let config = scratch.json();
        assert_eq!(config["projects"]["X:/dev/yaat"][TRUST_FIELD], json!(true));
        assert_eq!(
            config["projects"]["X:/dev/yaat-server"][TRUST_FIELD],
            json!(true)
        );
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn refuses_a_malformed_config_and_leaves_it_untouched() {
        let scratch = Scratch::new();
        scratch.seed("not json");
        let result = record_trust(&scratch.rows(), &[yaat_server()]);
        assert!(result.is_err(), "{result:?}");
        assert_eq!(scratch.bytes(), b"not json");
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn refuses_a_projects_value_that_is_not_an_object() {
        let scratch = Scratch::new();
        scratch.seed(r#"{"projects": 3}"#);
        let result = record_trust(&scratch.rows(), &[yaat_server()]);
        assert!(result.is_err(), "{result:?}");
        assert_eq!(scratch.bytes(), br#"{"projects": 3}"#);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn refuses_an_entry_that_is_not_an_object() {
        let scratch = Scratch::new();
        scratch.seed(r#"{"projects":{"X:/dev/yaat-server":3}}"#);
        let result = record_trust(&scratch.rows(), &[yaat_server()]);
        assert!(result.is_err(), "{result:?}");
        assert_eq!(scratch.bytes(), br#"{"projects":{"X:/dev/yaat-server":3}}"#);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn a_non_empty_config_json_in_the_config_home_receives_the_trust() {
        let scratch = Scratch::new();
        let legacy = scratch.0.join(".config.json");
        fs::write(&legacy, r#"{"numStartups":1}"#).expect("seed the .config.json");
        record_trust(&scratch.rows(), &[yaat_server()]).expect("trust is recorded");
        let config: Value =
            serde_json::from_slice(&fs::read(&legacy).expect("read the .config.json"))
                .expect("the .config.json is JSON");
        assert_eq!(
            config["projects"]["X:/dev/yaat-server"][TRUST_FIELD],
            json!(true)
        );
        assert_eq!(config["numStartups"], json!(1));
        assert!(!scratch.config().exists(), ".claude.json was written");
    }

    #[test]
    fn no_folders_writes_nothing() {
        let scratch = Scratch::new();
        assert_eq!(record_trust(&scratch.rows(), &[]), Ok(()));
        assert!(!scratch.config().exists());
    }
}
