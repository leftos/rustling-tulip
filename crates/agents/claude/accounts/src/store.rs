//! The account store: `accounts.json`, `configs/` and `credentials/`.
//!
//! A missing file is absent, a present file that does not parse is an error and never empty, and transient Windows
//! sharing errors (an antivirus or the indexer opening a fresh file) are retried. Credential files are sealed to this
//! Windows user and bound to their account, and are read and written through [`crate::guard`]; `configs/` holds only
//! each account's `oauthAccount`. Writes go to a temp file in the same folder that the guard creates and replace the
//! target, take the account lock as a parameter, and are refused while sealing fails in this process (the canary) or
//! `accounts.json` carries a `schemaVersion` this crate does not know.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::claude_files::identity_in;
use crate::guard::{self, GuardError, GuardErrorKind};
use crate::locks::{AccountLock, AccountLockGuard, ConsumeLock, LockError};
use crate::paths::{AccountsRoot, ClaudePaths, Email};
use crate::refresh::{Scrubbed, scrub};
use crate::seal::{self, AssociatedData, SealErrorKind, SealPurpose, SealedBytes};
use crate::secret::CredentialBlob;
use crate::usage::Identity;

/// The `accounts.json` schema this crate reads and writes. A file without `schemaVersion` is this version; a file of
/// any other version is read but never written.
const SCHEMA_VERSION: u64 = 1;
/// Windows errors that usually mean another process has the file open for a moment: access denied, sharing
/// violation, lock violation.
const TRANSIENT_OS_ERRORS: [i32; 3] = [5, 32, 33];
/// How many times a read or a replace is tried past a transient error.
const TRANSIENT_ATTEMPTS: u32 = 10;
/// The first wait after a transient error; each later wait doubles, up to [`TRANSIENT_MAX_DELAY`].
const TRANSIENT_FIRST_DELAY: Duration = Duration::from_millis(2);
const TRANSIENT_MAX_DELAY: Duration = Duration::from_millis(250);
/// How many more times a read is tried, at once, after the file grew between its length and its read.
const GROWTH_RETRIES: u32 = 3;
/// The largest file a read takes: a longer one is refused before its buffer is allocated.
const MAX_READ: u64 = 64 << 20;

/// The most characters an account's alias may have, counted after trimming.
pub const MAX_ALIAS_CHARS: usize = 64;

/// Numbers this process's temp files, so two threads writing one target never share a temp file.
static TEMP_SERIAL: AtomicU64 = AtomicU64::new(0);
/// How many temp names a write tries when each one it takes already exists.
const TEMP_NAME_ATTEMPTS: u32 = 16;

/// What went wrong in the store. The text names a file kind, never a path, a file's contents or a response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreErrorKind {
    /// The account store's root is a file, not a folder.
    RootNotDirectory,
    /// The account store's root is unsafe to use: it is a symbolic link, a junction or another reparse point, is owned
    /// by another user, can be written by another user, or is not a folder this user can read; or the folder it lives
    /// in is a link or reparse point or is not owner-only. Another store reached that way is refused rather than
    /// written.
    RootUnsafe,
    /// The account store's root could not be looked at.
    RootUnreadable,
    /// `accounts.json` does not exist, so no account does.
    AccountsMissing,
    /// `accounts.json` exists but could not be read.
    AccountsUnreadable,
    /// `accounts.json` exists but is not an account list this crate can read.
    AccountsCorrupt,
    /// `accounts.json` could not be written.
    AccountsWrite,
    /// `accounts.json` carries a `schemaVersion` this crate does not know: it is read, and every write to the store is
    /// refused.
    UnknownSchema,
    /// A write would change an existing account number's email or organization UUID. An account is re-identified only
    /// by removing it and adding it again under a new number, since its files are named by number alone.
    AccountReidentified,
    /// An account's email is not safe in a file name: empty, or holding a path separator, a character Windows reserves
    /// or a control character.
    UnsafeEmail,
    /// An account's config file exists but could not be read.
    ConfigUnreadable,
    /// An account's config file exists but is not a JSON object.
    ConfigCorrupt,
    /// An account's config file could not be written; the old file is unchanged and the temp file removed.
    ConfigWrite,
    /// As [`StoreErrorKind::ConfigWrite`], but the temp file, which holds the config's bytes, could not be removed.
    ConfigWriteLeftTemp,
    /// An account's credential file exists but could not be read, or is not one this crate may read: another user's,
    /// another link's, or writable by someone else.
    CredentialsUnreadable,
    /// An account's credential file is not a file the account store sealed: an older layout's, or a file this store did not
    /// write.
    /// It is reported and never overwritten.
    Foreign,
    /// An account's credential file is sealed but does not open: sealed for another Windows user or PC, for another
    /// account, or damaged.
    CredentialsUnopenable,
    /// An account's credential file is older than the store last wrote for that account, so a rolled-back file is
    /// refused rather than served.
    CredentialsRolledBack,
    /// An account's credential file could not be written.
    CredentialsWrite,
    /// Sealing does not work in this process (the canary failed), so the store takes no write.
    StoreReadOnly,
    /// No account with this number is in `accounts.json`.
    UnknownAccount,
    /// The account lock passed in belongs to another account store.
    WrongLock,
    /// No account number is left to give: the next one would be `u32::MAX`, which no account may hold.
    NumbersExhausted,
    /// The account number asked for is taken: below `nextNumber`, or already recorded.
    NumberTaken,
    /// An account with this email and organization is already stored, an API-key one included, or the same one is
    /// added twice in one call.
    AccountExists,
    /// A credential file no account records sits at the account number asked for and does not open as the new
    /// account's: it is kept, and the number is not taken.
    LeftoverUnopenable,
    /// A new rotation order does not name each account in `accounts.json`'s `order` exactly once.
    OrderMismatch,
    /// The account asked to be removed is the active one: switch to another account first.
    ActiveAccount,
    /// An alias is longer than [`MAX_ALIAS_CHARS`] characters once trimmed, or holds a control, bidi-control or other
    /// invisible character (a line or paragraph separator, a zero-width space, a byte-order mark).
    InvalidAlias,
}

impl fmt::Display for StoreErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::RootNotDirectory => "the account store is a file, not a folder",
            Self::RootUnsafe => concat!(
                "the account store's root is a link or reparse point, is owned by another user, can be written by ",
                "another user, or is not a folder this user can read, or the folder it lives in is a link or reparse point ",
                "or is not owner-only"
            ),
            Self::RootUnreadable => "the account store could not be read",
            Self::AccountsMissing => "no accounts are managed yet: accounts.json does not exist",
            Self::AccountsUnreadable => "accounts.json could not be read",
            Self::AccountsCorrupt => {
                "accounts.json exists but could not be parsed; repair or move it, then retry (it is never overwritten unread)"
            }
            Self::AccountsWrite => "accounts.json could not be written",
            Self::UnknownSchema => concat!(
                "accounts.json was written by a later version (its schemaVersion is not one this version knows); ",
                "writes are refused until claude-accounts knows it"
            ),
            Self::AccountReidentified => concat!(
                "a write would change an existing account number's email or organization UUID; ",
                "an account is re-identified only by removing it and adding it again under a new number"
            ),
            Self::UnsafeEmail => {
                "an account's email cannot be part of a file name: it is empty or holds a path separator, a reserved character or a control character"
            }
            Self::ConfigUnreadable => "the account's config file could not be read",
            Self::ConfigCorrupt => "the account's config file exists but is not a JSON object",
            Self::ConfigWrite => "the account's config file could not be written",
            Self::ConfigWriteLeftTemp => {
                "the account's config file could not be written, and its temp file could not be removed"
            }
            Self::CredentialsUnreadable => "the account's stored credentials could not be read",
            Self::Foreign => {
                "the account's credential file is not a file the account store sealed; it is reported and never overwritten"
            }
            Self::CredentialsUnopenable => {
                "the account's credential file could not be opened: sealed for another Windows user or PC, another account, or damaged"
            }
            Self::CredentialsRolledBack => {
                "the account's credential file is older than the store last wrote for it; a rolled-back file is refused"
            }
            Self::CredentialsWrite => "the account's credentials could not be written",
            Self::StoreReadOnly => concat!(
                "sealing does not work in this process, so the account store can be read but not written; ",
                "a restart may fix it"
            ),
            Self::UnknownAccount => "no such account in accounts.json",
            Self::WrongLock => "the account lock passed in belongs to another account store",
            Self::NumbersExhausted => "the account store has no account number left to give",
            Self::NumberTaken => {
                "the account number asked for is taken: it is below the next number or already recorded"
            }
            Self::AccountExists => "an account with this email and organization is already stored",
            Self::LeftoverUnopenable => {
                "a credential file no account records is at this account number and does not open as the new account's; it was kept"
            }
            Self::OrderMismatch => "the order must name each account once",
            Self::ActiveAccount => {
                "the active account cannot be removed; switch to another account first"
            }
            Self::InvalidAlias => {
                "an alias is at most 64 characters once trimmed and may not hold a control, bidi-control or invisible character"
            }
        };
        formatter.write_str(text)
    }
}

/// A store error: its kind, the account it concerns if any, and the I/O error under it if any.
#[derive(Debug, thiserror::Error)]
#[error("{kind}{account}")]
pub struct StoreError {
    kind: StoreErrorKind,
    account: AccountTag,
    #[source]
    source: Option<io::Error>,
}

impl StoreError {
    pub(crate) fn new(kind: StoreErrorKind, account: Option<u32>) -> Self {
        Self {
            kind,
            account: AccountTag(account),
            source: None,
        }
    }

    fn io(kind: StoreErrorKind, account: Option<u32>, source: io::Error) -> Self {
        Self {
            kind,
            account: AccountTag(account),
            source: Some(source),
        }
    }

    /// What went wrong.
    #[must_use]
    pub fn kind(&self) -> StoreErrorKind {
        self.kind
    }

    /// The account the error concerns, if one.
    #[must_use]
    pub fn account(&self) -> Option<u32> {
        self.account.0
    }
}

/// An error's account, written after its text as ` (account N)`.
#[derive(Clone, Copy, Debug)]
struct AccountTag(Option<u32>);

impl fmt::Display for AccountTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(number) => write!(formatter, " (account {number})"),
            None => Ok(()),
        }
    }
}

/// Whether an account signs in with OAuth or is a managed API key (`kind: "api_key"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountKind {
    Oauth,
    ApiKey,
}

/// Whether `record` is a kind of account the switch, the poller and the roster take: OAuth, not a managed API key.
fn switchable_kind(record: &AccountRecord) -> bool {
    record.kind == AccountKind::Oauth
}

/// One account as `accounts.json` records it. A copy: changing it changes nothing on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AccountRecord {
    pub number: u32,
    pub email: Email,
    pub uuid: String,
    /// The organization's UUID, empty when the record has none.
    pub org_uuid: String,
    /// The organization's name, empty when the record has none.
    pub org_name: String,
    /// When the account was added, as written.
    pub added: String,
    pub alias: Option<String>,
    /// Held out of automatic switching.
    pub disabled: bool,
    /// The owner allows the auto-switch engine to fall back to this account's prepaid extra-usage credits once every
    /// account is exhausted: `maySpendCredits` is JSON `true` exactly. Independent of [`Self::disabled`].
    pub may_spend_credits: bool,
    pub kind: AccountKind,
    /// The generation of the last credential write this store recorded for the account: a credential file older than
    /// it was rolled back and is refused. Zero before the first write.
    pub generation: u64,
}

/// `accounts.json` as read: the accounts, their order, the active one and the next account number, with every key kept
/// for a write back.
#[derive(Clone, Debug)]
pub struct Sequence {
    document: Map<String, Value>,
    schema_version: u64,
    next_number: u32,
    active: Option<u32>,
    numbers: Vec<u32>,
    accounts: BTreeMap<u32, AccountRecord>,
}

impl Sequence {
    /// The number the next account added gets: past every account number the file holds or has held, so a removed
    /// account's number is never given to another.
    #[must_use]
    pub fn next_number(&self) -> u32 {
        self.next_number
    }

    /// The active account's number, if one is active.
    #[must_use]
    pub fn active(&self) -> Option<u32> {
        self.active
    }

    /// The account numbers in rotation order.
    #[must_use]
    pub fn numbers(&self) -> &[u32] {
        &self.numbers
    }

