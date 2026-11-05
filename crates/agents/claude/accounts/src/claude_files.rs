//! Claude Code's own files: the live credential file `.credentials.json` and the
//! global config `~/.claude.json`; and the live login read from both, with or without Claude Code's locks, for a
//! capture into the account store, with the identity an `oauthAccount` names.
//!
//! Each write goes to a temp file in the target's own folder, is read back and parsed, and is then renamed onto the
//! target, so a reader sees the old file or the new one, never a torn one. A `~/.claude.json` update is a whole-file
//! round trip that keeps every key it does not change, in its order, and refuses a file that exists but cannot be
//! read: writing over a file it never read would lose the user's whole Claude Code state.
//!
//! Each writer takes the guard of the lock Claude Code holds around that file, and refuses a guard taken for other
//! paths: the live credential file under its two credential locks, `~/.claude.json` under its config lock. A guard
//! whose lock is no longer held (taken over as stale while held) is refused too, before anything is written.
//!
//! A file is read into a buffer that never grows and is zeroed on drop, its parsed tree is zeroed as it drops, and a
//! write serializes into a buffer sized exactly. One limit is `serde_json`'s and cannot be fixed from here: it decodes
//! an escaped string through an internal scratch buffer it never zeroes, and a parse that fails partway frees the part
//! of the tree it built without zeroing it. Values read from `~/.claude.json` or the live credential file can therefore
//! be left in freed memory, unzeroed.

use std::fs;
use std::io;
use std::path::Path;

use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::locks::{ClaudeCredentialsGuard, DirLockGuard};
use crate::paths::ClaudePaths;
use crate::refresh::{Scrubbed, scrub};
use crate::secret::CredentialBlob;
use crate::store;
use crate::usage::Identity;

