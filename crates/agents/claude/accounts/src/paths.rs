//! Where the account store and Claude Code keep their files. Nothing here reads the environment: the caller says where home is.

use std::ffi::OsString;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};

use crate::store::{StoreError, StoreErrorKind, is_plain_dir};

/// The bytes an account email may not hold: path separators and the characters Windows reserves in file names. Bytes
/// below 0x20 are refused as well.
const UNSAFE_EMAIL_BYTES: &[u8] = b"/\\:*?\"<>|";

/// An account's email, checked to be safe as part of a file name: non-empty, with no path separator, no character
/// Windows reserves in file names and no control character. The only way to make one is [`Email::parse`], so a path
/// ever built from an email stays in its folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Email(String);

impl Email {
    /// Checks `text` and keeps it.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::UnsafeEmail`] when `text` is empty or holds `/ \ : * ? " < > |` or a byte below 0x20.
    pub fn parse(text: &str) -> Result<Self, StoreError> {
        let unsafe_byte = text
            .bytes()
            .any(|byte| byte < 0x20 || UNSAFE_EMAIL_BYTES.contains(&byte));
        if text.is_empty() || unsafe_byte {
            return Err(StoreError::new(StoreErrorKind::UnsafeEmail, None));
        }
        Ok(Self(text.to_owned()))
    }

    /// The email's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The account store's root, which the caller supplies, and the files in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountsRoot {
    root: PathBuf,
}

impl AccountsRoot {
    /// An account store at `root`, the folder the caller chose for it.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The root folder itself.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `accounts.json`: the accounts, their order, the active one and the next account number.
    #[must_use]
    pub fn accounts_json(&self) -> PathBuf {
        self.root.join("accounts.json")
    }

    /// `configs/`: each account's `oauthAccount` block.
    #[must_use]
    pub fn configs_dir(&self) -> PathBuf {
        self.root.join("configs")
    }

    /// `credentials/`: each account's sealed credential file.
    #[must_use]
    pub fn credentials_dir(&self) -> PathBuf {
        self.root.join("credentials")
    }

    /// `.lock`: the account lock, held around every store write and switch.
    #[must_use]
    pub fn lock_file(&self) -> PathBuf {
        self.root.join(".lock")
    }