    /// Whether the file holds no account at all: no record, whether or not its number is in the rotation order, and an
    /// empty order.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty() && self.numbers.is_empty()
    }

    /// Account `number`'s record, if it exists.
    #[must_use]
    pub fn account(&self, number: u32) -> Option<AccountRecord> {
        self.accounts.get(&number).cloned()
    }

    /// Account `number`'s record when it is one to switch to, poll or show: an OAuth account. An API-key record is kept
    /// and [`Self::account`] still finds it, but switching one is not supported.
    #[must_use]
    pub fn switchable_account(&self, number: u32) -> Option<AccountRecord> {
        self.accounts
            .get(&number)
            .filter(|record| switchable_kind(record))
            .cloned()
    }

    /// The account list in `document`: `accounts` keyed by canonical numbers, each with a file-name-safe `email`. A
    /// missing `schemaVersion` is version 1; a missing `nextNumber`, or one at or under an account's number, is one past
    /// the highest account number.
    ///
    /// A file of another `schemaVersion` that the version 1 shape does not read is that version's, not a corrupt one:
    /// [`StoreErrorKind::UnknownSchema`]. A `schemaVersion` that is not a whole number is corrupt.
    fn from_document(document: Map<String, Value>) -> Result<Self, StoreError> {
        let schema_version = match document.get("schemaVersion") {
            None => SCHEMA_VERSION,
            Some(value) => value.as_u64().ok_or_else(accounts_corrupt)?,
        };
        let read = Self::from_shape(document, schema_version);
        if schema_version == SCHEMA_VERSION {
            return read;
        }
        read.map_err(|_| StoreError::new(StoreErrorKind::UnknownSchema, None))
    }

    /// The account list in `document`, read in the version 1 shape.
    fn from_shape(document: Map<String, Value>, schema_version: u64) -> Result<Self, StoreError> {
        let active = match document.get("active") {
            None | Some(Value::Null) => None,
            Some(value) => Some(as_u32(value).ok_or_else(accounts_corrupt)?),
        };
        let stored_next = match document.get("nextNumber") {
            None => 0,
            Some(value) => as_u32(value).ok_or_else(accounts_corrupt)?,
        };
        let numbers = rotation(document.get("order")).ok_or_else(accounts_corrupt)?;
        let accounts = account_records(document.get("accounts"))?;
        // An account numbered u32::MAX leaves no number for the next account.
        let past_highest = accounts
            .keys()
            .chain(&numbers)
            .copied()
            .max()
            .map_or(Some(1), |highest| highest.checked_add(1))
            .ok_or_else(accounts_corrupt)?;
        Ok(Self {
            document,
            schema_version,
            next_number: stored_next.max(past_highest),
            active,
            numbers,
            accounts,
        })
    }

    /// The document to write: as read, with `schemaVersion` set and `nextNumber` set to `next_number`.
    fn to_write(&self, next_number: u32) -> Map<String, Value> {
        let mut document = self.document.clone();
        document.insert("schemaVersion".to_owned(), Value::from(SCHEMA_VERSION));
        document.insert("nextNumber".to_owned(), Value::from(next_number));
        document
    }

    /// Sets or removes account `number`'s `disabled` flag, keeping every other key in place.
    fn set_disabled_flag(&mut self, number: u32, disabled: bool) {
        self.set_record_flag(number, "disabled", disabled);
        if let Some(account) = self.accounts.get_mut(&number) {
            account.disabled = disabled;
        }
    }

    /// Sets or removes account `number`'s `maySpendCredits` flag, keeping every other key in place.
    fn set_may_spend_credits_flag(&mut self, number: u32, on: bool) {
        self.set_record_flag(number, "maySpendCredits", on);
        if let Some(account) = self.accounts.get_mut(&number) {
            account.may_spend_credits = on;
        }
    }

    /// Stores `key: true` in account `number`'s record when `on`, else removes the key, keeping every other key.
    fn set_record_flag(&mut self, number: u32, key: &str, on: bool) {
        let record = self
            .document
            .get_mut("accounts")
            .and_then(Value::as_object_mut)
            .and_then(|records| records.get_mut(&number.to_string()))
            .and_then(Value::as_object_mut);
        if let Some(record) = record {
            if on {
                record.insert(key.to_owned(), Value::Bool(true));
            } else {
                record.shift_remove(key);
            }
        }
    }

    /// Sets the rotation order to `order`, `order` in the document and [`Self::numbers`] alike, keeping every other key
    /// in place. It does not check `order` against the accounts; [`Store::set_order`] does before it writes.
    pub fn set_order(&mut self, order: &[u32]) {
        self.numbers = order.to_vec();
        self.document
            .insert("order".to_owned(), Value::from(self.numbers.clone()));
    }

    /// Drops account `number`: its record under `accounts` and its number from `order`, keeping every other key in
    /// place. `nextNumber` is left as it is, so the number is never given to another account.
    fn remove_record(&mut self, number: u32) {
        if let Some(records) = self
            .document
            .get_mut("accounts")
            .and_then(Value::as_object_mut)
        {
            records.shift_remove(&number.to_string());
        }
        self.accounts.remove(&number);
        if self.numbers.contains(&number) {
            self.numbers.retain(|kept| *kept != number);
            self.document
                .insert("order".to_owned(), Value::from(self.numbers.clone()));
        }
    }

    /// Stores `alias` as account `number`'s `alias`, in its place when the key is there, or removes the key when
    /// `alias` is `None`, keeping every other key in place.
    fn set_alias_value(&mut self, number: u32, alias: Option<&str>) {
        let record = self
            .document
            .get_mut("accounts")
            .and_then(Value::as_object_mut)
            .and_then(|records| records.get_mut(&number.to_string()))
            .and_then(Value::as_object_mut);
        if let Some(record) = record {
            match alias {
                Some(alias) => {
                    record.insert("alias".to_owned(), Value::from(alias));
                }
                None => {
                    record.shift_remove("alias");
                }
            }
        }
        if let Some(account) = self.accounts.get_mut(&number) {
            account.alias = alias.map(str::to_owned);
        }
    }

    /// Makes account `number` the active one (`active`), keeping every other key in place.
    fn set_active_number(&mut self, number: u32) {
        self.document
            .insert("active".to_owned(), Value::from(number));
        self.active = Some(number);
    }

    /// Every switchable account's record ([`Self::switchable_account`]), in account-number order.
    pub(crate) fn records(&self) -> impl Iterator<Item = &AccountRecord> {
        self.accounts
            .values()
            .filter(|record| switchable_kind(record))
    }

    /// Every account's record, API-key ones included, in account-number order.
    pub(crate) fn every_record(&self) -> impl Iterator<Item = &AccountRecord> {
        self.accounts.values()
    }

    /// Sets account `number`'s credential generation, keeping every other key in place. It only ever rises.
    fn set_generation(&mut self, number: u32, generation: u64) {
        if let Some(record) = self
            .document
            .get_mut("accounts")
            .and_then(Value::as_object_mut)
            .and_then(|records| records.get_mut(&number.to_string()))
            .and_then(Value::as_object_mut)
        {
            record.insert("generation".to_owned(), Value::from(generation));
        }
        if let Some(account) = self.accounts.get_mut(&number) {
            account.generation = generation;
        }
    }

    /// Adds `record` as a new account: its entry under `accounts` and its number at the end of `order`.
    fn add_record(&mut self, record: &AccountRecord) {
        let mut entry = Map::new();
        entry.insert("email".to_owned(), Value::from(record.email.as_str()));
        entry.insert("uuid".to_owned(), Value::from(record.uuid.as_str()));
        entry.insert("orgUuid".to_owned(), Value::from(record.org_uuid.as_str()));
        entry.insert("orgName".to_owned(), Value::from(record.org_name.as_str()));
        entry.insert("added".to_owned(), Value::from(record.added.as_str()));
        if let Some(alias) = &record.alias {
            entry.insert("alias".to_owned(), Value::from(alias.as_str()));
        }
        if record.disabled {
            entry.insert("disabled".to_owned(), Value::Bool(true));
        }
        if record.kind == AccountKind::ApiKey {
            entry.insert("kind".to_owned(), Value::from("api_key"));
        }
        entry.insert("generation".to_owned(), Value::from(record.generation));
        if let Value::Object(records) = self
            .document
            .entry("accounts".to_owned())
            .or_insert_with(|| Value::Object(Map::new()))
        {
            records.insert(record.number.to_string(), Value::Object(entry));
        }
        self.accounts.insert(record.number, record.clone());
        self.numbers.push(record.number);
        self.document
            .insert("order".to_owned(), Value::from(self.numbers.clone()));
    }

    /// Sets account `number`'s `uuid`, keeping every other key in place.
    fn set_uuid(&mut self, number: u32, uuid: &str) {
        let record = self
            .document
            .get_mut("accounts")
            .and_then(Value::as_object_mut)
            .and_then(|records| records.get_mut(&number.to_string()))
            .and_then(Value::as_object_mut);
        if let Some(record) = record {
            record.insert("uuid".to_owned(), Value::String(uuid.to_owned()));
        }
        if let Some(account) = self.accounts.get_mut(&number) {
            uuid.clone_into(&mut account.uuid);
        }
    }
}

fn accounts_corrupt() -> StoreError {
    StoreError::new(StoreErrorKind::AccountsCorrupt, None)
}

fn as_u32(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|number| u32::try_from(number).ok())
}

/// The `order` array of account numbers; missing is empty, anything but an array of numbers is `None`.
fn rotation(value: Option<&Value>) -> Option<Vec<u32>> {
    let mut numbers = Vec::new();
    match value {
        None => {}
        Some(Value::Array(items)) => {
            for item in items {
                numbers.push(as_u32(item)?);
            }
        }
        Some(_) => return None,
    }
    Some(numbers)
}

/// The `accounts` object's records. Each key must be a number written canonically (`"3"`, never `"03"`), so the key a
/// write looks up is the key that was read.
fn account_records(value: Option<&Value>) -> Result<BTreeMap<u32, AccountRecord>, StoreError> {
    let mut accounts = BTreeMap::new();
    let records = match value {
        None => return Ok(accounts),
        Some(Value::Object(records)) => records,
        Some(_) => return Err(accounts_corrupt()),
    };
    for (key, record) in records {
        let number = key
            .parse::<u32>()
            .ok()
            .filter(|number| number.to_string() == *key)
            .ok_or_else(accounts_corrupt)?;
        let Value::Object(record) = record else {
            return Err(accounts_corrupt());
        };
        accounts.insert(number, account_record(number, record)?);
    }
    Ok(accounts)
}

fn account_record(number: u32, record: &Map<String, Value>) -> Result<AccountRecord, StoreError> {
    let email = record
        .get("email")
        .and_then(Value::as_str)
        .ok_or_else(accounts_corrupt)?;
    let email = Email::parse(email)
        .map_err(|_| StoreError::new(StoreErrorKind::UnsafeEmail, Some(number)))?;
    let kind = match record.get("kind").and_then(Value::as_str) {
        Some("api_key") => AccountKind::ApiKey,
        Some(_) | None => AccountKind::Oauth,
    };
    Ok(AccountRecord {
        number,
        email,
        uuid: string_or_empty(record, "uuid").ok_or_else(accounts_corrupt)?,
        org_uuid: string_or_empty(record, "orgUuid").ok_or_else(accounts_corrupt)?,
        org_name: string_or_empty(record, "orgName").ok_or_else(accounts_corrupt)?,
        added: string_or_empty(record, "added").ok_or_else(accounts_corrupt)?,
        alias: record
            .get("alias")
            .and_then(Value::as_str)
            .map(str::to_owned),
        disabled: record.get("disabled").is_some_and(truthy),
        may_spend_credits: record.get("maySpendCredits") == Some(&Value::Bool(true)),
        kind,
        generation: match record.get("generation") {
            None | Some(Value::Null) => 0,
            Some(value) => value.as_u64().ok_or_else(accounts_corrupt)?,
        },
    })
}

/// The string at `key`, empty when it is missing or null; `None` when it is something else.
fn string_or_empty(record: &Map<String, Value>, key: &str) -> Option<String> {
    match record.get(key) {
        None | Some(Value::Null) => Some(String::new()),
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => None,
    }
}

/// A JSON value's truth as Python's `bool()` reads it, the way the store reads its flags.
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number.abs() > 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// What a credential write did with the generation it replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Retention {
    /// The replaced credentials were kept as `<file>.prev`.
    Kept,
    /// There was nothing to keep: no current file, or the same credentials.
    NothingToKeep,
    /// Writing `.prev` failed with this error. The write went ahead.
    Failed(ErrorKind),
}

/// How auto-switch picks the account to switch to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// The account with the most headroom.
    Best,
    /// The account whose weekly window resets soonest.
    ConsumeFirst,
}