/// Why a write to Claude Code's files did not happen. It carries no file contents.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClaudeFileError {
    /// The lock guard given is not Claude Code's lock for this file (it was taken for other paths): nothing was read
    /// or written.
    #[error("the lock held is not Claude Code's lock for this file; nothing was written")]
    WrongLock,
    /// The lock guard given is no longer held: its lock went stale while held (the machine slept, or touching it
    /// stopped) and may have been taken over by another process. Nothing was read or written.
    #[error(
        "the lock for this file was lost while held and may belong to another process now; nothing was written"
    )]
    LockLost,
    /// `~/.claude.json` exists but could not be read, or is not a JSON object: it was left as it was.
    #[error(
        "the global config exists but could not be read; it was left untouched (move or repair it, then retry)"
    )]
    ConfigUnreadable,
    /// The file, `~/.claude.json` or the live credential file, no longer holds what the caller read before deciding to
    /// rewrite it (another writer changed, created or removed it since): it was left as it was.
    #[error("the file changed after it was read; it was left untouched")]
    Changed,
    /// The temp file could not be written, read back or renamed onto the target, or a rollback could not read the file
    /// it was to put back or remove; the target was left as it was, and the temp file removed.
    #[error("the file could not be read or written")]
    Write(#[source] io::Error),
    /// As [`ClaudeFileError::Write`], but the temp file could not be removed either: it is still on disk beside the
    /// target, holding the bytes that were to be written (for the live credential file, its tokens).
    #[error(
        "the file could not be written, and its temp file, which holds the new contents, could not be removed"
    )]
    WriteLeftTemp(#[source] io::Error),
    /// The live credential file exists but could not be read, or is not UTF-8 text. Nothing was written.
    #[error("Claude Code's live credential file exists but could not be read; nothing was written")]
    CredentialsUnreadable(#[source] io::Error),
}

/// Writes `blob` as Claude Code's live credential file, `<config home>/.credentials.json`, making its folder first:
/// through a temp file in the same folder renamed onto it, while
/// `guard`, Claude Code's credential locks for `paths` ([`crate::ClaudeCodeLocks::credentials`]), is held. On Unix the
/// file is readable by its owner alone. Whatever the file holds is replaced: a switch writes through
/// `rewrite_live_credentials` instead, with the bytes its snapshot read.
///
/// # Errors
///
/// [`ClaudeFileError::WrongLock`] when `guard` holds another config home's locks (nothing is written);
/// [`ClaudeFileError::LockLost`] when either of its locks is no longer held (nothing is written);
/// [`ClaudeFileError::Write`] when the folder, the temp file or the rename fails; the old file is then unchanged and
/// the temp file removed; [`ClaudeFileError::WriteLeftTemp`] when the temp file could not be removed after that.
pub fn write_live_credentials(
    paths: &ClaudePaths,
    guard: &ClaudeCredentialsGuard,
    blob: &CredentialBlob,
) -> Result<(), ClaudeFileError> {
    check_credentials_guard(paths, guard)?;
    let target = paths.credentials_file();
    if let Some(folder) = target.parent() {
        fs::create_dir_all(folder).map_err(ClaudeFileError::Write)?;
    }
    replace_file(&target, blob.expose().as_bytes())
}

/// As [`write_live_credentials`], only while the live credential file holds what `expected` says: the file is read
/// under `guard` and compared before the write. The compare and the replace are two steps, so a writer that ignores the
/// lock can still land between them.
///
/// # Errors
///
/// As [`write_live_credentials`]; [`ClaudeFileError::Write`] also when the file could not be read for the compare, and
/// [`ClaudeFileError::Changed`] when it does not hold what `expected` says. Neither writes anything.
pub(crate) fn rewrite_live_credentials(
    paths: &ClaudePaths,
    guard: &ClaudeCredentialsGuard,
    expected: Expected<'_>,
    blob: &CredentialBlob,
) -> Result<(), ClaudeFileError> {
    check_credentials_guard(paths, guard)?;
    let held = store::read_optional(&paths.credentials_file()).map_err(ClaudeFileError::Write)?;
    if !expected.matches(held.as_deref().map(Vec::as_slice)) {
        return Err(ClaudeFileError::Changed);
    }
    write_live_credentials(paths, guard, blob)
}

/// Updates Claude Code's global config, `paths`' `~/.claude.json`, in place,
/// while `guard`, Claude Code's config lock for `paths` ([`crate::ClaudeCodeLocks::config`]), is held: reads the whole
/// file (an absent one is an empty object), lets `mutator` change it, and writes it back as two-space-indented JSON
/// with every other key kept in its order and every number as it was read. `mutator` returns whether it changed
/// anything; when it did not, nothing is written. Returns whether the file was written.
///
/// # Errors
///
/// [`ClaudeFileError::WrongLock`] when `guard` is not `paths`' config lock (nothing is read or written);
/// [`ClaudeFileError::LockLost`] when the lock is no longer held (nothing is read or written);
/// [`ClaudeFileError::ConfigUnreadable`] when the file exists but cannot be read or is not a JSON object (nothing is
/// written); [`ClaudeFileError::Write`] when the write fails (the old file is unchanged), or
/// [`ClaudeFileError::WriteLeftTemp`] when its temp file could not be removed after that.
pub fn update_global_config(
    paths: &ClaudePaths,
    guard: &DirLockGuard,
    mutator: impl FnOnce(&mut Map<String, Value>) -> bool,
) -> Result<bool, ClaudeFileError> {
    let mut config = read_expected_config(paths, guard, Expected::Unchecked)?;
    if !mutator(&mut config.0) {
        return Ok(false);
    }
    write_config(paths.global_config(), &config.0)?;
    Ok(true)
}

/// What a live file must hold for [`rewrite_global_config`] to rewrite `~/.claude.json`, or for
/// [`rewrite_live_credentials`] to rewrite the live credential file: the bytes a caller read before it
/// decided on the change, or no file at all.
#[derive(Clone, Copy)]
pub(crate) enum Expected<'a> {
    /// Any contents: the caller decided nothing from an earlier read.
    Unchecked,
    /// No file.
    Absent,
    /// Exactly these bytes.
    Bytes(&'a [u8]),
}

impl Expected<'_> {
    /// Whether `held`, the file's bytes or `None` for no file, is what this expects.
    fn matches(self, held: Option<&[u8]>) -> bool {
        match self {
            Self::Unchecked => true,
            Self::Absent => held.is_none(),
            Self::Bytes(expected) => held == Some(expected),
        }
    }
}

/// As [`update_global_config`], only while the file holds what `expected` says, and always writing what `mutator`
/// leaves: the bytes written, in a buffer zeroed on drop. The file is read once, and that read is both compared and
/// changed. The compare and the replace are two steps, so a writer that ignores the lock can still land between them.
/// A switch keeps the bytes written, so its rollback restores or removes the file only while it still holds them.
///
/// # Errors
///
/// As [`update_global_config`], and [`ClaudeFileError::Changed`] when the file does not hold what `expected` says
/// (`mutator` is not called, and nothing is written).
pub(crate) fn rewrite_global_config(
    paths: &ClaudePaths,
    guard: &DirLockGuard,
    expected: Expected<'_>,
    mutator: impl FnOnce(&mut Map<String, Value>),
) -> Result<Zeroizing<Vec<u8>>, ClaudeFileError> {
    let mut config = read_expected_config(paths, guard, expected)?;
    mutator(&mut config.0);
    write_config(paths.global_config(), &config.0)
}