    /// `settings.json`: the auto-switch engine's and the fleet verdict's settings ([`crate::Settings`]).
    #[must_use]
    pub fn settings(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    /// `.settings.lock`: the lock held around every write of `settings.json`.
    #[must_use]
    pub fn settings_lock(&self) -> PathBuf {
        self.root.join(".settings.lock")
    }

    /// `autoswitch_state.json`: the auto-switch engines' shared state: the last switch and the
    /// quarantined accounts.
    #[must_use]
    pub fn autoswitch_state(&self) -> PathBuf {
        self.root.join("autoswitch_state.json")
    }

    /// `.autoswitch_state.lock`: the lock held around every read and read-modify-write of `autoswitch_state.json`.
    #[must_use]
    pub fn autoswitch_state_lock(&self) -> PathBuf {
        self.root.join(".autoswitch_state.lock")
    }

    /// `engine_lease.json`: which auto-switch engine is live on this account store, and its last heartbeat.
    #[must_use]
    pub fn engine_lease(&self) -> PathBuf {
        self.root.join("engine_lease.json")
    }

    /// `.engine_lease.lock`: the lock held around every look at and write of `engine_lease.json`.
    #[must_use]
    pub fn engine_lease_lock(&self) -> PathBuf {
        self.root.join(".engine_lease.lock")
    }

    /// `cache/`: the store's caches, the usage table among them.
    #[must_use]
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// `cache/usage.json`: the per-account usage table (schema [`crate::USAGE_SCHEMA_VERSION`]).
    #[must_use]
    pub fn usage_json(&self) -> PathBuf {
        self.cache_dir().join("usage.json")
    }

    /// `cache/.usage.lock`: the lock held around every read-modify-write of `cache/usage.json`.
    #[must_use]
    pub fn usage_lock(&self) -> PathBuf {
        self.cache_dir().join(".usage.lock")
    }

    /// Account `number`'s credential file, `credentials/<number>.cred`, sealed to this user.
    #[must_use]
    pub fn credential_path(&self, number: u32) -> PathBuf {
        self.credentials_dir().join(format!("{number}.cred"))
    }

    /// The credential file one write replaced, `credentials/<number>.cred.prev`: a raw copy of its ciphertext.
    #[must_use]
    pub fn credential_previous_path(&self, number: u32) -> PathBuf {
        self.credentials_dir().join(format!("{number}.cred.prev"))
    }

    /// Account `number`'s config file, `configs/<number>.json`.
    #[must_use]
    pub fn config_path(&self, number: u32) -> PathBuf {
        self.configs_dir().join(format!("{number}.json"))
    }

    /// The lock that makes account `number`'s token refreshes one at a time, `credentials/.consume-<number>.lock`.
    #[must_use]
    pub fn consume_lock(&self, number: u32) -> PathBuf {
        self.credentials_dir()
            .join(format!(".consume-{number}.lock"))
    }

    /// Checks every folder between the root and `file`, both left out, so a folder someone swapped for a symbolic link
    /// or junction is refused before `file` is opened, created, renamed, locked or listed through it. A folder counts as
    /// plain as the store's `is_plain_dir` sees it. The walk stops at the first folder that does not exist yet: the
    /// operation's own `create_dir_all` or open makes it or fails on it. For a file directly in the root there is no folder
    /// to check, so the call is a no-op there. The root itself is not checked here: [`crate::Store::open`] and every
    /// lock in the root (each taken through `locks::acquire_in_root`) refuse a root that is a link or open to another
    /// user, and a file read through the guard is checked on its own handle.
    ///
    /// The check is not atomic with the open that follows: a folder swapped for a link between the two is still
    /// followed. Closing that needs opens relative to a folder handle, which this crate does not make.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`] when a folder on the way is a symbolic link, a junction or not a folder, or when
    /// `file` is not under the root or its path under the root holds anything but plain names (a programming error);
    /// the error of looking at a folder that exists but cannot be looked at.
    pub fn check_folders(&self, file: &Path) -> io::Result<()> {
        let relative = file.strip_prefix(&self.root).map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "the path is not under the account store",
            )
        })?;
        let Some(folders) = relative.parent() else {
            return Ok(());
        };
        let mut folder = self.root.clone();
        for component in folders.components() {
            let Component::Normal(name) = component else {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "the path under the account store is not plain names",
                ));
            };
            folder.push(name);
            if is_plain_dir(&folder) {
                continue;
            }
            return match fs::symlink_metadata(&folder) {
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
                Ok(_) => Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "a folder in the account store is a symbolic link, a junction or not a folder, which is not followed",
                )),
            };
        }
        Ok(())
    }
}

/// Where Claude Code keeps its live login.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveStore {
    /// The live credential file, `<config home>/.credentials.json`, alone: Windows and Linux.
    File,
    /// The macOS Keychain's generic-password item of `service` and `account`, read and written through Apple's
    /// `security` tool at `security`. A read takes the item, and the live credential file only when there is no item;
    /// a write goes to the item and rewrites the live credential file with the same bytes when one exists, never
    /// creating it, so a running Claude Code that watches the file notices the change sooner.
    Keychain {
        /// The item's service: `Claude Code-credentials`, suffixed for a config dir that is not the default.
        service: String,
        /// The item's account: the user's name, as Claude Code picks it.
        account: String,
        /// The `security` tool to run: `/usr/bin/security`, never one found on `PATH`.
        security: PathBuf,
    },
}

/// Claude Code's own files: its config home (`~/.claude`) and its global config file (`~/.claude.json` by default,
/// found by [`resolve_claude_paths`]), and where it keeps its live login ([`LiveStore`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudePaths {
    config_home: PathBuf,
    global_config: PathBuf,
    live: LiveStore,
}

impl ClaudePaths {
    /// Claude Code's config home and global config file, its live login kept in the live credential file
    /// ([`LiveStore::File`]).
    #[must_use]
    pub fn new(config_home: PathBuf, global_config: PathBuf) -> Self {
        Self {
            config_home,
            global_config,
            live: LiveStore::File,
        }
    }

