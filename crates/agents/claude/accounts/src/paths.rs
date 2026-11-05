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

/// Claude Code's own files: its config home (`~/.claude`) and its global config file (`~/.claude.json`), and where it
/// keeps its live login ([`LiveStore`]).
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

    /// Claude Code's global config file, `~/.claude.json`: its `oauthAccount` names the account logged in now.
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

    /// Claude Code's global config lock, `~/.claude.json.lock`.
    #[must_use]
    pub fn config_lock(&self) -> PathBuf {
        sibling_lock(&self.global_config)
    }
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