/// `paths`' `~/.claude.json`, read once while `guard`, its config lock, is held, and parsed, when it holds what
/// `expected` says: the guard check, the compare and the parse a rewrite starts with.
fn read_expected_config(
    paths: &ClaudePaths,
    guard: &DirLockGuard,
    expected: Expected<'_>,
) -> Result<ScrubbedConfig, ClaudeFileError> {
    check_config_guard(paths, guard)?;
    let held = store::read_optional(paths.global_config())
        .map_err(|_| ClaudeFileError::ConfigUnreadable)?;
    let held = held.as_deref().map(Vec::as_slice);
    if !expected.matches(held) {
        return Err(ClaudeFileError::Changed);
    }
    Ok(ScrubbedConfig(parse_config(held)?))
}

/// [`ClaudeFileError::WrongLock`] when `guard` is not `paths`' config lock, [`ClaudeFileError::LockLost`] when it is no
/// longer held.
fn check_config_guard(paths: &ClaudePaths, guard: &DirLockGuard) -> Result<(), ClaudeFileError> {
    if guard.path() != paths.config_lock() {
        return Err(ClaudeFileError::WrongLock);
    }
    if !guard.still_owned() {
        return Err(ClaudeFileError::LockLost);
    }
    Ok(())
}

/// [`ClaudeFileError::WrongLock`] when `guard` holds another config home's credential locks than `paths`',
/// [`ClaudeFileError::LockLost`] when either of its locks is no longer held.
fn check_credentials_guard(
    paths: &ClaudePaths,
    guard: &ClaudeCredentialsGuard,
) -> Result<(), ClaudeFileError> {
    if !guard.guards(paths) {
        return Err(ClaudeFileError::WrongLock);
    }
    if !guard.still_owned() {
        return Err(ClaudeFileError::LockLost);
    }
    Ok(())
}

/// Writes `config` as `path`, two-space-indented, making its folder first: the bytes written, in a buffer zeroed on
/// drop.
fn write_config(
    path: &Path,
    config: &Map<String, Value>,
) -> Result<Zeroizing<Vec<u8>>, ClaudeFileError> {
    let bytes = pretty_exact(config)?;
    if let Some(folder) = path
        .parent()
        .filter(|folder| !folder.as_os_str().is_empty())
    {
        fs::create_dir_all(folder).map_err(ClaudeFileError::Write)?;
    }
    replace_file(path, &bytes)?;
    Ok(bytes)
}

/// A parsed `~/.claude.json` whose strings are zeroed when it drops, on every path out of an update: it holds
/// `oauthAccount` and MCP servers' `env` values.
struct ScrubbedConfig(Map<String, Value>);

impl Drop for ScrubbedConfig {
    fn drop(&mut self) {
        self.0.values_mut().for_each(scrub);
    }
}

/// `config` as two-space-indented JSON, in a buffer sized exactly by a first pass that only counts the bytes: a buffer
/// that grew would free an unzeroed copy of what it held so far (`oauthAccount`, MCP servers' `env` values).
fn pretty_exact(config: &Map<String, Value>) -> Result<Zeroizing<Vec<u8>>, ClaudeFileError> {
    let failed = |error: serde_json::Error| ClaudeFileError::Write(io::Error::other(error));
    let mut counted = ByteCount(0);
    serde_json::to_writer_pretty(&mut counted, config).map_err(failed)?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(counted.0));
    serde_json::to_writer_pretty(&mut *bytes, config).map_err(failed)?;
    Ok(bytes)
}

/// A writer that keeps nothing and counts the bytes written to it.
struct ByteCount(usize);

impl io::Write for ByteCount {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `~/.claude.json`'s bytes, as read, parsed as a JSON object: an absent file (`None`) is an empty object. A parsed
/// value that is not an object is zeroed as it drops.
fn parse_config(bytes: Option<&[u8]>) -> Result<Map<String, Value>, ClaudeFileError> {
    let Some(bytes) = bytes else {
        return Ok(Map::new());
    };
    let mut tree =
        Scrubbed(serde_json::from_slice(bytes).map_err(|_| ClaudeFileError::ConfigUnreadable)?);
    match &mut tree.0 {
        Value::Object(config) => Ok(std::mem::take(config)),
        _ => Err(ClaudeFileError::ConfigUnreadable),
    }
}

/// The login an `oauthAccount` value names: its non-empty `emailAddress`, and its `organizationUuid` (a missing one
/// counting as none). `None` when it names no email.
pub(crate) fn identity_in(account: &Value) -> Option<Identity> {
    let email = account
        .get("emailAddress")
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())?;
    let org_uuid = account
        .get("organizationUuid")
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(Identity::new(email, org_uuid))
}