/// The auto-switch engine's settings, with their defaults and ranges; the server reads them from its own settings file
/// and hands them to the engine and the poll.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct AutoSwitchSettings {
    /// The binding window's utilization, in percent, at which to switch; the server allows 50 to 99.9.
    pub threshold: f64,
    /// Seconds between engine ticks; the server allows 15 to 3600.
    pub interval_seconds: f64,
    /// Least seconds between proactive switches; the server allows 0 to 86400.
    pub cooldown_seconds: f64,
    /// Percentage points a target must beat the active account by; the server allows 0 to 50.
    pub hysteresis_pct: f64,
    /// How the engine picks the account to switch to.
    pub strategy: Strategy,
    /// Failed polls in a row before an account counts as unhealthy; the server allows 1 to 100.
    pub unhealthy_ticks: u32,
    /// The model names whose weekly limits also count, matched case-insensitively; `all` counts every scoped window, and
    /// an empty list counts the account-wide windows only. The server refuses an empty name, one with spaces around
    /// it, and one named twice ignoring case.
    pub models: Vec<String>,
}

impl Default for AutoSwitchSettings {
    fn default() -> Self {
        Self {
            threshold: 90.0,
            interval_seconds: 60.0,
            cooldown_seconds: 300.0,
            hysteresis_pct: 10.0,
            strategy: Strategy::Best,
            unhealthy_ticks: 3,
            models: Vec::new(),
        }
    }
}

/// An account's config backup: its `oauthAccount` identity block and nothing else, whose strings are zeroed when it
/// drops. A copy taken through `Deref` (`.clone()`, `.get(..).cloned()`, serialising `&*config`) is not scrubbed: wrap
/// it in `Scrubbed`.
pub struct AccountConfig(Map<String, Value>);

impl Drop for AccountConfig {
    fn drop(&mut self) {
        self.0.values_mut().for_each(scrub);
    }
}

impl std::ops::Deref for AccountConfig {
    type Target = Map<String, Value>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Debug for AccountConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountConfig")
            .finish_non_exhaustive()
    }
}

impl AccountConfig {
    /// The config as a [`Scrubbed`] tree, which zeroes its strings when it drops. The taken-from map is empty, so this
    /// value's own drop scrubs nothing.
    pub(crate) fn into_scrubbed(mut self) -> Scrubbed {
        Scrubbed(Value::Object(std::mem::take(&mut self.0)))
    }
}

/// Whether sealing works in this process: the gate every write and every pre-spend check runs. [`default_canary`] is
/// the one production uses; a test replaces it to make DPAPI fail without DPAPI.
pub type SealCanary = fn() -> bool;

/// Whether a seal-and-open round trip works in this process.
#[must_use]
pub fn default_canary() -> bool {
    crate::seal::canary().is_ok()
}

/// A hook the tests run once at a point the engine or a switch reaches ([`Store::on_freshen`], [`Store::on_switch_locks`]).
type TestHook = Box<dyn FnOnce() + Send>;

/// Takes the hook in `slot`, if any, and runs it.
fn run_hook(slot: &Mutex<Option<TestHook>>) {
    let hook = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(hook) = hook {
        hook();
    }
}

/// The account store, read and written in place.
pub struct Store {
    root: AccountsRoot,
    canary: SealCanary,
    fail_read_back: bool,
    /// How long a stash discard waits for each of its locks.
    lock_wait: Duration,
    freshen_hook: Mutex<Option<TestHook>>,
    switch_locks_hook: Mutex<Option<TestHook>>,
}

impl fmt::Debug for Store {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Store")
            .field("root", &self.root)
            .field("canary", &self.canary)
            .field("fail_read_back", &self.fail_read_back)
            .field("lock_wait", &self.lock_wait)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// The store at `root`. A root that does not exist yet is fine: every read of it is absent.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::RootNotDirectory`] when `root` is a file, [`StoreErrorKind::RootUnsafe`] when `root` is a
    /// link or reparse point, is owned by another user, can be written by another user, or is not a folder this user
    /// can read, or the folder it lives in is a link or reparse point or is not owner-only,
    /// [`StoreErrorKind::RootUnreadable`] when it cannot be looked at.
    pub fn open(root: AccountsRoot) -> Result<Self, StoreError> {
        Self::open_with_canary(root, default_canary)
    }

    /// The store at `root`, whose writes and pre-spend checks ask `canary` whether sealing works in this process.
    /// A test seam, not a production switch: a caller that passes a canary saying sealing works lifts the store's
    /// read-only gate.
    ///
    /// # Errors
    ///
    /// As [`Store::open`].
    #[doc(hidden)]
    pub fn open_with_canary(root: AccountsRoot, canary: SealCanary) -> Result<Self, StoreError> {
        check_root(&root)?;
        Ok(Self {
            root,
            canary,
            fail_read_back: false,
            lock_wait: crate::locks::ACCOUNT_LOCK_TIMEOUT,
            freshen_hook: Mutex::new(None),
            switch_locks_hook: Mutex::new(None),
        })
    }

    /// Makes every credential write fail as a read-back that does not match does, after its temp file is written: the
    /// tests' way to pin that a failed write leaves the old file and no temp.
    #[doc(hidden)]
    #[must_use]
    pub fn with_failed_read_back(mut self) -> Self {
        self.fail_read_back = true;
        self
    }