    /// These paths with the live login kept in the macOS Keychain item of `service` and `account`, read and written
    /// through the `security` tool at `security` ([`LiveStore::Keychain`]).
    #[must_use]
    pub fn with_keychain(
        self,
        service: impl Into<String>,
        account: impl Into<String>,
        security: impl Into<PathBuf>,
    ) -> Self {
        Self {
            live: LiveStore::Keychain {
                service: service.into(),
                account: account.into(),
                security: security.into(),
            },
            ..self
        }
    }

    /// Where Claude Code keeps its live login.
    #[must_use]
    pub fn live_store(&self) -> &LiveStore {
        &self.live
    }

    /// Claude Code's global config file, `~/.claude.json` by default ([`resolve_claude_paths`]): its `oauthAccount`
    /// names the account logged in now.
    #[must_use]
    pub fn global_config(&self) -> &Path {
        &self.global_config
    }

    /// Claude Code's live credential file, `<config home>/.credentials.json`: the active account's tokens, which
    /// Claude Code renews. With [`LiveStore::Keychain`] the item comes first, and the file only stands in when there is
    /// no item.
    #[must_use]
    pub fn credentials_file(&self) -> PathBuf {
        self.config_home.join(".credentials.json")
    }

    /// Claude Code's primary OAuth refresh lock, `<config home>/.oauth_refresh.lock`.
    #[must_use]
    pub fn oauth_refresh_lock(&self) -> PathBuf {
        self.config_home.join(".oauth_refresh.lock")
    }

    /// Claude Code's legacy credentials lock beside the config home, `~/.claude.lock`.
    #[must_use]
    pub fn credentials_lock(&self) -> PathBuf {
        sibling_lock(&self.config_home)
    }

    /// Claude Code's global config lock, `<global config>.lock`: `~/.claude.json.lock` by default, and beside whichever
    /// file [`resolve_claude_paths`] chose.
    #[must_use]
    pub fn config_lock(&self) -> PathBuf {
        sibling_lock(&self.global_config)
    }
}

/// The variable that moves Claude Code's config home, and its global config with it.
const CONFIG_DIR_VAR: &str = "CLAUDE_CONFIG_DIR";

/// The variable that points Claude Code at a custom OAuth server, which gives its global config its own name.
const CUSTOM_OAUTH_VAR: &str = "CLAUDE_CODE_CUSTOM_OAUTH_URL";

/// The variable Claude Code reads its home from on Windows before the OS's own home folder.
const USER_PROFILE_VAR: &str = "USERPROFILE";

/// Claude Code's config home and global config as Claude Code finds them, from `env`, the environment it runs with,
/// `os_home`, the OS's home folder, and `has_content`, whether a file exists and is not empty ([`file_has_content`] on
/// disk). An empty variable counts as unset.
///
/// - The config home is `CLAUDE_CONFIG_DIR` when it is set, else `<home>/.claude`, where home is `USERPROFILE` on
///   Windows when it is set, else `os_home`. With `CLAUDE_CONFIG_DIR` set neither `USERPROFILE` nor `os_home` is
///   consulted, and `has_content` is only asked about a file inside it.
/// - The global config is `<config home>/.config.json` when that file is not empty, else `.claude.json` in
///   `CLAUDE_CONFIG_DIR`, or in home when it is unset; `.claude-custom-oauth.json` in its place when
///   `CLAUDE_CODE_CUSTOM_OAUTH_URL` is set.
///
/// The config lock follows the chosen file ([`ClaudePaths::config_lock`]). The live login is kept in the live
/// credential file ([`LiveStore::File`]). `None` when no home is found.
pub fn resolve_claude_paths(
    env: impl Fn(&str) -> Option<OsString>,
    os_home: impl FnOnce() -> Option<PathBuf>,
    has_content: impl Fn(&Path) -> bool,
) -> Option<ClaudePaths> {
    resolve_paths_on(cfg!(windows), env, os_home, has_content)
}