/// Claude Code's live login as read: the credential file's whole text, and from `~/.claude.json` its `oauthAccount`
/// object and whether it holds an API key.
pub(crate) struct LiveLogin {
    /// The live credential file's text; `None` when the file does not exist.
    pub(crate) credential: Option<CredentialBlob>,
    /// `~/.claude.json`'s `oauthAccount` when it is an object; `None` when the file or the key is missing, or the key is
    /// something else.
    pub(crate) oauth_account: Option<Value>,
    /// Whether `~/.claude.json` holds a truthy `primaryApiKey`, as the switch reads it.
    pub(crate) api_key: bool,
}

/// The live login, read with no lock: the first look a capture takes before it asks the profile oracle. Both files are
/// read as the switch reads them, following a link (a user may link them), under the store's size cap;
/// `~/.claude.json`'s parsed tree is zeroed as it drops, keeping only `oauthAccount` and whether an API key is there.
///
/// # Errors
///
/// [`ClaudeFileError::CredentialsUnreadable`] when the credential file exists but cannot be read or is not UTF-8;
/// [`ClaudeFileError::ConfigUnreadable`] when `~/.claude.json` exists but cannot be read or is not a JSON object.
pub(crate) fn read_live_login(paths: &ClaudePaths) -> Result<LiveLogin, ClaudeFileError> {
    let credential = match store::read_optional(&paths.credentials_file())
        .map_err(ClaudeFileError::CredentialsUnreadable)?
    {
        Some(bytes) => {
            let text = std::str::from_utf8(&bytes).map_err(|error| {
                ClaudeFileError::CredentialsUnreadable(io::Error::new(
                    io::ErrorKind::InvalidData,
                    error,
                ))
            })?;
            Some(CredentialBlob::new(text.to_owned()))
        }
        None => None,
    };
    let held = store::read_optional(paths.global_config())
        .map_err(|_| ClaudeFileError::ConfigUnreadable)?;
    let config = ScrubbedConfig(parse_config(held.as_deref().map(Vec::as_slice))?);
    Ok(LiveLogin {
        credential,
        oauth_account: config
            .0
            .get("oauthAccount")
            .filter(|value| value.is_object())
            .cloned(),
        api_key: config.0.get("primaryApiKey").is_some_and(store::truthy),
    })
}

/// [`read_live_login`] while `credentials`, Claude Code's credential locks for `paths`, and `config`, its config lock,
/// are held: the read a capture writes from.
///
/// # Errors
///
/// [`ClaudeFileError::WrongLock`] when a guard was taken for other paths, [`ClaudeFileError::LockLost`] when a lock is
/// no longer held (nothing is read), else as [`read_live_login`].
pub(crate) fn read_live_login_locked(
    paths: &ClaudePaths,
    credentials: &ClaudeCredentialsGuard,
    config: &DirLockGuard,
) -> Result<LiveLogin, ClaudeFileError> {
    check_credentials_guard(paths, credentials)?;
    check_config_guard(paths, config)?;
    read_live_login(paths)
}

/// Drops `primaryApiKey` from Claude Code's global config, `paths`' `~/.claude.json`,
/// when it makes an OAuth credential live: a managed API key left beside it would be billed instead. `guard` is
/// Claude Code's config lock for `paths`, held. Every other key is kept, and `customApiKeyResponses` is left alone.
/// Returns whether a key was removed; a config without one (or no config at all) is not rewritten.
///
/// # Errors
///
/// As [`update_global_config`]: a guard for other paths is refused, and an unreadable config is refused rather than
/// overwritten unread.
pub fn clear_primary_api_key(
    paths: &ClaudePaths,
    guard: &DirLockGuard,
) -> Result<bool, ClaudeFileError> {
    update_global_config(paths, guard, |config| {
        match config.shift_remove("primaryApiKey") {
            Some(mut key) => {
                scrub(&mut key);
                true
            }
            None => false,
        }
    })
}

/// Writes `bytes` back as `paths`' live credential file, byte for byte, while `guard`, Claude Code's credential locks
/// for `paths`, is still held, and only while the file holds exactly `written`: the rollback of a switch that wrote
/// those bytes over it. `Ok(true)` when the file was put back, `Ok(false)` when it holds other bytes, written since, or
/// is gone, and was left as it is. Unlike [`write_live_credentials`] the read-back only compares the bytes, since the
/// file being put back need not have been JSON. The compare and the replace are two steps, so a writer that ignores the
/// lock can still land between them.
///
/// # Errors
///
/// As [`write_live_credentials`], and [`ClaudeFileError::Write`] when the file could not be read for the compare.
pub(crate) fn restore_live_credentials(
    paths: &ClaudePaths,
    guard: &ClaudeCredentialsGuard,
    written: &[u8],
    bytes: &[u8],
) -> Result<bool, ClaudeFileError> {
    check_credentials_guard(paths, guard)?;
    restore_if_holding(&paths.credentials_file(), written, bytes)
}