    /// Makes a stash discard ([`crate::discard_held`]) wait up to `wait` for each of its locks (the account's consume
    /// lock, the account lock and the stash's manifest lock) instead of [`crate::ACCOUNT_LOCK_TIMEOUT`]: the tests' way
    /// to see a busy lock answered without waiting the real timeout out, or to wait longer on a loaded machine.
    #[doc(hidden)]
    #[must_use]
    pub fn with_lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
        self
    }

    /// How long a stash discard waits for each of its locks ([`Store::with_lock_wait`]).
    pub(crate) fn lock_wait(&self) -> Duration {
        self.lock_wait
    }

    /// Runs `hook` once, at the start of the next switch-target freshen ([`crate::engine::freshen_target`]), before it
    /// reads anything: the tests' way to race a write against the engine after its ranking. A later call replaces a
    /// hook not yet run.
    #[doc(hidden)]
    pub fn on_freshen(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .freshen_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }

    /// Takes the hook [`Store::on_freshen`] set, if any, and runs it.
    pub(crate) fn run_freshen_hook(&self) {
        run_hook(&self.freshen_hook);
    }

    /// Runs `hook` once, in the next [`crate::switch::switch`] that passes its checks before the locks, before it takes
    /// the first of them: the tests' way to change the world between the early checks and the ones under the locks. A
    /// later call replaces a hook not yet run.
    #[doc(hidden)]
    pub fn on_switch_locks(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .switch_locks_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }

    /// Takes the hook [`Store::on_switch_locks`] set, if any, and runs it.
    pub(crate) fn run_switch_locks_hook(&self) {
        run_hook(&self.switch_locks_hook);
    }

    /// The account store's root.
    #[must_use]
    pub fn root(&self) -> &AccountsRoot {
        &self.root
    }

    /// `accounts.json`, or `None` when it does not exist. A file of a `schemaVersion` this crate does not know is read
    /// when it is in the version 1 shape (writes of it are refused), and is [`StoreErrorKind::UnknownSchema`] when it
    /// is not.
    ///
    /// It holds no secret (account numbers, emails, `uuid`s, `orgUuid`s and flags), so the parsed tree is dropped
    /// without scrubbing.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::AccountsUnreadable`] when the file cannot be read, [`StoreErrorKind::AccountsCorrupt`] when it
    /// is not an account list (a torn file is an error, never an empty list), [`StoreErrorKind::UnsafeEmail`] when an
    /// account's email cannot be part of a file name, [`StoreErrorKind::UnknownSchema`] when a later version's file is
    /// not in the version 1 shape.
    pub fn read_sequence(&self) -> Result<Option<Sequence>, StoreError> {
        read_sequence_in(&self.root)
    }

    /// Account `number`'s config file as an [`AccountConfig`], holding only its `oauthAccount` block, or `None` when
    /// the file does not exist. Any other key a file carries is not read.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::ConfigUnreadable`] when the file cannot be read, [`StoreErrorKind::ConfigCorrupt`] when it is
    /// not a JSON object or holds no `oauthAccount` object.
    pub fn read_account_config(&self, number: u32) -> Result<Option<AccountConfig>, StoreError> {
        let path = self.root.config_path(number);
        let Some(bytes) = self.read_unlinked(&path).map_err(|error| {
            StoreError::io(StoreErrorKind::ConfigUnreadable, Some(number), error)
        })?
        else {
            return Ok(None);
        };
        let mut tree = Scrubbed(
            serde_json::from_slice::<Value>(&bytes)
                .map_err(|_| StoreError::new(StoreErrorKind::ConfigCorrupt, Some(number)))?,
        );
        match &mut tree.0 {
            Value::Object(config) => match config.get_mut("oauthAccount") {
                Some(Value::Object(identity)) => Ok(Some(AccountConfig(std::mem::take(identity)))),
                Some(_) | None => Err(StoreError::new(StoreErrorKind::ConfigCorrupt, Some(number))),
            },
            _ => Err(StoreError::new(StoreErrorKind::ConfigCorrupt, Some(number))),
        }
    }

    /// Replaces account `number`'s config file with `{"oauthAccount": oauth_account}`: the identity alone, through a
    /// temp file this write creates, read back and compared, then renamed onto it. A value that is not a JSON object
    /// is refused rather than written.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::WrongLock`], [`StoreErrorKind::StoreReadOnly`], [`StoreErrorKind::UnknownSchema`],
    /// [`StoreErrorKind::ConfigWrite`] when the identity is not an object, the folder, the temp file or the rename
    /// fails (the old file is unchanged and the temp file removed), or [`StoreErrorKind::ConfigWriteLeftTemp`] when the
    /// temp file could not be removed after that.
    pub fn write_account_config(
        &self,
        lock: &AccountLockGuard,
        number: u32,
        oauth_account: &Value,
    ) -> Result<(), StoreError> {
        self.check_write(lock, Some(number))?;
        let write_error = |error| StoreError::io(StoreErrorKind::ConfigWrite, Some(number), error);
        if !oauth_account.is_object() {
            return Err(write_error(io::Error::new(
                ErrorKind::InvalidInput,
                "the account's oauthAccount must be a JSON object",
            )));
        }
        let target = self.root.config_path(number);
        let bytes =
            serde_json::to_vec_pretty(&serde_json::json!({ "oauthAccount": oauth_account }))
                .map_err(io::Error::other)
                .map_err(write_error)?;
        self.prepare_folder(&target).map_err(write_error)?;
        self.replace_guarded(&target, &bytes, |temp| read_back_equals(temp, &bytes))
            .map_err(|failed| {
                let kind = if failed.temp_left {
                    StoreErrorKind::ConfigWriteLeftTemp
                } else {
                    StoreErrorKind::ConfigWrite
                };
                StoreError::io(kind, Some(number), failed.error)
            })
    }

    /// Account `number`'s credentials, `None` when the file does not exist. The file is read through
    /// [`crate::guard`], its header's generation compared with the roster's floor (a rolled-back file is refused), and
    /// its payload opened under the account's associated data.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::CredentialsUnreadable`] when the file or a folder on the way cannot be read or is not this
    /// user's alone, [`StoreErrorKind::Foreign`] when it is not a sealed file, [`StoreErrorKind::CredentialsUnopenable`]
    /// when it does not open, [`StoreErrorKind::CredentialsRolledBack`] when it is older than the store's last write
    /// for the account.
    pub fn read_credentials(&self, number: u32) -> Result<Option<CredentialBlob>, StoreError> {
        let binding = self.credential_binding(number)?.unwrap_or_default();
        Ok(self
            .read_credential_file(number, &binding)?
            .map(|current| current.blob))
    }

    /// Replaces account `number`'s credentials with a sealed file bound to the account, its organization, the write's
    /// generation and its email. The current file is kept byte for byte as `<file>.prev` when its plaintext differs,
    /// and a current file that is foreign or does not open refuses the write rather than being overwritten.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::WrongLock`], [`StoreErrorKind::StoreReadOnly`], [`StoreErrorKind::UnknownSchema`],
    /// [`StoreErrorKind::Foreign`] or [`StoreErrorKind::CredentialsUnopenable`] when the current file cannot be read,
    /// or [`StoreErrorKind::CredentialsWrite`] when the sealed file cannot be written or read back.
    pub fn write_credentials(
        &self,
        lock: &AccountLockGuard,
        number: u32,
        blob: &CredentialBlob,
    ) -> Result<Retention, StoreError> {
        self.check_write(lock, Some(number))?;
        let write_error =
            |error| StoreError::io(StoreErrorKind::CredentialsWrite, Some(number), error);
        self.prepare_folder(&self.root.credential_path(number))
            .map_err(write_error)?;
        let Some(binding) = self.credential_binding(number)? else {
            return Err(StoreError::new(
                StoreErrorKind::UnknownAccount,
                Some(number),
            ));
        };
        let current = self.read_credential_file(number, &binding)?;
        let generation = current
            .as_ref()
            .map_or(binding.generation, |current| {
                current.generation.max(binding.generation)
            })
            .saturating_add(1);
        let ad = binding.associated_data(number, generation);
        let sealed = seal_credentials(number, &ad, blob)?;
        let retention = match &current {
            Some(current) if current.blob.expose() != blob.expose() => {
                self.keep_previous(number, current)
            }
            Some(_) | None => Retention::NothingToKeep,
        };
        self.write_sealed(
            &self.root.credential_path(number),
            &sealed,
            &ad,
            blob,
            Some(number),
        )?;
        self.raise_generation(number, generation);
        Ok(retention)
    }

    /// The parts of account `number`'s associated data the roster knows: its organization UUID, its email and the
    /// generation of its last recorded write. `None` when no record names the number: [`Store::write_credentials`]
    /// seals no file for a number without a record, since it would stop opening once a record's identity arrived;
    /// [`Store::add_accounts`] seals a new account's file under the binding of the record it is about to write.
    fn credential_binding(&self, number: u32) -> Result<Option<CredentialBinding>, StoreError> {
        let sequence = self
            .read_sequence()
            .map_err(|_| StoreError::new(StoreErrorKind::CredentialsUnreadable, Some(number)))?;
        Ok(sequence
            .and_then(|sequence| sequence.account(number))
            .map(|record| binding_of(&record)))
    }

    /// Account `number`'s sealed file and what it holds, `None` when the file does not exist.
    fn read_credential_file(
        &self,
        number: u32,
        binding: &CredentialBinding,
    ) -> Result<Option<CurrentCredential>, StoreError> {
        let path = self.root.credential_path(number);
        let sealed = match guard::read_file(self.root.root(), &path) {
            Ok(sealed) => sealed,
            Err(error) if error.kind() == GuardErrorKind::Missing => return Ok(None),
            Err(error) => {
                return Err(refused_by_guard(
                    StoreErrorKind::CredentialsUnreadable,
                    Some(number),
                    error,
                ));
            }
        };
        let generation = sealed
            .generation()
            .ok_or_else(|| StoreError::new(StoreErrorKind::Foreign, Some(number)))?;
        if generation < binding.generation {
            return Err(StoreError::new(
                StoreErrorKind::CredentialsRolledBack,
                Some(number),
            ));
        }
        let blob = seal::open(
            SealPurpose::Credentials,
            &binding.associated_data(number, generation),
            &sealed,
        )
        .map_err(|error| match error.kind() {
            SealErrorKind::Foreign => StoreError::new(StoreErrorKind::Foreign, Some(number)),
            SealErrorKind::Unopenable => {
                StoreError::new(StoreErrorKind::CredentialsUnopenable, Some(number))
            }
            _ => StoreError::new(StoreErrorKind::CredentialsUnreadable, Some(number)),
        })?;
        Ok(Some(CurrentCredential {
            sealed,
            blob,
            generation,
        }))
    }

    /// Keeps the ciphertext `current` holds as `<file>.prev`, byte for byte, never re-sealed.
    fn keep_previous(&self, number: u32, current: &CurrentCredential) -> Retention {
        let target = self.root.credential_previous_path(number);
        match self.replace_guarded(&target, current.sealed.as_bytes(), |_| Ok(())) {
            Ok(()) => Retention::Kept,
            Err(failed) => Retention::Failed(failed.error.kind()),
        }
    }

    /// Writes a sealed credential file through a guard-made temp file, reads the ciphertext and the opened plaintext
    /// back, then renames it onto `target`.
    fn write_sealed(
        &self,
        target: &Path,
        sealed: &SealedBytes,
        ad: &AssociatedData,
        expected: &CredentialBlob,
        account: Option<u32>,
    ) -> Result<(), StoreError> {
        let write_error = |error| StoreError::io(StoreErrorKind::CredentialsWrite, account, error);
        self.prepare_folder(target).map_err(write_error)?;
        let bytes = sealed.as_bytes();
        self.replace_guarded(target, bytes, |temp| {
            if self.fail_read_back {
                // A write that landed a byte wrong: the read-back below must notice it and leave the old file alone.
                let mut landed = read_optional(temp)?.unwrap_or_default();
                if let Some(last) = landed.last_mut() {
                    *last ^= 0x01;
                }
                fs::write(temp, &landed)?;
            }
            let written = guard::read_file(self.root.root(), temp).map_err(guard_io)?;
            if written.as_bytes() != bytes {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "the written file did not read back as written",
                ));
            }
            let opened = seal::open(SealPurpose::Credentials, ad, &written)
                .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string()))?;
            if opened.expose() != expected.expose() {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "the written file opened to other credentials",
                ));
            }
            Ok(())
        })
        .map_err(|failed| StoreError::io(StoreErrorKind::CredentialsWrite, account, failed.error))
    }

    /// Records `generation` as account `number`'s floor, so a file older than it is refused later. Best effort: a
    /// failure leaves the floor where it was, which only lets a rollback to a generation the floor has not passed go.
    fn raise_generation(&self, number: u32, generation: u64) {
        let Ok(Some(mut sequence)) = self.read_sequence() else {
            return;
        };
        if sequence
            .account(number)
            .is_some_and(|record| record.generation < generation)
        {
            sequence.set_generation(number, generation);
            if let Err(error) = self.write_sequence_file(&sequence) {
                tracing::warn!(account = number, %error, "the credential generation floor could not be raised");
            }
        }
    }

    /// Writes `sequence` back to `accounts.json`, every key it was read with kept in its place, with `schemaVersion`
    /// and `nextNumber` set.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::WrongLock`], [`StoreErrorKind::StoreReadOnly`], [`StoreErrorKind::UnknownSchema`] (also
    /// when `sequence` was read from a file of an unknown version), [`StoreErrorKind::AccountReidentified`] when the
    /// write would change an existing account number's email or organization UUID, or
    /// [`StoreErrorKind::AccountsWrite`] when the file cannot be written.
    pub fn write_sequence(
        &self,
        lock: &AccountLockGuard,
        sequence: &Sequence,
    ) -> Result<(), StoreError> {
        self.check_write(lock, None)?;
        self.write_sequence_file(sequence)
    }

    /// Holds account `number` out of automatic switching (`disabled` true) or returns it (`disabled` false). Returns
    /// `false`, leaving the file untouched, when the account is already in that state.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::WrongLock`], [`StoreErrorKind::StoreReadOnly`], [`StoreErrorKind::UnknownSchema`],
    /// [`StoreErrorKind::AccountsMissing`], [`StoreErrorKind::UnknownAccount`], or the errors of
    /// [`Store::read_sequence`] and [`Store::write_sequence`].
    pub fn set_disabled(
        &self,
        lock: &AccountLockGuard,
        number: u32,
        disabled: bool,
    ) -> Result<bool, StoreError> {
        self.check_write(lock, Some(number))?;
        let Some(mut sequence) = self.read_sequence()? else {
            return Err(StoreError::new(
                StoreErrorKind::AccountsMissing,
                Some(number),
            ));
        };
        let Some(account) = sequence.accounts.get(&number) else {
            return Err(StoreError::new(
                StoreErrorKind::UnknownAccount,
                Some(number),
            ));
        };
        if account.disabled == disabled {
            return Ok(false);
        }
        sequence.set_disabled_flag(number, disabled);
        self.write_sequence_file(&sequence)?;
        Ok(true)
    }

    /// Sets the rotation order (`order` in `accounts.json`) to `order`, every other key kept. Returns `false`, leaving
    /// the file untouched, when the order is already `order`.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::WrongLock`], [`StoreErrorKind::StoreReadOnly`], [`StoreErrorKind::UnknownSchema`],
    /// [`StoreErrorKind::AccountsMissing`], [`StoreErrorKind::OrderMismatch`] when `order` is not the current order's
    /// numbers each exactly once, or the errors of [`Store::read_sequence`] and [`Store::write_sequence`].
    pub fn set_order(&self, lock: &AccountLockGuard, order: &[u32]) -> Result<bool, StoreError> {
        self.check_write(lock, None)?;
        let Some(mut sequence) = self.read_sequence()? else {
            return Err(StoreError::new(StoreErrorKind::AccountsMissing, None));
        };
        if sequence.numbers() == order {
            return Ok(false);
        }
        let mut wanted = order.to_vec();
        let mut current = sequence.numbers().to_vec();
        wanted.sort_unstable();
        current.sort_unstable();
        if wanted != current || wanted.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(StoreError::new(StoreErrorKind::OrderMismatch, None));
        }
        sequence.set_order(order);
        self.write_sequence_file(&sequence)?;
        Ok(true)
    }

    /// Allows (`on` true) or stops (`on` false) the auto-switch engine falling back to account `number`'s prepaid
    /// extra-usage credits: stores `maySpendCredits: true` or removes the key, leaving `disabled` and every other key as
    /// they are. Returns `false`, leaving the file untouched, when the account is already in that state.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::WrongLock`], [`StoreErrorKind::StoreReadOnly`], [`StoreErrorKind::UnknownSchema`],
    /// [`StoreErrorKind::AccountsMissing`], [`StoreErrorKind::UnknownAccount`], or the errors of
    /// [`Store::read_sequence`] and [`Store::write_sequence`].
    pub fn set_may_spend_credits(
        &self,
        lock: &AccountLockGuard,
        number: u32,
        on: bool,
    ) -> Result<bool, StoreError> {
        self.check_write(lock, Some(number))?;
        let Some(mut sequence) = self.read_sequence()? else {
            return Err(StoreError::new(
                StoreErrorKind::AccountsMissing,
                Some(number),
            ));
        };
        let Some(account) = sequence.accounts.get(&number) else {
            return Err(StoreError::new(
                StoreErrorKind::UnknownAccount,
                Some(number),
            ));
        };
        if account.may_spend_credits == on {
            return Ok(false);
        }
        sequence.set_may_spend_credits_flag(number, on);
        self.write_sequence_file(&sequence)?;
        Ok(true)
    }

    /// Makes account `number` the active account. Returns `false`, leaving the file untouched, when it already is.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::WrongLock`], [`StoreErrorKind::StoreReadOnly`], [`StoreErrorKind::UnknownSchema`],
    /// [`StoreErrorKind::AccountsMissing`], [`StoreErrorKind::UnknownAccount`], or the errors of
    /// [`Store::read_sequence`] and [`Store::write_sequence`].
    pub fn set_active(&self, lock: &AccountLockGuard, number: u32) -> Result<bool, StoreError> {
        self.check_write(lock, Some(number))?;
        let Some(mut sequence) = self.read_sequence()? else {
            return Err(StoreError::new(
                StoreErrorKind::AccountsMissing,
                Some(number),
            ));
        };
        if !sequence.accounts.contains_key(&number) {
            return Err(StoreError::new(
                StoreErrorKind::UnknownAccount,
                Some(number),
            ));
        }
        if sequence.active == Some(number) {
            return Ok(false);
        }
        sequence.set_active_number(number);
        self.write_sequence_file(&sequence)?;
        Ok(true)
    }

    /// Removes account `number` from the store. It takes the account's consume lock, then the account lock, in the
    /// order a switch takes them, and under both drops the account's refresh stash entries and their manifest rows
    /// under the stash's manifest lock, so no pass can adopt and spend one for the number; then drops the record and
    /// its `order` entry, keeping `nextNumber` so the number is never given again, writes `accounts.json`, and deletes
    /// `credentials/<n>.cred`, `credentials/<n>.cred.prev` and `configs/<n>.json`, a file already absent counting as
    /// deleted. A crash between the write and the deletions leaves files no record names, which nothing reads. The
    /// consume lock file, `credentials/.consume-<n>.lock`, stays: a waiter's open handle must keep naming the one lock,
    /// and the number is never given again. Returns `true` when it removed the account.
    ///
    /// The account `accounts.json` names as active is refused, and so is the one Claude Code is logged in as now
    /// (`claude`'s `~/.claude.json` `oauthAccount`, by email and organization). The checks run once before the locks
    /// too, so a refused removal creates no lock file.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::StoreReadOnly`], [`StoreErrorKind::UnknownSchema`], [`StoreErrorKind::AccountsMissing`],
    /// [`StoreErrorKind::UnknownAccount`], [`StoreErrorKind::ActiveAccount`] for the active account or the live login,
    /// each with nothing changed; [`StoreErrorKind::AccountsWrite`] when a lock cannot be taken or the file cannot be
    /// written; [`StoreErrorKind::CredentialsWrite`] when the account's stash entries cannot all be dropped (the
    /// record stays then), or when a credential file that is there cannot be deleted; [`StoreErrorKind::ConfigWrite`]
    /// when the config file cannot be deleted (the record is already gone then); or the errors of
    /// [`Store::read_sequence`].
    pub fn remove_account(&self, claude: &ClaudePaths, number: u32) -> Result<bool, StoreError> {
        if let Some(kind) = self.write_refusal() {
            return Err(StoreError::new(kind, Some(number)));
        }
        self.removable(claude, number)?;
        let lock_failed = |error: LockError| {
            StoreError::io(
                StoreErrorKind::AccountsWrite,
                Some(number),
                io::Error::other(error),
            )
        };
        let consume =
            ConsumeLock::acquire_within(&self.root, number, self.lock_wait).map_err(lock_failed)?;
        let lock = AccountLock::acquire_within(&self.root, self.lock_wait).map_err(lock_failed)?;
        self.check_write(&lock, Some(number))?;
        let mut sequence = self.removable(claude, number)?;
        crate::refresh::stash::drop_account_entries(
            &self.root,
            number,
            self.lock_wait,
            jiff::Timestamp::now(),
        )
        .map_err(|error| StoreError::io(StoreErrorKind::CredentialsWrite, Some(number), error))?;
        sequence.remove_record(number);
        self.write_sequence_file(&sequence)?;
        self.remove_file_if_present(
            &self.root.credential_path(number),
            StoreErrorKind::CredentialsWrite,
            number,
        )?;
        self.remove_file_if_present(
            &self.root.credential_previous_path(number),
            StoreErrorKind::CredentialsWrite,
            number,
        )?;
        self.remove_file_if_present(
            &self.root.config_path(number),
            StoreErrorKind::ConfigWrite,
            number,
        )?;
        drop(lock);
        drop(consume);
        Ok(true)
    }

    /// `accounts.json` when account `number` may be removed: it exists, holds the account, and the account is neither
    /// the active one nor the one Claude Code is logged in as now, resolved as a refresh resolves it.
    fn removable(&self, claude: &ClaudePaths, number: u32) -> Result<Sequence, StoreError> {
        let Some(sequence) = self.read_sequence()? else {
            return Err(StoreError::new(
                StoreErrorKind::AccountsMissing,
                Some(number),
            ));
        };
        let Some(account) = sequence.accounts.get(&number) else {
            return Err(StoreError::new(
                StoreErrorKind::UnknownAccount,
                Some(number),
            ));
        };
        let live = crate::poll::live_identity(claude);
        if sequence.active == Some(number) || live.is_some_and(|live| live == Identity::of(account))
        {
            return Err(StoreError::new(StoreErrorKind::ActiveAccount, Some(number)));
        }
        Ok(sequence)
    }

    /// Deletes the store file at `path`, once the folders on the way are checked for links; a file already absent counts
    /// as deleted, and transient sharing errors are retried. Any other failure is `kind`.
    fn remove_file_if_present(
        &self,
        path: &Path,
        kind: StoreErrorKind,
        number: u32,
    ) -> Result<(), StoreError> {
        let failed = |error| StoreError::io(kind, Some(number), error);
        self.root.check_folders(path).map_err(failed)?;
        match retry_transient(|| fs::remove_file(path)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(failed(error)),
        }
    }

    /// Sets account `number`'s alias to `alias`, trimmed, or removes it when `alias` is `None` or blank once trimmed,
    /// every other key kept. Returns `false`, leaving the file untouched, when the alias is already that.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::WrongLock`], [`StoreErrorKind::StoreReadOnly`], [`StoreErrorKind::UnknownSchema`],
    /// [`StoreErrorKind::InvalidAlias`] for an alias over [`MAX_ALIAS_CHARS`] characters once trimmed or holding a
    /// control, bidi-control or other invisible character (as a model name may not), [`StoreErrorKind::AccountsMissing`], [`StoreErrorKind::UnknownAccount`], or the errors of
    /// [`Store::read_sequence`] and [`Store::write_sequence`].
    pub fn set_alias(
        &self,
        lock: &AccountLockGuard,
        number: u32,
        alias: Option<&str>,
    ) -> Result<bool, StoreError> {
        self.check_write(lock, Some(number))?;
        let alias = alias.map(str::trim).filter(|alias| !alias.is_empty());
        if let Some(alias) = alias
            && (alias.chars().count() > MAX_ALIAS_CHARS
                || alias.chars().any(crate::settings::is_control_or_bidi))
        {
            return Err(StoreError::new(StoreErrorKind::InvalidAlias, Some(number)));
        }
        let Some(mut sequence) = self.read_sequence()? else {
            return Err(StoreError::new(
                StoreErrorKind::AccountsMissing,
                Some(number),
            ));
        };
        let Some(account) = sequence.accounts.get(&number) else {
            return Err(StoreError::new(
                StoreErrorKind::UnknownAccount,
                Some(number),
            ));
        };
        if account.alias.as_deref() == alias {
            return Ok(false);
        }
        sequence.set_alias_value(number, alias);
        self.write_sequence_file(&sequence)?;
        Ok(true)
    }

    /// Adds `accounts` to the store, in order, and returns the number each was given. Every check runs before any
    /// write: the numbers ([`AccountNumber`]), an email and organization no stored account (an API-key one included)
    /// or earlier account of the batch has, and an `oauthAccount` that is an object naming the account's email,
    /// organization and uuid. Then each account's config file is written (its `oauthAccount` alone), then its
    /// credential file, sealed under the record about to be written at generation 1, or at one past a leftover's when
    /// the number already holds a file that opens as this account's (kept as `.prev` when it differs), and last
    /// `accounts.json`, once: the records, their numbers appended to `order`, and `nextNumber` past the highest.
    ///
    /// A crash before `accounts.json` is written leaves config and credential files no record names, which nothing
    /// reads; a later add of the same login at that number seals over them, and another login's skips the number or is
    /// refused. The caller sets the active account itself ([`Store::set_active`]).
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::WrongLock`], [`StoreErrorKind::StoreReadOnly`], [`StoreErrorKind::UnknownSchema`] or the errors
    /// of [`Store::read_sequence`]; [`StoreErrorKind::NumbersExhausted`] when the number would be `u32::MAX`,
    /// [`StoreErrorKind::NumberTaken`] for an [`AccountNumber::Exactly`] below `nextNumber` or already recorded,
    /// [`StoreErrorKind::LeftoverUnopenable`] for one whose leftover credential file does not open as the account's,
    /// [`StoreErrorKind::AccountExists`] for an email and organization already stored or twice in the batch, and
    /// [`StoreErrorKind::ConfigWrite`] for an `oauthAccount` that does not name the account, each with nothing written;
    /// then the errors of [`Store::write_account_config`], [`StoreErrorKind::CredentialsWrite`], and those of
    /// [`Store::write_sequence`].
    pub fn add_accounts(
        &self,
        lock: &AccountLockGuard,
        accounts: &[NewAccount<'_>],
    ) -> Result<Vec<u32>, StoreError> {
        self.check_write(lock, None)?;
        let mut sequence = match self.read_sequence()? {
            Some(sequence) => sequence,
            None => Sequence::from_document(Map::new())?,
        };
        let (additions, next_number) = self.plan_additions(&sequence, accounts)?;
        for (account, addition) in accounts.iter().zip(&additions) {
            self.write_account_config(lock, addition.record.number, account.oauth_account)?;
        }
        for (account, addition) in accounts.iter().zip(&additions) {
            self.seal_addition(addition, account.credentials)?;
        }
        for addition in &additions {
            sequence.add_record(&addition.record);
        }
        sequence.next_number = sequence.next_number.max(next_number);
        self.write_sequence_file(&sequence)?;
        Ok(additions
            .iter()
            .map(|addition| addition.record.number)
            .collect())
    }

    /// [`Store::add_accounts`]' checks: each account's record, numbered and at the generation its file will be sealed
    /// at, and the leftover file at its number, if one opens; with the `nextNumber` to write, one past the highest
    /// number old and new, and never below the stored one.
    fn plan_additions(
        &self,
        sequence: &Sequence,
        accounts: &[NewAccount<'_>],
    ) -> Result<(Vec<Addition>, u32), StoreError> {
        let stored_next = sequence.next_number;
        let mut next = stored_next;
        let mut taken: BTreeSet<u32> = sequence
            .accounts
            .keys()
            .chain(&sequence.numbers)
            .copied()
            .collect();
        let mut identities: Vec<(Identity, Option<u32>)> = sequence
            .every_record()
            .map(|record| (Identity::of(record), Some(record.number)))
            .collect();
        let mut additions = Vec::with_capacity(accounts.len());
        for account in accounts {
            account.check_oauth_account()?;
            let identity = Identity::new(account.email.as_str(), &account.org_uuid);
            if let Some((_, number)) = identities.iter().find(|(known, _)| *known == identity) {
                return Err(StoreError::new(StoreErrorKind::AccountExists, *number));
            }
            let addition = match account.number {
                AccountNumber::Next => self.next_addition(account, next, &taken)?,
                AccountNumber::Exactly(number) => {
                    self.exact_addition(account, number, stored_next, &taken)?
                }
            };
            let number = addition.record.number;
            // Never u32::MAX: both kinds of addition refuse it.
            next = next.max(number.saturating_add(1));
            taken.insert(number);
            identities.push((identity, None));
            additions.push(addition);
        }
        Ok((additions, next))
    }

    /// The first number from `next` on that no account holds and whose leftover credential file, if any, opens as
    /// `account`'s. A leftover that does not open is kept and its number skipped; one that cannot be read for another
    /// reason refuses the add with that error.
    fn next_addition(
        &self,
        account: &NewAccount<'_>,
        next: u32,
        taken: &BTreeSet<u32>,
    ) -> Result<Addition, StoreError> {
        let mut number = next;
        loop {
            if number == u32::MAX {
                return Err(StoreError::new(StoreErrorKind::NumbersExhausted, None));
            }
            if !taken.contains(&number) {
                match self.addition_at(account, number) {
                    Err(error) if error.kind() == StoreErrorKind::LeftoverUnopenable => {
                        tracing::info!(
                            account = number,
                            "a leftover credential file does not open as the new account's; the number is skipped"
                        );
                    }
                    found => return found,
                }
            }
            number += 1;
        }
    }

    /// `account` at exactly `number`: at or past the stored `nextNumber`, held by no account nor earlier in the batch,
    /// and with no leftover credential file there, or one that opens as `account`'s.
    fn exact_addition(
        &self,
        account: &NewAccount<'_>,
        number: u32,
        stored_next: u32,
        taken: &BTreeSet<u32>,
    ) -> Result<Addition, StoreError> {
        if number == u32::MAX {
            return Err(StoreError::new(
                StoreErrorKind::NumbersExhausted,
                Some(number),
            ));
        }
        if number < stored_next || taken.contains(&number) {
            return Err(StoreError::new(StoreErrorKind::NumberTaken, Some(number)));
        }
        self.addition_at(account, number)
    }

    /// `account`'s record at `number`, and the leftover credential file there when one opens under the record's
    /// binding; [`StoreErrorKind::LeftoverUnopenable`] when a file is there and does not open (foreign, sealed for
    /// another binding, or rolled back), and the read's own error when it cannot be read.
    fn addition_at(&self, account: &NewAccount<'_>, number: u32) -> Result<Addition, StoreError> {
        let mut record = account.record(number);
        let leftover = self
            .read_credential_file(number, &binding_of(&record))
            .map_err(|error| match error.kind() {
                StoreErrorKind::Foreign
                | StoreErrorKind::CredentialsUnopenable
                | StoreErrorKind::CredentialsRolledBack => {
                    StoreError::new(StoreErrorKind::LeftoverUnopenable, Some(number))
                }
                _ => error,
            })?;
        record.generation = leftover
            .as_ref()
            .map_or(1, |leftover| leftover.generation.saturating_add(1));
        Ok(Addition { record, leftover })
    }

    /// Writes `blob` as `addition`'s credential file, sealed at its record's generation, keeping a differing leftover
    /// as `.prev`.
    fn seal_addition(&self, addition: &Addition, blob: &CredentialBlob) -> Result<(), StoreError> {
        let record = &addition.record;
        let ad = binding_of(record).associated_data(record.number, record.generation);
        let sealed = seal_credentials(record.number, &ad, blob)?;
        let differing = addition
            .leftover
            .as_ref()
            .filter(|leftover| leftover.blob.expose() != blob.expose());
        if let Some(Retention::Failed(kind)) =
            differing.map(|leftover| self.keep_previous(record.number, leftover))
        {
            tracing::warn!(
                account = record.number,
                ?kind,
                "the leftover credential file could not be kept as its .prev copy"
            );
        }
        self.write_sealed(
            &self.root.credential_path(record.number),
            &sealed,
            &ad,
            blob,
            Some(record.number),
        )
    }

    /// Records `uuid` as account `number`'s uuid when it has none: only while the account still has no uuid and still
    /// holds `expected`'s email and organization, so an account removed and re-added in the meantime never gets its
    /// predecessor's uuid. An existing uuid is never rewritten. Returns whether the file was written.
    ///
    /// # Errors
    ///
    /// [`StoreErrorKind::WrongLock`], [`StoreErrorKind::StoreReadOnly`], [`StoreErrorKind::UnknownSchema`],
    /// [`StoreErrorKind::AccountsMissing`], or the errors of [`Store::read_sequence`] and [`Store::write_sequence`].
    pub fn backfill_uuid(
        &self,
        lock: &AccountLockGuard,
        number: u32,
        uuid: &str,
        expected: &Identity,
    ) -> Result<bool, StoreError> {
        self.check_write(lock, Some(number))?;
        let Some(mut sequence) = self.read_sequence()? else {
            return Err(StoreError::new(
                StoreErrorKind::AccountsMissing,
                Some(number),
            ));
        };
        let fillable = sequence.accounts.get(&number).is_some_and(|account| {
            account.uuid.trim().is_empty()
                && account.email.as_str() == expected.email
                && account.org_uuid == expected.org_uuid
        });
        if uuid.is_empty() || !fillable {
            return Ok(false);
        }
        sequence.set_uuid(number, uuid);
        self.write_sequence_file(&sequence)?;
        Ok(true)
    }

    fn check_write(&self, lock: &AccountLockGuard, account: Option<u32>) -> Result<(), StoreError> {
        if *lock.root() != self.root {
            return Err(StoreError::new(StoreErrorKind::WrongLock, account));
        }
        match self.write_refusal() {
            Some(kind) => Err(StoreError::new(kind, account)),
            None => Ok(()),
        }
    }

    /// Why the store would refuse every write now, `None` when it would take one. Every write checks it first, and so
    /// do the active account's poll before it spends a grant or stashes, the switch before its locks, the server's
    /// switch and membership routes, the auto-switch engine's tick, and the inactive accounts' refresh
    /// ([`crate::consume_backup_grant`]) before it takes the drain guard and spends:
    ///
    /// - [`StoreErrorKind::StoreReadOnly`] when sealing does not work in this process (the canary's seal-and-open round
    ///   trip failed), so a successor could never be written;
    /// - [`StoreErrorKind::UnknownSchema`] when `accounts.json` is a later version's;
    /// - [`StoreErrorKind::AccountsUnreadable`], [`StoreErrorKind::AccountsCorrupt`] or
    ///   [`StoreErrorKind::UnsafeEmail`] when `accounts.json` is there but cannot be read or parsed, so its version
    ///   cannot be known. A missing `accounts.json` refuses nothing.
    #[must_use]
    pub fn write_refusal(&self) -> Option<StoreErrorKind> {
        if !(self.canary)() {
            return Some(StoreErrorKind::StoreReadOnly);
        }
        version_refusal(&self.root)
    }

    /// Writes `sequence` under the account lock, with `nextNumber` and every account's credential generation the larger
    /// of its own and the one on disk now, so a `Sequence` read before another writer moved either never lowers them.
    fn write_sequence_file(&self, sequence: &Sequence) -> Result<(), StoreError> {
        if sequence.schema_version != SCHEMA_VERSION {
            return Err(StoreError::new(StoreErrorKind::UnknownSchema, None));
        }
        let mut sequence = sequence.clone();
        let on_disk = self.read_sequence()?;
        for record in on_disk.iter().flat_map(Sequence::every_record) {
            let Some(mine) = sequence.account(record.number) else {
                continue;
            };
            if mine.email != record.email || mine.org_uuid != record.org_uuid {
                return Err(StoreError::new(
                    StoreErrorKind::AccountReidentified,
                    Some(record.number),
                ));
            }
            if mine.generation < record.generation {
                sequence.set_generation(record.number, record.generation);
            }
        }
        let on_disk = on_disk.map_or(0, |current| current.next_number);
        let write_error = |error| StoreError::io(StoreErrorKind::AccountsWrite, None, error);
        let target = self.root.accounts_json();
        ensure_root(self.root.root()).map_err(write_error)?;
        let bytes =
            serde_json::to_vec_pretty(&sequence.to_write(sequence.next_number.max(on_disk)))
                .map_err(io::Error::other)
                .map_err(write_error)?;
        self.replace_guarded(&target, &bytes, |temp| {
            let written = read_optional(temp)?.unwrap_or_default();
            if written.as_slice() != bytes {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "the written file did not read back as written",
                ));
            }
            if serde_json::from_slice::<Value>(&written).is_err() {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "the written file did not parse back as JSON",
                ));
            }
            Ok(())
        })
        .map_err(|failed| StoreError::io(StoreErrorKind::AccountsWrite, None, failed.error))
    }

    /// Makes `target`'s folder, and every folder between the root and it, owner-only; the root is made first.
    fn prepare_folder(&self, target: &Path) -> io::Result<()> {
        ensure_root(self.root.root())?;
        match target.parent() {
            Some(folder) if folder != self.root.root() => {
                guard::create_folder(self.root.root(), folder).map_err(guard_io)
            }
            _ => Ok(()),
        }
    }

    /// Writes `bytes` to a temp file the guard creates beside `target`, runs `check` on it, then renames it onto
    /// `target`. A write that fails removes its temp file and reports whether it could not.
    fn replace_guarded(
        &self,
        target: &Path,
        bytes: &[u8],
        check: impl FnOnce(&Path) -> io::Result<()>,
    ) -> Result<(), TempWriteError> {
        let (temp, mut file) = self.guarded_temp(target)?;
        let synced = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        synced
            .and_then(|()| check(&temp))
            .and_then(|()| replace_with_retry(&temp, target))
            .map_err(|error| TempWriteError {
                error,
                temp_left: !remove_temp(&temp),
            })
    }

    /// A new temp file beside `target`, created by the guard with a fresh name: one that already exists, a link
    /// included, is never opened or removed, and the next serial is taken instead.
    fn guarded_temp(&self, target: &Path) -> Result<(PathBuf, File), TempWriteError> {
        let mut attempt = 1;
        loop {
            let temp = temp_path(target);
            match guard::create_file(self.root.root(), &temp) {
                Ok(file) => return Ok((temp, file)),
                Err(error) if is_already_exists(&error) && attempt < TEMP_NAME_ATTEMPTS => {
                    attempt += 1;
                }
                Err(error) => {
                    return Err(TempWriteError {
                        error: guard_io(error),
                        temp_left: false,
                    });
                }
            }
        }
    }

    /// A store file's bytes, read through the guard: every folder from the root down, and the file itself, is checked
    /// for a link, this user's ownership and no one else's write access before it is opened. `None` when it does not
    /// exist.
    fn read_unlinked(&self, path: &Path) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
        read_unlinked_in(&self.root, path)
    }
}

/// The file at `path` under `root`, read through the guard: `None` when it does not exist, an error when it cannot be
/// read or is reached through a link.
fn read_unlinked_in(root: &AccountsRoot, path: &Path) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    match guard::read_file(root.root(), path) {
        Ok(sealed) => Ok(Some(Zeroizing::new(sealed.as_bytes().to_vec()))),
        Err(error) if error.kind() == GuardErrorKind::Missing => Ok(None),
        Err(error) => Err(guard_io(error)),
    }
}

/// `accounts.json` of `root`, as [`Store::read_sequence`] reads it.
fn read_sequence_in(root: &AccountsRoot) -> Result<Option<Sequence>, StoreError> {
    let Some(bytes) = read_unlinked_in(root, &root.accounts_json())
        .map_err(|error| StoreError::io(StoreErrorKind::AccountsUnreadable, None, error))?
    else {
        return Ok(None);
    };
    let Ok(Value::Object(document)) = serde_json::from_slice::<Value>(&bytes) else {
        return Err(accounts_corrupt());
    };
    Sequence::from_document(document).map(Some)
}

/// Why `accounts.json` of `root` refuses a write to any file of the store, `None` when it takes one: the version half of
/// [`Store::write_refusal`], without the sealing canary, for the files that hold nothing sealed (the usage table, the
/// engine state and the engine lease). A missing `accounts.json` refuses nothing; one of another `schemaVersion` is
/// [`StoreErrorKind::UnknownSchema`]; one that cannot be read or parsed is that error's kind. It takes no lock, and
/// `accounts.json` is only ever replaced whole: the usage, state and lease writers call it holding their own file's
/// lock, whose acquire checked the root, while the stash calls it once holding no lock, after checking the folders on
/// the way, and again under its manifest lock.
pub(crate) fn version_refusal(root: &AccountsRoot) -> Option<StoreErrorKind> {
    match read_sequence_in(root) {
        Ok(None) => None,
        Ok(Some(sequence)) if sequence.schema_version == SCHEMA_VERSION => None,
        Ok(Some(_)) => Some(StoreErrorKind::UnknownSchema),
        Err(error) => Some(error.kind()),
    }
}

/// What a credential file's associated data is built from: the account's organization UUID, its identity (email) and
/// the generation of its last recorded write.
#[derive(Clone, Debug, Default)]
struct CredentialBinding {
    organization: String,
    generation: u64,
    identity: String,
}

impl CredentialBinding {
    /// Account `number`'s associated data at `generation`.
    fn associated_data(&self, number: u32, generation: u64) -> AssociatedData {
        AssociatedData::credentials(
            number,
            self.organization.clone(),
            generation,
            self.identity.clone(),
        )
    }
}

/// The binding of `record`'s credential file: the one place a record's organization, email and generation become the
/// parts of its associated data.
fn binding_of(record: &AccountRecord) -> CredentialBinding {
    CredentialBinding {
        organization: record.org_uuid.clone(),
        generation: record.generation,
        identity: record.email.as_str().to_owned(),
    }
}

/// `blob` sealed for account `number` under `ad`.
fn seal_credentials(
    number: u32,
    ad: &AssociatedData,
    blob: &CredentialBlob,
) -> Result<SealedBytes, StoreError> {
    seal::seal(SealPurpose::Credentials, ad, blob).map_err(|error| {
        StoreError::io(
            StoreErrorKind::CredentialsWrite,
            Some(number),
            io::Error::new(ErrorKind::InvalidInput, error.to_string()),
        )
    })
}

/// Which number [`Store::add_accounts`] gives a new account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountNumber {
    /// The next number: `nextNumber`, or past it when a leftover credential file there does not open as the account's.
    Next,
    /// Exactly this number, which must be at or past `nextNumber` and held by no account.
    Exactly(u32),
}

/// One account for [`Store::add_accounts`] to add: its record's fields, its `oauthAccount` and its credential. A new
/// account may not spend credits until the owner allows it.
pub struct NewAccount<'a> {
    pub number: AccountNumber,
    pub email: Email,
    pub uuid: String,
    /// The organization's UUID, empty for none.
    pub org_uuid: String,
    /// The organization's name, empty for none.
    pub org_name: String,
    /// When the account was added, as written.
    pub added: String,
    pub alias: Option<String>,
    /// Held out of automatic switching.
    pub disabled: bool,
    pub kind: AccountKind,
    /// The `oauthAccount` object its config file holds: it must name `email`, `org_uuid` and, when both carry one,
    /// `uuid`.
    pub oauth_account: &'a Value,
    pub credentials: &'a CredentialBlob,
}

impl fmt::Debug for NewAccount<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NewAccount")
            .field("number", &self.number)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl NewAccount<'_> {
    /// The account's record at `number`, before its first credential write.
    fn record(&self, number: u32) -> AccountRecord {
        AccountRecord {
            number,
            email: self.email.clone(),
            uuid: self.uuid.clone(),
            org_uuid: self.org_uuid.clone(),
            org_name: self.org_name.clone(),
            added: self.added.clone(),
            alias: self.alias.clone(),
            disabled: self.disabled,
            may_spend_credits: false,
            kind: self.kind,
            generation: 0,
        }
    }

    /// [`StoreErrorKind::ConfigWrite`] unless `oauth_account` is an object naming the account's email and organization,
    /// and its uuid when both carry one.
    fn check_oauth_account(&self) -> Result<(), StoreError> {
        let uuid_matches = match self
            .oauth_account
            .get("accountUuid")
            .and_then(Value::as_str)
        {
            Some(uuid) if !uuid.is_empty() && !self.uuid.is_empty() => uuid == self.uuid,
            Some(_) | None => true,
        };
        let names_account = identity_in(self.oauth_account)
            == Some(Identity::new(self.email.as_str(), &self.org_uuid));
        if self.oauth_account.is_object() && names_account && uuid_matches {
            return Ok(());
        }
        Err(StoreError::io(
            StoreErrorKind::ConfigWrite,
            None,
            io::Error::new(
                ErrorKind::InvalidInput,
                "the account's oauthAccount must be a JSON object naming its email, organization and uuid",
            ),
        ))
    }
}

/// An account [`Store::add_accounts`] will add: its record, at the generation its credential file is sealed at, and
/// the leftover file at its number when one opens as its own.
struct Addition {
    record: AccountRecord,
    leftover: Option<CurrentCredential>,
}

/// A credential file as read: the sealed bytes, what they hold, and the generation their header names.
struct CurrentCredential {
    sealed: SealedBytes,
    blob: CredentialBlob,
    generation: u64,
}

/// Makes `root` and its parent folder owner-only when they do not exist yet. The folder above the parent must exist:
/// `%LOCALAPPDATA%` for the real store, the test's scratch folder for a test.
pub(crate) fn ensure_root(root: &Path) -> io::Result<()> {
    if is_plain_dir(root) {
        return Ok(());
    }
    let parent = root.parent().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "the account store has no parent folder",
        )
    })?;
    if !is_plain_dir(parent) {
        let grandparent = parent.parent().ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "the account store's parent has no parent folder",
            )
        })?;
        if !is_plain_dir(grandparent) {
            fs::create_dir_all(grandparent)?;
        }
        guard::create_folder(grandparent, parent).map_err(guard_io)?;
    }
    guard::create_folder(parent, root).map_err(guard_io)
}

/// Refuses a root, or the folder it lives in, that is a link or reparse point, owned by another user, or writable by
/// another principal: a store reached that way is another store, and a file there is another file. A root or parent
/// that does not exist yet passes: it is made owner-only when it is created.
pub(crate) fn check_root(root: &AccountsRoot) -> Result<(), StoreError> {
    let path = root.root();
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_link(&metadata) => {
            return Err(StoreError::new(StoreErrorKind::RootUnsafe, None));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(StoreError::new(StoreErrorKind::RootNotDirectory, None));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(StoreError::io(StoreErrorKind::RootUnreadable, None, error)),
    }
    if let Some(parent) = path.parent() {
        match fs::symlink_metadata(parent) {
            Ok(metadata) if is_link(&metadata) => {
                return Err(StoreError::new(StoreErrorKind::RootUnsafe, None));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(StoreError::io(StoreErrorKind::RootUnreadable, None, error)),
        }
    }
    // The owner and DACL checks the guard makes, on the parent alone and then the root: the same owner-only rule the
    // store's files must pass.
    if let Some(parent) = path.parent().filter(|parent| is_plain_dir(parent)) {
        guard::check_folder(parent, parent).map_err(root_refusal)?;
        if is_plain_dir(path) {
            guard::check_folder(parent, path).map_err(root_refusal)?;
        }
    } else if is_plain_dir(path) {
        guard::check_folder(path, path).map_err(root_refusal)?;
    }
    Ok(())
}

/// The guard's refusal of a store root, as [`StoreErrorKind::RootUnsafe`] with its check in the text.
fn root_refusal(error: GuardError) -> StoreError {
    StoreError::io(
        StoreErrorKind::RootUnsafe,
        None,
        io::Error::new(ErrorKind::InvalidInput, error),
    )
}

#[cfg(windows)]
fn is_link(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
}

#[cfg(not(windows))]
fn is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// The guard's refusal as an I/O error that keeps its path and check in its text, which the log line carries, and the
/// guard error itself as its inner error (`get_ref`), whose own cause is next in the source chain.
fn guard_io(error: GuardError) -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, error)
}

/// [`guard_io`] under the store's kind for the read or write the guard refused; a file over 64 KiB is no sealed file.
fn refused_by_guard(kind: StoreErrorKind, account: Option<u32>, error: GuardError) -> StoreError {
    let kind = if error.kind() == GuardErrorKind::TooLarge {
        StoreErrorKind::Foreign
    } else {
        kind
    };
    StoreError::io(
        kind,
        account,
        io::Error::new(ErrorKind::InvalidInput, error),
    )
}

/// Whether the guard refused to create `error`'s name because something is there already.
pub(crate) fn is_already_exists(error: &GuardError) -> bool {
    std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<io::Error>())
        .is_some_and(|source| source.kind() == ErrorKind::AlreadyExists)
}

/// A write through a temp file that failed: why, and whether the temp file, which holds the bytes, is still on disk
/// because it could not be removed.
pub(crate) struct TempWriteError {
    pub(crate) error: io::Error,
    pub(crate) temp_left: bool,
}

/// Writes `bytes` to a secret temp file beside `target` that this call creates ([`create_secret_temp`]), runs `check`
/// on it, then renames it onto `target`. A write that fails removes its temp file and reports whether it could not.
pub(crate) fn replace_secret(
    target: &Path,
    bytes: &[u8],
    check: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), TempWriteError> {
    let (temp, mut file) = create_secret_temp(target).map_err(|error| TempWriteError {
        error,
        temp_left: false,
    })?;
    let synced = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    synced
        .and_then(|()| check(&temp))
        .and_then(|()| replace_with_retry(&temp, target))
        .map_err(|error| TempWriteError {
            error,
            temp_left: !remove_temp(&temp),
        })
}

/// Whether the file at `path` holds exactly `expected`: the read-back of a write that must land byte for byte.
pub(crate) fn read_back_equals(path: &Path, expected: &[u8]) -> io::Result<()> {
    let written = read_optional(path)?.ok_or_else(|| io::Error::from(ErrorKind::NotFound))?;
    if written.as_slice() == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            ErrorKind::InvalidData,
            "the written file did not read back as written",
        ))
    }
}

/// A new temp file beside `target`, created with [`secret_options`] and opened for writing ([`create_temp`]).
fn create_secret_temp(target: &Path) -> io::Result<(PathBuf, File)> {
    create_temp(target, &mut secret_options())
}

/// A new temp file beside `target`, created with `options` and `create_new`. A name that already exists (another
/// writer's, one a crashed run left behind, or a link planted there) is never opened, truncated or removed: the next
/// serial is taken instead, up to [`TEMP_NAME_ATTEMPTS`] names.
fn create_temp(target: &Path, options: &mut OpenOptions) -> io::Result<(PathBuf, File)> {
    options.create_new(true);
    let mut attempt = 1;
    loop {
        let temp = temp_path(target);
        match options.open(&temp) {
            Err(error)
                if error.kind() == ErrorKind::AlreadyExists && attempt < TEMP_NAME_ATTEMPTS =>
            {
                attempt += 1;
            }
            opened => return opened.map(|file| (temp, file)),
        }
    }
}

/// Options for writing a file that holds a secret: on Unix, a file they create is readable by its owner alone
/// (`0o600`).
fn secret_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
}

/// `<target stem>.<pid>.<serial>.tmp` beside `target`, unique to this write within the process.
fn temp_path(target: &Path) -> PathBuf {
    temp_name(target, TEMP_SERIAL.fetch_add(1, Ordering::Relaxed))
}

/// `<target stem>.<pid>.<serial>.tmp` beside `target`.
fn temp_name(target: &Path, serial: u64) -> PathBuf {
    target.with_extension(format!("{}.{serial}.tmp", process::id()))
}

/// Writes `value` as pretty JSON to `target` through a temp file beside it that this call creates ([`create_temp`]),
/// renamed over the target once its bytes have read back as JSON; the temp file is discarded when any later step fails.
pub(crate) fn write_json_atomic(target: &Path, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    let (temp, file) = create_temp(target, OpenOptions::new().write(true))?;
    let written =
        write_validated(file, &temp, &bytes).and_then(|()| replace_with_retry(&temp, target));
    if written.is_err() {
        discard_temp(&temp);
    }
    written
}

/// Writes `bytes` to `file`, the temp file at `path`, then reads them back and parses them, so a bad write is caught
/// before it replaces anything.
fn write_validated(mut file: File, path: &Path, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    let written = fs::read(path)?;
    if serde_json::from_slice::<Value>(&written).is_err() {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "the written file did not parse back as JSON",
        ));
    }
    Ok(())
}

fn discard_temp(path: &Path) {
    // The write's own error is the one worth reporting; a temp file that cannot be removed only leaves litter.
    let _removed = remove_temp(path);
}

/// Removes a temp file: whether it is gone (one already gone counts).
fn remove_temp(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(error) => error.kind() == ErrorKind::NotFound,
    }
}

/// A file's bytes in a buffer zeroed on drop, `None` when it does not exist, with transient sharing errors retried, and
/// a file that grew while it was read read again.
pub(crate) fn read_optional(path: &Path) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    absent_as_none(retry_growth(|| retry_transient(|| read_zeroizing(path))))
}

/// As [`read_optional`], for a file the store owns: one that is a symbolic link, a junction or
/// anything else but a regular file is refused with [`ErrorKind::InvalidInput`] and not read, so a link planted in its
/// place cannot hand it another file. The live Claude Code files are read with [`read_optional`], which follows links,
/// since a user may link them.
pub(crate) fn read_optional_unlinked(path: &Path) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    read_checked(|| entry_at(path), || read_zeroizing(path))
}

/// What a path names, from its own metadata, a link not followed.
#[derive(Debug, Clone, Copy)]
enum Entry {
    File,
    Link,
    Other,
}

fn entry_at(path: &Path) -> io::Result<Entry> {
    let file_type = fs::symlink_metadata(path)?.file_type();
    if file_type.is_symlink() {
        Ok(Entry::Link)
    } else if file_type.is_file() {
        Ok(Entry::File)
    } else {
        Ok(Entry::Other)
    }
}

/// Reads with `read`, retried as [`read_optional`] retries it, every attempt checking `entry` first so that a link
/// swapped in during a retry's wait is refused, not opened.
fn read_checked(
    mut entry: impl FnMut() -> io::Result<Entry>,
    mut read: impl FnMut() -> io::Result<Zeroizing<Vec<u8>>>,
) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    let mut attempt = || match entry()? {
        Entry::File => read(),
        Entry::Link => Err(io::Error::new(
            ErrorKind::InvalidInput,
            "the file is a symbolic link or junction, which is not followed",
        )),
        Entry::Other => Err(io::Error::new(
            ErrorKind::InvalidInput,
            "the file is not a regular file",
        )),
    };
    absent_as_none(retry_growth(|| retry_transient(&mut attempt)))
}

/// A read's bytes, `None` for a file that does not exist.
fn absent_as_none<T>(read: io::Result<T>) -> io::Result<Option<T>> {
    match read {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Whether `path` is a directory itself, not a symbolic link or junction to one (the file type of a link's own
/// metadata is neither a directory nor a file).
pub(crate) fn is_plain_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

/// Whether `path` is a regular file itself, not a symbolic link to one.
pub(crate) fn is_plain_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

/// Runs `read` again, with no wait, up to [`GROWTH_RETRIES`] more times while it fails because its file grew after
/// its length was read ([`FileGrew`]); any other error, and the last growth error, is returned.
fn retry_growth<T>(mut read: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut retries = 0;
    loop {
        match read() {
            Err(error) if retries < GROWTH_RETRIES && grew(&error) => retries += 1,
            result => return result,
        }
    }
}

/// The marker inside the [`ErrorKind::InvalidData`] error of a source longer than the size it was read with.
#[derive(Debug)]
struct FileGrew;

impl fmt::Display for FileGrew {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the file grew after its length was read")
    }
}

impl std::error::Error for FileGrew {}

/// Whether `error` is [`read_bounded`]'s growth error.
fn grew(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<FileGrew>)
}

/// Reads a whole file into a buffer sized from the length its metadata gives ([`read_bounded`]): a file that grew
/// since is an error, not a reallocation that would leave an unzeroed copy. A file longer than [`MAX_READ`] is an
/// [`ErrorKind::InvalidData`] error, with nothing allocated.
fn read_zeroizing(path: &Path) -> io::Result<Zeroizing<Vec<u8>>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    if length > MAX_READ {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "the file is larger than any file the store reads",
        ));
    }
    let size = usize::try_from(length).map_err(io::Error::other)?;
    read_bounded(&mut file, size)
}

/// Reads `reader` to its end into a buffer zeroed on drop that never grows: `size` bytes, plus one that only a longer
/// source fills. A source longer than `size` is an [`ErrorKind::InvalidData`] error carrying [`FileGrew`]; a shorter
/// one is read whole.
pub(crate) fn read_bounded(reader: &mut impl Read, size: usize) -> io::Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(vec![0; size.saturating_add(1)]);
    let mut filled = 0;
    while let Some(rest) = bytes.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        match reader.read(rest) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    if filled > size {
        return Err(io::Error::new(ErrorKind::InvalidData, FileGrew));
    }
    bytes.truncate(filled);
    Ok(bytes)
}

/// Renames `from` onto `to`, replacing it, with transient sharing errors retried.
fn replace_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    retry_transient(|| fs::rename(from, to))
}

/// Runs `operation` up to [`TRANSIENT_ATTEMPTS`] times while it fails with a transient Windows sharing error, waiting
/// 2 ms, then twice as long each time, up to 0.25 s.
pub(crate) fn retry_transient<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut delay = TRANSIENT_FIRST_DELAY;
    let mut attempt = 1;
    loop {
        match operation() {
            Err(error) if attempt < TRANSIENT_ATTEMPTS && is_transient(&error) => {
                thread::sleep(delay);
                delay = (delay * 2).min(TRANSIENT_MAX_DELAY);
                attempt += 1;
            }
            result => return result,
        }
    }
}

fn is_transient(error: &io::Error) -> bool {
    cfg!(windows)
        && error
            .raw_os_error()
            .is_some_and(|code| TRANSIENT_OS_ERRORS.contains(&code))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test that cannot set up its folder or read its files must fail loudly"
)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::Ordering;

    use super::{
        ErrorKind, File, GROWTH_RETRIES, GuardError, GuardErrorKind, MAX_READ, Map, Sequence,
        StoreErrorKind, TEMP_SERIAL, Value, grew, guard, guard_io, io, read_bounded, read_optional,
        read_optional_unlinked, refused_by_guard, retry_growth, root_refusal, temp_name,
        write_json_atomic,
    };

    fn sequence_of(document: &Value) -> Sequence {
        let document: Map<String, Value> = document
            .as_object()
            .cloned()
            .expect("the document is an object");
        Sequence::from_document(document).expect("the document is an account list")
    }

    #[test]
    fn sequence_is_empty_counts_records_outside_the_order() {
        assert!(
            sequence_of(&serde_json::json!({})).is_empty(),
            "a document with no account is empty"
        );
        assert!(
            sequence_of(&serde_json::json!({"order": [], "accounts": {}, "nextNumber": 4}))
                .is_empty(),
            "a used-up number range alone holds no account"
        );
        let outside = sequence_of(
            &serde_json::json!({"order": [], "accounts": {"2": {"email": "two@example.test"}}}),
        );
        assert!(
            outside.numbers().is_empty(),
            "the record is outside the order"
        );
        assert!(
            !outside.is_empty(),
            "a record outside the order is an account"
        );
        let listed = sequence_of(
            &serde_json::json!({"order": [2], "accounts": {"2": {"email": "two@example.test"}}}),
        );
        assert!(!listed.is_empty(), "a record in the order is an account");
        let order_only = sequence_of(&serde_json::json!({"order": [2], "accounts": {}}));
        assert!(
            !order_only.is_empty(),
            "a number in the order without a record is not an empty store"
        );
    }

    const LEFT: &[u8] = b"left by an earlier run";
    const CONFIG: &[u8] = br#"{"oauthAccount": {"emailAddress": "one@example.test"}}"#;

    /// A file that grew between its metadata and its read, then held still: the second read, sized afresh, fits.
    #[test]
    fn a_file_that_grows_once_is_read_on_the_retry() {
        let mut reads = 0;

        let read = retry_growth(|| {
            reads += 1;
            let size = if reads == 1 {
                CONFIG.len() - 1
            } else {
                CONFIG.len()
            };
            read_bounded(&mut &CONFIG[..], size)
        })
        .expect("the second read fits its size");

        assert_eq!(read.as_slice(), CONFIG);
        assert_eq!(reads, 2, "the file was read once more after it grew");
    }

    #[test]
    fn a_file_that_keeps_growing_fails_after_the_retries() {
        let mut reads = 0;

        let error = retry_growth(|| {
            reads += 1;
            read_bounded(&mut &CONFIG[..], CONFIG.len() - 1)
        })
        .expect_err("a file that grows on every read is refused");

        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(grew(&error), "the last growth error is the one returned");
        assert_eq!(
            reads,
            1 + GROWTH_RETRIES,
            "the read was tried once, then retried {GROWTH_RETRIES} times"
        );
    }

    #[test]
    fn invalid_data_that_is_not_growth_is_returned_without_a_retry() {
        let mut reads = 0;

        let error = retry_growth(|| -> io::Result<()> {
            reads += 1;
            Err(io::Error::new(
                ErrorKind::InvalidData,
                "not the growth marker",
            ))
        })
        .expect_err("the error is returned");

        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(!grew(&error));
        assert_eq!(reads, 1, "an error other than growth is not retried");
    }

    #[test]
    fn a_file_over_the_cap_fails_and_is_not_retried() {
        let dir =
            std::env::temp_dir().join(format!("claude-accounts-store-cap-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create the test folder");
        let path = dir.join("huge.json");
        File::create(&path)
            .and_then(|file| file.set_len(MAX_READ + 1))
            .expect("size the file past the cap");

        let read = read_optional(&path);
        let wiped = fs::remove_dir_all(&dir);

        let error = read.expect_err("a file past the cap is refused");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(
            !grew(&error),
            "a file past the cap is not retried as one that grew"
        );
        wiped.expect("remove the test folder");
    }

    /// A regular file held open without sharing, swapped for a link during the retry's wait: the retry checks again
    /// and refuses the link rather than opening it. Transient sharing errors exist only on Windows.
    #[cfg(windows)]
    #[test]
    fn a_link_swapped_in_during_a_transient_retry_is_refused() {
        use super::{Entry, read_checked};

        let mut checks = 0;
        let mut reads = 0;

        let error = read_checked(
            || {
                checks += 1;
                Ok(if checks == 1 {
                    Entry::File
                } else {
                    Entry::Link
                })
            },
            || {
                reads += 1;
                Err(io::Error::from_raw_os_error(32))
            },
        )
        .expect_err("the link is refused");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "the file is a symbolic link or junction, which is not followed"
        );
        assert_eq!(
            (checks, reads),
            (2, 1),
            "the second attempt checked again and never opened the link"
        );
    }

    #[test]
    fn a_folder_at_the_file_path_is_refused_as_not_a_regular_file() {
        let dir = std::env::temp_dir().join(format!(
            "claude-accounts-store-folder-{}",
            std::process::id()
        ));
        let path = dir.join("accounts.json");
        fs::create_dir_all(&path).expect("make a folder at the file's path");

        let read = read_optional_unlinked(&path);
        let wiped = fs::remove_dir_all(&dir);

        let error = read.expect_err("a folder is not read as the file");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "the file is not a regular file");
        wiped.expect("remove the test folder");
    }

    /// A file that grew between its metadata and its read is the source longer than the size it was read with.
    #[test]
    fn a_config_larger_than_its_metadata_is_refused() {
        let config: &[u8] = br#"{"oauthAccount": {"emailAddress": "one@example.test"}}"#;

        let grown = read_bounded(&mut &config[..], config.len() - 1)
            .expect_err("a source longer than its size is refused");
        let whole =
            read_bounded(&mut &config[..], config.len()).expect("a source of its size is read");
        let shrunk = read_bounded(&mut &config[..], config.len() + 5)
            .expect("a source shorter than its size is read");

        assert_eq!(grown.kind(), ErrorKind::InvalidData);
        assert_eq!(whole.as_slice(), config);
        assert_eq!(
            whole.capacity(),
            config.len() + 1,
            "the buffer never grew past its first size"
        );
        assert_eq!(shrunk.as_slice(), config);
    }

    #[test]
    fn json_temp_name_collision_takes_the_next_serial() {
        let dir = std::env::temp_dir().join(format!(
            "claude-accounts-store-json-unit-{}",
            std::process::id()
        ));
        // A folder left by an earlier run is wiped first; one that does not exist is the usual case.
        let _wiped = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the test's folder can be made");
        let target = dir.join("accounts.json");
        // TEMP_SERIAL is process-global: a parallel test that takes these serials first makes this pass vacuously, never fail.
        let next = TEMP_SERIAL.load(Ordering::Relaxed);
        let taken: Vec<_> = (next..next + 4)
            .map(|serial| temp_name(&target, serial))
            .collect();
        for path in &taken {
            fs::write(path, LEFT).expect("a taken temp name can be made");
        }

        write_json_atomic(&target, &serde_json::json!({ "written": true }))
            .expect("the write takes a free name");

        for path in &taken {
            assert_eq!(
                fs::read(path).expect("a taken temp file stays"),
                LEFT,
                "a taken name is never opened"
            );
        }
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(&target).expect("the target was written"))
                .expect("the target is JSON");
        assert_eq!(written, serde_json::json!({ "written": true }));
    }

    /// A guard refusal the store's own helpers wrap: a folder that is not below the base it is checked against.
    fn guard_refusal() -> GuardError {
        guard::check_folder(Path::new("the-store"), Path::new("elsewhere"))
            .expect_err("a folder outside the base is refused")
    }

    /// The guard error `error` keeps as its inner error.
    fn kept_guard(error: &io::Error) -> &GuardError {
        error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<GuardError>())
            .expect("the guard error is kept as the source")
    }

    /// [`guard_io`] keeps the guard error itself as its inner error.
    #[test]
    fn guard_io_keeps_the_guard_error_as_its_source() {
        let refusal = guard_refusal();
        let text = refusal.to_string();

        let error = guard_io(refusal);

        assert_eq!(
            error.kind(),
            ErrorKind::InvalidInput,
            "the I/O kind is unchanged"
        );
        assert_eq!(error.to_string(), text, "the refusal's text is unchanged");
        assert_eq!(
            kept_guard(&error).kind(),
            GuardErrorKind::Io,
            "the source is the guard error itself"
        );
    }

    /// [`root_refusal`] keeps the guard error itself as its I/O error's inner error, under the store's kind.
    #[test]
    fn root_refusal_keeps_the_guard_error_as_its_source() {
        let refusal = guard_refusal();
        let text = refusal.to_string();

        let error = root_refusal(refusal);

        assert_eq!(
            error.kind(),
            StoreErrorKind::RootUnsafe,
            "the store's kind is unchanged"
        );
        let io_error = std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<io::Error>())
            .expect("the root refusal carries the guard's I/O error");
        assert_eq!(
            io_error.kind(),
            ErrorKind::InvalidInput,
            "the I/O kind is unchanged"
        );
        assert_eq!(
            io_error.to_string(),
            text,
            "the refusal's text is unchanged"
        );
        assert_eq!(
            kept_guard(io_error).kind(),
            GuardErrorKind::Io,
            "the source is the guard error itself"
        );
    }

    /// [`refused_by_guard`] keeps the guard error itself as its I/O error's inner error, under the store's kind.
    #[test]
    fn refused_by_guard_keeps_the_guard_error_as_its_source() {
        let refusal = guard_refusal();
        let text = refusal.to_string();

        let error = refused_by_guard(StoreErrorKind::CredentialsUnreadable, Some(1), refusal);

        assert_eq!(
            error.kind(),
            StoreErrorKind::CredentialsUnreadable,
            "the store's kind is unchanged"
        );
        assert_eq!(error.account(), Some(1), "the account is unchanged");
        let io_error = std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<io::Error>())
            .expect("the store error carries the guard's I/O error");
        assert_eq!(
            io_error.kind(),
            ErrorKind::InvalidInput,
            "the I/O kind is unchanged"
        );
        assert_eq!(
            io_error.to_string(),
            text,
            "the refusal's text is unchanged"
        );
        assert_eq!(
            kept_guard(io_error).kind(),
            GuardErrorKind::Io,
            "the source is the guard error itself"
        );
    }
}