/// [`resolve_claude_paths`] with `windows` saying whether `USERPROFILE` is read.
fn resolve_paths_on(
    windows: bool,
    env: impl Fn(&str) -> Option<OsString>,
    os_home: impl FnOnce() -> Option<PathBuf>,
    has_content: impl Fn(&Path) -> bool,
) -> Option<ClaudePaths> {
    let set = |name: &str| env(name).filter(|value| !value.is_empty());
    let (config_home, config_folder) = if let Some(dir) = set(CONFIG_DIR_VAR) {
        let dir = PathBuf::from(dir);
        (dir.clone(), dir)
    } else {
        let profile = if windows { set(USER_PROFILE_VAR) } else { None };
        let home = profile.map(PathBuf::from).or_else(os_home)?;
        (home.join(".claude"), home)
    };
    let legacy = config_home.join(".config.json");
    if has_content(&legacy) {
        return Some(ClaudePaths::new(config_home, legacy));
    }
    let suffix = if set(CUSTOM_OAUTH_VAR).is_some() {
        "-custom-oauth"
    } else {
        ""
    };
    let global_config = config_folder.join(format!(".claude{suffix}.json"));
    Some(ClaudePaths::new(config_home, global_config))
}

/// Whether `path` is a file holding at least one byte.
#[must_use]
pub fn file_has_content(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() > 0)
}