/// Writes `bytes` back as `paths`' `~/.claude.json`, byte for byte, while `guard`, its config lock, is still held, and
/// only while the file holds exactly `written`: the rollback of a switch that wrote those bytes over it. `Ok(true)`
/// when the file was put back, `Ok(false)` when it holds other bytes, written since, or is gone, and was left as it
/// is. The compare and the replace are two steps, so a writer that ignores the lock can still land between them.
///
/// # Errors
///
/// [`ClaudeFileError::WrongLock`], [`ClaudeFileError::LockLost`], [`ClaudeFileError::Write`] or
/// [`ClaudeFileError::WriteLeftTemp`], as [`update_global_config`]; [`ClaudeFileError::Write`] also when the file
/// could not be read for the compare.
pub(crate) fn restore_global_config(
    paths: &ClaudePaths,
    guard: &DirLockGuard,
    written: &[u8],
    bytes: &[u8],
) -> Result<bool, ClaudeFileError> {
    check_config_guard(paths, guard)?;
    restore_if_holding(paths.global_config(), written, bytes)
}

/// Writes `bytes` as `target`, byte for byte, when it holds exactly `written`: whether it was written. A failed read of
/// `target` is reported as [`ClaudeFileError::Write`].
fn restore_if_holding(
    target: &Path,
    written: &[u8],
    bytes: &[u8],
) -> Result<bool, ClaudeFileError> {
    let held = store::read_optional(target).map_err(ClaudeFileError::Write)?;
    if held.as_deref().map(Vec::as_slice) != Some(written) {
        return Ok(false);
    }
    replace_checked(target, bytes, |temp| store::read_back_equals(temp, bytes))?;
    Ok(true)
}

/// Removes `paths`' live credential file while `guard`, Claude Code's credential locks for `paths`, is still held, and
/// only while it holds exactly `written`: the rollback of a switch that created it with those bytes. `Ok(true)` when the
/// file is gone (one already gone included), `Ok(false)` when it holds other bytes, written since, and was left.
///
/// # Errors
///
/// [`ClaudeFileError::WrongLock`] or [`ClaudeFileError::LockLost`], as [`write_live_credentials`] (nothing is removed);
/// [`ClaudeFileError::Write`] when the file could not be read or removed.
pub(crate) fn remove_live_credentials(
    paths: &ClaudePaths,
    guard: &ClaudeCredentialsGuard,
    written: &[u8],
) -> Result<bool, ClaudeFileError> {
    check_credentials_guard(paths, guard)?;
    remove_if_holding(&paths.credentials_file(), written)
}

/// Removes `paths`' `~/.claude.json` while `guard`, its config lock, is still held, and only while it holds exactly
/// `written`: the rollback of a switch that created it with those bytes. `Ok(true)` when the file is gone (one already
/// gone included), `Ok(false)` when it holds other bytes, written since, and was left.
///
/// # Errors
///
/// [`ClaudeFileError::WrongLock`] or [`ClaudeFileError::LockLost`], as [`update_global_config`] (nothing is removed);
/// [`ClaudeFileError::Write`] when the file could not be read or removed.
pub(crate) fn remove_global_config(
    paths: &ClaudePaths,
    guard: &DirLockGuard,
    written: &[u8],
) -> Result<bool, ClaudeFileError> {
    check_config_guard(paths, guard)?;
    remove_if_holding(paths.global_config(), written)
}

/// Removes `path` when it holds exactly `written`: whether it is gone. A failed read of `path` is reported as
/// [`ClaudeFileError::Write`].
fn remove_if_holding(path: &Path, written: &[u8]) -> Result<bool, ClaudeFileError> {
    remove_if_holding_with(path, written, || ())
}

/// As [`remove_if_holding`], running `between` after the compare matched and before the delete. The compare and the
/// delete go through one handle that shares reading alone, so no other writer can swap the file in between. A failed
/// read of `path` is reported as [`ClaudeFileError::Write`].
#[cfg(all(windows, target_pointer_width = "64"))]
fn remove_if_holding_with(
    path: &Path,
    written: &[u8],
    between: impl FnOnce(),
) -> Result<bool, ClaudeFileError> {
    crate::win32::remove_file_if_holding(path, written, between).map_err(ClaudeFileError::Write)
}