/// `<parent>/<name>.lock` for `<parent>/<name>`, a trailing separator ignored.
fn sibling_lock(path: &Path) -> PathBuf {
    let Some(name) = path.file_name() else {
        let mut text = path.as_os_str().to_owned();
        text.push(".lock");
        return PathBuf::from(text);
    };
    let mut lock_name = OsString::from(name);
    lock_name.push(".lock");
    match path.parent() {
        Some(parent) => parent.join(lock_name),
        None => PathBuf::from(lock_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// An absolute folder on this platform.
    fn absolute(name: &str) -> PathBuf {
        let root = if cfg!(windows) { r"X:\" } else { "/" };
        Path::new(root).join(name)
    }

    /// An environment of `pairs`, every other variable unset.
    fn env_of<'a>(pairs: &'a [(&'static str, PathBuf)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone().into_os_string())
        }
    }

    /// The paths resolved on Windows from `pairs`, an OS home of `os`, and the files in `full` the only ones with
    /// content.
    fn resolve(
        pairs: &[(&'static str, PathBuf)],
        os: &str,
        full: &[PathBuf],
    ) -> Option<ClaudePaths> {
        resolve_paths_on(
            true,
            env_of(pairs),
            || Some(absolute(os)),
            |path| full.iter().any(|file| file == path),
        )
    }

    #[test]
    fn config_dir_names_its_claude_json() {
        let dir = absolute("cfg");
        let paths = resolve(&[(CONFIG_DIR_VAR, dir.clone())], "os", &[]);
        assert_eq!(
            paths,
            Some(ClaudePaths::new(dir.clone(), dir.join(".claude.json")))
        );
    }

    #[test]
    fn config_dir_with_custom_oauth_names_the_custom_oauth_file() {
        let dir = absolute("cfg");
        let pairs = [
            (CONFIG_DIR_VAR, dir.clone()),
            (CUSTOM_OAUTH_VAR, PathBuf::from("https://auth.example")),
        ];
        let paths = resolve(&pairs, "os", &[]);
        assert_eq!(
            paths,
            Some(ClaudePaths::new(
                dir.clone(),
                dir.join(".claude-custom-oauth.json")
            ))
        );
    }

    #[test]
    fn config_dir_with_a_non_empty_config_json_names_it() {
        let dir = absolute("cfg");
        let paths = resolve(
            &[(CONFIG_DIR_VAR, dir.clone())],
            "os",
            &[dir.join(".config.json"), dir.join(".claude.json")],
        );
        assert_eq!(
            paths,
            Some(ClaudePaths::new(dir.clone(), dir.join(".config.json")))
        );
    }

    #[test]
    fn config_dir_with_an_empty_config_json_names_claude_json() {
        let dir = absolute("cfg");
        let paths = resolve(
            &[(CONFIG_DIR_VAR, dir.clone())],
            "os",
            &[dir.join(".claude.json")],
        );
        assert_eq!(
            paths,
            Some(ClaudePaths::new(dir.clone(), dir.join(".claude.json")))
        );
    }

    #[test]
    fn user_profile_names_its_claude_json_and_config_home() {
        let profile = absolute("profile");
        let paths = resolve(&[(USER_PROFILE_VAR, profile.clone())], "os", &[]);
        assert_eq!(
            paths,
            Some(ClaudePaths::new(
                profile.join(".claude"),
                profile.join(".claude.json")
            ))
        );
    }

    #[test]
    fn user_profile_config_json_under_its_claude_folder_wins_when_not_empty() {
        let profile = absolute("profile");
        let legacy = profile.join(".claude").join(".config.json");
        let paths = resolve(
            &[(USER_PROFILE_VAR, profile.clone())],
            "os",
            std::slice::from_ref(&legacy),
        );
        assert_eq!(
            paths,
            Some(ClaudePaths::new(profile.join(".claude"), legacy))
        );
    }

    #[test]
    fn unset_everything_falls_back_to_the_os_home() {
        let os = absolute("os");
        let empty = [
            (CONFIG_DIR_VAR, PathBuf::new()),
            (USER_PROFILE_VAR, PathBuf::new()),
        ];
        let paths = resolve(&empty, "os", &[]);
        assert_eq!(
            paths,
            Some(ClaudePaths::new(
                os.join(".claude"),
                os.join(".claude.json")
            ))
        );
    }

    #[test]
    fn off_windows_user_profile_is_ignored() {
        let os = absolute("os");
        let pairs = [(USER_PROFILE_VAR, absolute("profile"))];
        let paths = resolve_paths_on(false, env_of(&pairs), || Some(os.clone()), |_| false);
        assert_eq!(
            paths,
            Some(ClaudePaths::new(
                os.join(".claude"),
                os.join(".claude.json")
            ))
        );
    }

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "a test fails loudly on a broken scratch dir"
    )]
    fn file_has_content_needs_a_file_of_at_least_one_byte() {
        let dir = std::env::temp_dir().join(format!("rt-paths-content-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create the scratch dir");
        let (empty, one, missing) = (dir.join("empty"), dir.join("one"), dir.join("missing"));
        fs::write(&empty, b"").expect("write the empty file");
        fs::write(&one, b"x").expect("write the one-byte file");
        let seen = (
            file_has_content(&empty),
            file_has_content(&one),
            file_has_content(&missing),
        );
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(seen, (false, true, false));
    }

    #[test]
    fn the_config_lock_follows_the_chosen_file() {
        let dir = absolute("cfg");
        let plain =
            resolve(&[(CONFIG_DIR_VAR, dir.clone())], "os", &[]).map(|paths| paths.config_lock());
        assert_eq!(plain, Some(dir.join(".claude.json.lock")));
        let legacy = resolve(
            &[(CONFIG_DIR_VAR, dir.clone())],
            "os",
            &[dir.join(".config.json")],
        )
        .map(|paths| paths.config_lock());
        assert_eq!(legacy, Some(dir.join(".config.json.lock")));
    }

    #[test]
    fn config_dir_never_reads_user_profile_the_os_home_or_a_file_outside_it() {
        let dir = absolute("cfg");
        let pairs = [
            (CONFIG_DIR_VAR, dir.clone()),
            (USER_PROFILE_VAR, absolute("profile")),
        ];
        let asked = RefCell::new(Vec::new());
        let probed = RefCell::new(Vec::new());
        let os_home_read = RefCell::new(false);
        let paths = resolve_paths_on(
            true,
            |name| {
                asked.borrow_mut().push(name.to_owned());
                env_of(&pairs)(name)
            },
            || {
                *os_home_read.borrow_mut() = true;
                Some(absolute("os"))
            },
            |path| {
                probed.borrow_mut().push(path.to_path_buf());
                false
            },
        );
        assert_eq!(
            paths,
            Some(ClaudePaths::new(dir.clone(), dir.join(".claude.json")))
        );
        assert!(
            !asked.borrow().iter().any(|name| name == USER_PROFILE_VAR),
            "{:?}",
            asked.borrow()
        );
        assert!(!*os_home_read.borrow(), "the OS home was read");
        assert!(
            !probed.borrow().is_empty(),
            "the config dir's .config.json was not probed"
        );
        assert!(
            probed.borrow().iter().all(|path| path.starts_with(&dir)),
            "{:?}",
            probed.borrow()
        );
    }
}