/// As [`remove_if_holding`], running `between` after the compare matched and before the delete. Unix has no removal
/// through the descriptor the compare read, so the file is opened and compared through one descriptor, then its name is
/// renamed to a sibling only this removal knows (`<name>.rt-remove-<pid>-<n>`; rename(2) moves the name in one step),
/// and the entry set aside is removed only when it is the file the descriptor read (same device and inode, a link
/// followed as the open followed it). A file another writer put at the name after the compare is renamed back
/// without replacing anything made at the name meanwhile, and reported kept (`Ok(false)`); when that put-back fails it
/// is left set aside with a warning. A failed read of `path` is reported as [`ClaudeFileError::Write`].
#[cfg(unix)]
fn remove_if_holding_with(
    path: &Path,
    written: &[u8],
    between: impl FnOnce(),
) -> Result<bool, ClaudeFileError> {
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;

    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(ClaudeFileError::Write(error)),
    };
    // One byte past `written` is enough to tell a longer file from it.
    let limit = u64::try_from(written.len())
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut held = Zeroizing::new(Vec::with_capacity(written.len().saturating_add(1)));
    (&file)
        .take(limit)
        .read_to_end(&mut held)
        .map_err(ClaudeFileError::Write)?;
    if held.as_slice() != written {
        return Ok(false);
    }
    let compared = file.metadata().map_err(ClaudeFileError::Write)?;
    between();
    let Some(name) = path.file_name() else {
        return Err(ClaudeFileError::Write(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the file to remove has no name",
        )));
    };
    let aside = path.with_file_name(crate::locks::aside_name(name));
    match fs::rename(path, &aside) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(ClaudeFileError::Write(error)),
    }
    let same = fs::metadata(&aside)
        .is_ok_and(|renamed| renamed.dev() == compared.dev() && renamed.ino() == compared.ino());
    if same {
        return match fs::remove_file(&aside) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(error) => {
                put_back(&aside, path);
                Err(ClaudeFileError::Write(error))
            }
        };
    }
    put_back(&aside, path);
    Ok(false)
}

/// Renames the file set aside as `aside` back to `path`, never replacing a file made at `path` meanwhile (macOS's
/// `renameatx_np(2)` with `RENAME_EXCL` fails with `EEXIST` when the destination exists); when that fails it is left
/// set aside with a warning naming the file by a fixed description, so no path under the user's profile is logged, and
/// the set-aside name's shape.
#[cfg(unix)]
fn put_back(aside: &Path, path: &Path) {
    if let Err(error) = crate::locks::rename_exclusive(aside, path) {
        let file = match path.file_name().and_then(|name| name.to_str()) {
            Some(".credentials.json") => "Claude Code's live credential file",
            Some(".claude.json") => "Claude Code's global config",
            _ => "a Claude Code file",
        };
        tracing::warn!(
            file,
            set_aside_as = "<file name>.rt-remove-<pid>-<n>, beside the file",
            error = %error.kind(),
            "a file set aside for removal was not the one compared and could not be put back; it was left set aside"
        );
    }
}

/// As [`remove_if_holding`], running `between` after the compare matched and before the delete: a read, a compare and
/// a removal by name. Neither Unix nor 64-bit Windows takes locks, so the switch, the only caller, never runs here. A
/// failed read of `path` is reported as [`ClaudeFileError::Write`].
#[cfg(not(any(unix, all(windows, target_pointer_width = "64"))))]
fn remove_if_holding_with(
    path: &Path,
    written: &[u8],
    between: impl FnOnce(),
) -> Result<bool, ClaudeFileError> {
    let Some(held) = store::read_optional(path).map_err(ClaudeFileError::Write)? else {
        return Ok(true);
    };
    if held.as_slice() != written {
        return Ok(false);
    }
    between();
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(ClaudeFileError::Write(error)),
    }
}

/// Writes `bytes` to a temp file beside `target` that this write creates, reads them back and parses them as JSON,
/// then renames the temp file onto `target`. The read-back stays in a buffer zeroed on drop: the bytes may hold tokens.
fn replace_file(target: &Path, bytes: &[u8]) -> Result<(), ClaudeFileError> {
    replace_checked(target, bytes, check_json)
}

/// As [`replace_file`], with `check` run on the temp file before the rename.
fn replace_checked(
    target: &Path,
    bytes: &[u8],
    check: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), ClaudeFileError> {
    store::replace_secret(target, bytes, check).map_err(|failed| {
        if failed.temp_left {
            ClaudeFileError::WriteLeftTemp(failed.error)
        } else {
            ClaudeFileError::Write(failed.error)
        }
    })
}

fn check_json(path: &Path) -> io::Result<()> {
    let written =
        store::read_optional(path)?.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    serde_json::from_slice::<serde::de::IgnoredAny>(&written)
        .map(|_| ())
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the written file did not parse back as JSON",
            )
        })
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test whose config cannot be serialized must fail loudly"
)]
mod tests {
    use serde_json::{Map, json};

    use super::pretty_exact;

    #[test]
    fn a_config_is_serialized_into_a_buffer_it_never_outgrows() {
        let mut config = Map::new();
        config.insert(
            "oauthAccount".to_owned(),
            json!({ "emailAddress": "one@example.test" }),
        );
        config.insert(
            "mcpServers".to_owned(),
            json!({ "s": { "env": { "TOKEN": "x".repeat(20_000) } } }),
        );

        let bytes = pretty_exact(&config).expect("the config serializes");

        assert_eq!(
            bytes.capacity(),
            bytes.len(),
            "the buffer was sized exactly, so it never reallocated"
        );
        assert_eq!(
            bytes.as_slice(),
            serde_json::to_vec_pretty(&config)
                .expect("the config serializes")
                .as_slice()
        );
    }
}

/// The rollback's conditional removal, under Claude Code's locks, which this platform alone takes.
#[cfg(all(test, accounts_locks))]
#[expect(
    clippy::expect_used,
    reason = "a test that cannot set up its files or take its locks must fail loudly"
)]
mod removal_tests {
    use std::fs;

    use super::{remove_global_config, remove_if_holding_with, remove_live_credentials};
    use crate::locks::ClaudeCodeLocks;
    use crate::paths::ClaudePaths;

    const WRITTEN: &[u8] = br#"{"oauthAccount": {"emailAddress": "two@example.test"}}"#;
    const OTHER: &[u8] = br#"{"numStartups": 99}"#;

    /// Claude Code's paths in a fresh folder of the temp folder, with both live files holding `bytes`.
    pub(super) fn live_files(name: &str, bytes: &[u8]) -> ClaudePaths {
        let dir = std::env::temp_dir().join(format!(
            "claude-accounts-claude-files-unit-{name}-{}",
            std::process::id()
        ));
        // A folder left by an earlier run is wiped first; one that does not exist is the usual case.
        let _wiped = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".claude")).expect("the test's folder can be made");
        let paths = ClaudePaths::new(dir.join(".claude"), dir.join(".claude.json"));
        fs::write(paths.credentials_file(), bytes)
            .expect("the live credential file can be written");
        fs::write(paths.global_config(), bytes).expect("the global config can be written");
        paths
    }

    #[test]
    fn a_created_file_still_holding_the_written_bytes_is_removed() {
        let paths = live_files("removed", WRITTEN);
        let credentials =
            ClaudeCodeLocks::credentials(&paths).expect("the credential locks are free");
        let config = ClaudeCodeLocks::config(&paths).expect("the config lock is free");

        let credentials_gone = remove_live_credentials(&paths, &credentials, WRITTEN)
            .expect("the credential file is removed");
        let config_gone =
            remove_global_config(&paths, &config, WRITTEN).expect("the config is removed");

        assert!(
            credentials_gone && config_gone,
            "both removals report the file gone"
        );
        assert!(
            !paths.credentials_file().exists(),
            "the credential file holding the written bytes is removed"
        );
        assert!(
            !paths.global_config().exists(),
            "the config holding the written bytes is removed"
        );
    }

    #[test]
    fn a_created_file_holding_other_bytes_is_kept() {
        let paths = live_files("kept", OTHER);
        let credentials =
            ClaudeCodeLocks::credentials(&paths).expect("the credential locks are free");
        let config = ClaudeCodeLocks::config(&paths).expect("the config lock is free");

        let credentials_gone = remove_live_credentials(&paths, &credentials, WRITTEN)
            .expect("the credential file is read");
        let config_gone =
            remove_global_config(&paths, &config, WRITTEN).expect("the config is read");

        assert!(
            !credentials_gone && !config_gone,
            "neither removal reports the file gone"
        );
        assert_eq!(
            fs::read(paths.credentials_file()).expect("the credential file stays"),
            OTHER
        );
        assert_eq!(
            fs::read(paths.global_config()).expect("the config stays"),
            OTHER
        );
    }

    /// A writer renames another file onto the target between the compare and the removal. Windows compares and
    /// deletes through one handle that refuses the rename; on macOS the rename lands, and the removal, which sets the
    /// file aside and checks it is the one compared, puts the newcomer back.
    #[test]
    fn a_file_swapped_in_after_the_compare_is_not_deleted() {
        let paths = live_files("swapped", WRITTEN);
        let target = paths.global_config();
        let newcomer = target.with_file_name("newcomer.json");
        fs::write(&newcomer, OTHER).expect("the newcomer can be written");
        let mut renamed = None;

        let gone = remove_if_holding_with(target, WRITTEN, || {
            renamed = Some(fs::rename(&newcomer, target));
        })
        .expect("the removal runs");

        #[cfg(windows)]
        {
            assert!(gone, "the removal reports the file it compared gone");
            assert_ne!(
                fs::read(target).ok().as_deref(),
                Some(OTHER),
                "the target does not hold the newcomer's bytes"
            );
            assert_eq!(
                fs::read(&newcomer).ok().as_deref(),
                Some(OTHER),
                "the newcomer was not deleted in the compared file's place (rename: {renamed:?})"
            );
        }
        #[cfg(unix)]
        {
            assert!(
                renamed.as_ref().is_some_and(Result::is_ok),
                "the newcomer was renamed onto the target: {renamed:?}"
            );
            assert!(
                !gone,
                "the target holds the newcomer, so the removal reports it kept"
            );
            assert_eq!(
                fs::read(target).ok().as_deref(),
                Some(OTHER),
                "the newcomer renamed onto the target was not deleted"
            );
            let prefix = format!(
                "{}.rt-remove-",
                target
                    .file_name()
                    .and_then(|name| name.to_str())
                    .expect("the target has a name")
            );
            let left = fs::read_dir(target.parent().expect("the target has a folder"))
                .expect("the target's folder lists")
                .filter(|entry| {
                    entry
                        .as_ref()
                        .is_ok_and(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
                })
                .count();
            assert_eq!(left, 0, "nothing is left set aside");
        }
    }

    #[cfg(windows)]
    #[test]
    #[expect(
        clippy::print_stdout,
        reason = "a test that cannot make its symlink says why it was skipped"
    )]
    fn a_symlink_at_the_target_is_not_followed() {
        let paths = live_files("symlink", WRITTEN);
        let file = paths.global_config();
        let link = file.with_file_name("link.json");
        let made = std::os::windows::fs::symlink_file(file, &link);
        if made.as_ref().err().and_then(std::io::Error::raw_os_error) == Some(1314) {
            println!("skipped: creating a symlink needs Developer Mode or elevation");
            return;
        }
        made.expect("the symlink can be made");

        let gone = remove_if_holding_with(&link, WRITTEN, || ()).expect("the removal runs");

        assert!(
            !gone,
            "a link does not hold the written bytes, so the removal reports it kept"
        );
        assert_eq!(
            fs::read(file).ok().as_deref(),
            Some(WRITTEN),
            "the link's target is neither deleted nor changed"
        );
    }
}

/// The switch's compared credential write, under Claude Code's credential locks, which this platform alone takes.
#[cfg(all(test, accounts_locks))]
#[expect(
    clippy::expect_used,
    reason = "a test that cannot set up its files or take its locks must fail loudly"
)]
mod rewrite_tests {
    use std::fs;

    use super::removal_tests::live_files;
    use super::{ClaudeFileError, Expected, rewrite_live_credentials};
    use crate::locks::ClaudeCodeLocks;
    use crate::secret::CredentialBlob;

    const HELD: &[u8] = br#"{"claudeAiOauth": {"accessToken": "fake-access-held", "refreshToken": "fake-refresh-held"}}"#;
    const OTHER: &[u8] = br#"{"claudeAiOauth": {"accessToken": "fake-access-other", "refreshToken": "fake-refresh-other"}}"#;
    const NEW: &str = r#"{"claudeAiOauth": {"accessToken": "fake-access-new", "refreshToken": "fake-refresh-new"}}"#;

    /// `rewrite_live_credentials` of [`NEW`] over a live credential file holding [`HELD`], expecting `expected`, and
    /// what the file holds afterwards.
    fn rewrite(name: &str, expected: Expected<'_>) -> (Result<(), ClaudeFileError>, Vec<u8>) {
        let paths = live_files(name, HELD);
        let locks = ClaudeCodeLocks::credentials(&paths).expect("the credential locks are free");
        let result = rewrite_live_credentials(
            &paths,
            &locks,
            expected,
            &CredentialBlob::new(NEW.to_owned()),
        );
        (
            result,
            fs::read(paths.credentials_file()).expect("the live credential file reads"),
        )
    }

    #[test]
    fn a_file_expected_absent_that_exists_is_left_unchanged() {
        let (result, held) = rewrite("rewrite-absent", Expected::Absent);

        assert!(
            matches!(result, Err(ClaudeFileError::Changed)),
            "an existing file is not the absent one expected"
        );
        assert_eq!(held, HELD, "the file is byte for byte unchanged");
    }

    #[test]
    fn a_file_holding_other_bytes_than_expected_is_left_unchanged() {
        let (result, held) = rewrite("rewrite-other", Expected::Bytes(OTHER));

        assert!(
            matches!(result, Err(ClaudeFileError::Changed)),
            "a file holding other bytes is refused"
        );
        assert_eq!(held, HELD, "the file is byte for byte unchanged");
    }

    #[test]
    fn a_file_holding_the_expected_bytes_is_rewritten() {
        let (result, held) = rewrite("rewrite-expected", Expected::Bytes(HELD));

        assert!(
            result.is_ok(),
            "a file holding the expected bytes is rewritten"
        );
        assert_eq!(held, NEW.as_bytes(), "the file holds the new credential");
    }
}
