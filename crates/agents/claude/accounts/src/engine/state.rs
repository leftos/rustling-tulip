//! cswap's `autoswitch_state.json`, read and written under `.autoswitch_state.lock` as cswap's engine does.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::engine::{EngineError, LeaseHolder, read_file, store_refusal};
use crate::locks::{ACCOUNT_LOCK_TIMEOUT, acquire_in_root};
use crate::paths::AccountsRoot;
use crate::store;

/// The `schemaVersion` cswap stamps on the state file at every write.
pub const STATE_SCHEMA_VERSION: u64 = 1;

/// cswap's auto-switch state, shared by every engine on an account store: when and where the last switch went, what the
/// account it left looked like then, and the quarantined accounts. Times are seconds since the epoch, as cswap writes
/// them. The file is read key by key, as cswap reads it: a key whose value has another type than cswap writes reads
/// as absent and stays in [`AutoswitchState::extra`] as written, beside every key this crate does not model, and all of
/// them are written back unchanged; a typed field that is set replaces the key's value.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct AutoswitchState {
    /// The file's schema version (`schemaVersion`); every update stamps [`STATE_SCHEMA_VERSION`].
    pub schema_version: Option<u64>,
    /// When an engine last switched (`lastSwitchAt`): the cooldown runs from here.
    pub last_switch_at: Option<f64>,
    /// The account number the last switch went to (`lastSwitchTo`), a string as cswap writes it.
    pub last_switch_to: Option<String>,
    /// The account number the last switch left (`lastSwitchFrom`), a number as cswap writes it.
    pub last_switch_from: Option<u32>,
    /// The left account's headroom at the switch (`leftHeadroom`): `None` when the key is absent (a record from
    /// before cswap wrote it: no evidence), `Some(None)` when it is null (unmeasured, as a failover leaves it).
    pub left_headroom: Option<Option<f64>>,
    /// When the left account's window recovers (`leftRecoveryAt`): `None` when the key is absent, `Some(None)` when it
    /// is null, which means never (cswap's infinity).
    pub left_recovery_at: Option<Option<f64>>,
    /// What triggered the last switch (`leftTrigger`), such as `proactive`, `consume-first` or `failover`.
    pub left_trigger: Option<String>,
    /// The quarantined accounts by account number (`quarantine`): every key of the file's `quarantine` object, an
    /// entry this crate cannot read included, since cswap counts every key as quarantined.
    pub quarantine: BTreeMap<String, QuarantineRecord>,
    /// The paid fallback this store's engine is in (`paidFallback`): the account whose prepaid credits it spends on.
    /// `None` when it is in none, and when the key is malformed, which is then dropped at the next write; a malformed
    /// key with an `ownerExit` reads as its bar instead, written back in this crate's shape.
    pub paid_fallback: Option<PaidFallback>,
    /// Every other key of the file, and every key whose value has the wrong type, as written.
    pub extra: Map<String, Value>,
}

/// One account's entry in the state file's `quarantine` object.
#[derive(Clone, Debug, PartialEq)]
pub enum QuarantineRecord {
    /// An entry of the shape cswap's `_quarantine` writes.
    Entry(QuarantineEntry),
    /// An entry that is not of that shape (not an object, or a key missing or of another type), kept as written.
    Malformed(Value),
}

impl QuarantineRecord {
    /// The entry, when it has cswap's shape.
    #[must_use]
    pub fn entry(&self) -> Option<&QuarantineEntry> {
        match self {
            Self::Entry(entry) => Some(entry),
            Self::Malformed(_) => None,
        }
    }

    fn read(value: Value) -> Self {
        match QuarantineEntry::deserialize(&value) {
            Ok(entry) => Self::Entry(entry),
            Err(_) => Self::Malformed(value),
        }
    }

    fn to_value(&self) -> Value {
        match self {
            Self::Entry(entry) => entry.to_value(),
            Self::Malformed(value) => value.clone(),
        }
    }
}

/// One quarantined account, as cswap's `_quarantine` records it. Every other key is kept in
/// [`QuarantineEntry::extra`].
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct QuarantineEntry {
    /// The account's email.
    pub email: String,
    /// Why it was quarantined, such as `invalid_grant` or `identity-conflict`.
    pub reason: String,
    /// When, as an RFC 3339 UTC time to the second.
    pub at: String,
    /// The fingerprint of the refresh token it was quarantined with: a replaced credential releases the quarantine.
    pub refresh_token_fingerprint: Option<String>,
    /// Every other key of the entry, as written.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl QuarantineEntry {
    /// An entry for the account `email`, quarantined for `reason` at `at` (RFC 3339 UTC to the second, as cswap's
    /// `_now_iso` writes it) with the refresh token whose fingerprint is `refresh_token_fingerprint`.
    #[must_use]
    pub fn new(
        email: impl Into<String>,
        reason: impl Into<String>,
        at: impl Into<String>,
        refresh_token_fingerprint: Option<String>,
    ) -> Self {
        Self {
            email: email.into(),
            reason: reason.into(),
            at: at.into(),
            refresh_token_fingerprint,
            extra: Map::new(),
        }
    }

    fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("email".to_owned(), Value::from(self.email.clone()));
        map.insert("reason".to_owned(), Value::from(self.reason.clone()));
        map.insert("at".to_owned(), Value::from(self.at.clone()));
        map.insert(
            "refreshTokenFingerprint".to_owned(),
            self.refresh_token_fingerprint
                .clone()
                .map_or(Value::Null, Value::from),
        );
        for (key, value) in &self.extra {
            map.entry(key.clone()).or_insert_with(|| value.clone());
        }
        Value::Object(map)
    }
}

/// The paid fallback an engine is in (`paidFallback`): once every account was exhausted it moved the login onto
/// account `number`, funded with prepaid extra-usage credits, from account `from`, at `since`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PaidFallback {
    /// The account whose credits are spent on.
    pub number: u32,
    /// The account the login left at entry; `number` itself for an entry in place.
    pub from: u32,
    /// When the paid fallback began, in seconds since the epoch, as `lastSwitchAt` is written.
    pub since: f64,
    /// The bar an owner's move off the paid account set (`ownerExit`); omitted when none.
    #[serde(default)]
    pub owner_exit: Option<OwnerExit>,
    /// The engine that entered it (`leaseOwner`): an engine that is not this one ignores the key.
    pub lease_owner: LeaseOwner,
    /// Every other key of the object, as written.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl PaidFallback {
    /// A paid fallback on account `number`, entered from `from` at `since` by the engine `lease_owner`, with no bar.
    #[must_use]
    pub fn new(number: u32, from: u32, since: f64, lease_owner: LeaseOwner) -> Self {
        Self {
            number,
            from,
            since,
            owner_exit: None,
            lease_owner,
            extra: Map::new(),
        }
    }

    fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("number".to_owned(), Value::from(self.number));
        map.insert("from".to_owned(), Value::from(self.from));
        map.insert("since".to_owned(), Value::from(self.since));
        if let Some(bar) = &self.owner_exit {
            map.insert("ownerExit".to_owned(), bar.to_value());
        }
        let mut owner = Map::new();
        owner.insert(
            "owner".to_owned(),
            Value::from(self.lease_owner.owner.clone()),
        );
        owner.insert("pid".to_owned(), Value::from(self.lease_owner.pid));
        owner.insert(
            "startedAt".to_owned(),
            Value::from(self.lease_owner.started_at),
        );
        map.insert("leaseOwner".to_owned(), Value::Object(owner));
        keep_extra(map, &self.extra)
    }
}

/// The bar an owner's move off the paid account sets: no paid entry until some account has
/// [`crate::engine::SPENT_HEADROOM_PCT`] headroom again, or the opted-in accounts gain one missing from `opted_in`, the
/// accounts whose `maySpendCredits` was on when the bar was set. Turning a credits toggle off never lifts it, and the
/// bar keeps the set it was written with, so an account turned off and on again does not lift it either.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct OwnerExit {
    /// When the bar was set, in seconds since the epoch.
    pub at: f64,
    /// The opted-in accounts' numbers when the bar was set, in ascending order; `None` when the set could not be read
    /// (`optedIn` missing or malformed), and then only a refill lifts the bar.
    pub opted_in: Option<Vec<u32>>,
    /// Every other key of the object, as written.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl OwnerExit {
    /// A bar set at `at`, while the accounts `opted_in` had their credits toggle on.
    #[must_use]
    pub fn new(at: f64, mut opted_in: Vec<u32>) -> Self {
        opted_in.sort_unstable();
        opted_in.dedup();
        Self {
            at,
            opted_in: Some(opted_in),
            extra: Map::new(),
        }
    }

    fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("at".to_owned(), Value::from(self.at));
        if let Some(opted_in) = &self.opted_in {
            map.insert("optedIn".to_owned(), Value::from(opted_in.clone()));
        }
        keep_extra(map, &self.extra)
    }

    /// The bar of an `ownerExit` value that is not of this shape: its `at` when that is a number (else 0), its
    /// `optedIn` when that is an array of account numbers (else `None`), and its other keys when it is an object.
    fn salvage(value: &Value) -> Self {
        let object = value.as_object();
        let field = |key: &str| object.and_then(|object| object.get(key));
        Self {
            at: field("at").and_then(Value::as_f64).unwrap_or(0.0),
            opted_in: parsed(field("optedIn")),
            extra: object
                .map(|object| others(object, &["at", "optedIn"]))
                .unwrap_or_default(),
        }
    }
}

/// `map` with every key of `extra` it does not already hold, as an object.
fn keep_extra(mut map: Map<String, Value>, extra: &Map<String, Value>) -> Value {
    for (key, value) in extra {
        map.entry(key.clone()).or_insert_with(|| value.clone());
    }
    Value::Object(map)
}

/// The engine a paid fallback belongs to, as the engine lease names it: its owner name, its process id, and when its
/// hold on the lease began (`startedAt`), which tells it from a later process that reused the pid.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct LeaseOwner {
    /// The engine's owner name, such as `ops-tower`.
    pub owner: String,
    /// The engine's process id.
    pub pid: u32,
    /// When the engine's lease began, in seconds since the epoch, as [`crate::engine::Lease::started_at`] reads it.
    pub started_at: f64,
}

impl LeaseOwner {
    /// The engine `owner` in process `pid`, whose lease began at `started_at`.
    #[must_use]
    pub fn new(owner: impl Into<String>, pid: u32, started_at: f64) -> Self {
        Self {
            owner: owner.into(),
            pid,
            started_at,
        }
    }

    /// Whether this is the engine `holder` names, holding the lease that began at `started_at`: the owner, the pid and
    /// the lease's start all match.
    #[must_use]
    pub fn is(&self, holder: &LeaseHolder, started_at: f64) -> bool {
        self.owner == holder.owner()
            && self.pid == holder.pid()
            && self.started_at.total_cmp(&started_at).is_eq()
    }
}

impl AutoswitchState {
    /// Reads the state file of `root` under its lock, waiting up to [`ACCOUNT_LOCK_TIMEOUT`]. A missing file, or one
    /// that is not a JSON object, reads as the default, as cswap's `{}` does.
    ///
    /// # Errors
    ///
    /// [`EngineError::Lock`] when the lock is not taken, [`EngineError::Read`] when the file exists but cannot be read.
    pub fn read(root: &AccountsRoot) -> Result<Self, EngineError> {
        check_state_folders(root)?;
        let _lock = acquire_in_root(root, &root.autoswitch_state_lock(), ACCOUNT_LOCK_TIMEOUT)
            .map_err(EngineError::Lock)?;
        read_unlocked(root)
    }

    /// Reads, changes and writes the state file of `root` under its lock, waiting up to [`ACCOUNT_LOCK_TIMEOUT`], as
    /// cswap's `_mutate_state` does: the state read as [`AutoswitchState::read`] reads it, stamped with
    /// [`STATE_SCHEMA_VERSION`], is handed to `change` and written atomically. Returns the state written.
    ///
    /// # Errors
    ///
    /// As [`AutoswitchState::update_with`].
    pub fn update(
        root: &AccountsRoot,
        change: impl FnOnce(&mut Self),
    ) -> Result<Self, EngineError> {
        Self::update_with(root, ACCOUNT_LOCK_TIMEOUT, change)
    }

    /// [`AutoswitchState::update`], waiting up to `timeout` for the lock.
    ///
    /// # Errors
    ///
    /// [`EngineError::Lock`] when the lock is not taken, [`EngineError::StoreRefused`] when `accounts.json` refuses
    /// writes, [`EngineError::Read`] when the file exists but cannot be read, [`EngineError::UnknownVersion`] when it
    /// carries a `schemaVersion` that is not the integer [`STATE_SCHEMA_VERSION`], [`EngineError::Write`] when it
    /// cannot be written; nothing is written then.
    pub fn update_with(
        root: &AccountsRoot,
        timeout: Duration,
        change: impl FnOnce(&mut Self),
    ) -> Result<Self, EngineError> {
        check_state_folders(root)?;
        let _lock = acquire_in_root(root, &root.autoswitch_state_lock(), timeout)
            .map_err(EngineError::Lock)?;
        store_refusal(root)?;
        let mut state = read_unlocked(root)?;
        if !state.known_version() {
            return Err(unknown_version());
        }
        state.schema_version = Some(STATE_SCHEMA_VERSION);
        change(&mut state);
        store::write_json_atomic(&root.autoswitch_state(), &state.to_map())
            .map_err(EngineError::Write)?;
        Ok(state)
    }

    /// Reads the state file of `root` under its lock, waiting up to `timeout`, and hands it to `change`, which returns
    /// its result and whether the state is to be written. Only then is the state stamped with [`STATE_SCHEMA_VERSION`]
    /// and written atomically, as cswap writes the file only after a switch; otherwise the file keeps its bytes.
    ///
    /// # Errors
    ///
    /// As [`AutoswitchState::update_with`]; [`EngineError::StoreRefused`], [`EngineError::UnknownVersion`] and
    /// [`EngineError::Write`] only when `change` asked for the write.
    pub fn update_if<T>(
        root: &AccountsRoot,
        timeout: Duration,
        change: impl FnOnce(&mut Self) -> (T, bool),
    ) -> Result<T, EngineError> {
        check_state_folders(root)?;
        let _lock = acquire_in_root(root, &root.autoswitch_state_lock(), timeout)
            .map_err(EngineError::Lock)?;
        let mut state = read_unlocked(root)?;
        let known = state.known_version();
        let (result, write) = change(&mut state);
        if write {
            store_refusal(root)?;
            if !known {
                return Err(unknown_version());
            }
            state.schema_version = Some(STATE_SCHEMA_VERSION);
            store::write_json_atomic(&root.autoswitch_state(), &state.to_map())
                .map_err(EngineError::Write)?;
        }
        Ok(result)
    }

    /// [`EngineError::UnknownVersion`], logged as a refused write is, when the state as read is of a newer or unknown
    /// version ([`AutoswitchState::known_version`]): what acts on it must not, since it could never be written.
    pub(crate) fn refuse_unknown_version(&self) -> Result<(), EngineError> {
        if self.known_version() {
            Ok(())
        } else {
            Err(unknown_version())
        }
    }

    /// False when the state as read carries a `schemaVersion` that is not the integer [`STATE_SCHEMA_VERSION`] (another
    /// number, or a value of another type, which stays in [`AutoswitchState::extra`]): a newer or unknown version wrote
    /// it, so it is read but never written ([`unknown_version`]). A state without one is this version's.
    fn known_version(&self) -> bool {
        !(self
            .schema_version
            .is_some_and(|version| version != STATE_SCHEMA_VERSION)
            || self.extra.contains_key("schemaVersion"))
    }

    /// The state in the file's object, each known key taken out of `map` only when it has the type cswap writes.
    fn from_map(mut map: Map<String, Value>) -> Self {
        Self {
            schema_version: take(&mut map, "schemaVersion"),
            last_switch_at: take(&mut map, "lastSwitchAt"),
            last_switch_to: take(&mut map, "lastSwitchTo"),
            last_switch_from: take(&mut map, "lastSwitchFrom"),
            left_headroom: take(&mut map, "leftHeadroom"),
            left_recovery_at: take(&mut map, "leftRecoveryAt"),
            left_trigger: take(&mut map, "leftTrigger"),
            quarantine: take_quarantine(&mut map),
            paid_fallback: take_paid_fallback(&mut map),
            extra: map,
        }
    }

    /// The file's object: [`AutoswitchState::extra`] with every set field put over it.
    fn to_map(&self) -> Map<String, Value> {
        let mut map = self.extra.clone();
        put(
            &mut map,
            "schemaVersion",
            self.schema_version.map(Value::from),
        );
        put(
            &mut map,
            "lastSwitchAt",
            self.last_switch_at.map(Value::from),
        );
        put(
            &mut map,
            "lastSwitchTo",
            self.last_switch_to.clone().map(Value::from),
        );
        put(
            &mut map,
            "lastSwitchFrom",
            self.last_switch_from.map(Value::from),
        );
        put(&mut map, "leftHeadroom", self.left_headroom.map(nullable));
        put(
            &mut map,
            "leftRecoveryAt",
            self.left_recovery_at.map(nullable),
        );
        put(
            &mut map,
            "leftTrigger",
            self.left_trigger.clone().map(Value::from),
        );
        if !self.quarantine.is_empty() {
            let entries = self
                .quarantine
                .iter()
                .map(|(number, record)| (number.clone(), record.to_value()))
                .collect();
            map.insert("quarantine".to_owned(), Value::Object(entries));
        }
        if let Some(paid) = &self.paid_fallback {
            map.insert("paidFallback".to_owned(), paid.to_value());
        }
        map
    }
}

/// [`EngineError::UnknownVersion`], logged at warn: a write of a state file of a newer or unknown version is refused.
fn unknown_version() -> EngineError {
    tracing::warn!(
        file = "the auto-switch state file",
        "the auto-switch state file is of a newer or unknown version; it is read and never written"
    );
    EngineError::UnknownVersion
}

/// `key`'s value when it reads as a `T`, taken out of `map`; a value of another type is left in place.
fn take<T: DeserializeOwned>(map: &mut Map<String, Value>, key: &str) -> Option<T> {
    let value = T::deserialize(map.get(key)?).ok()?;
    map.shift_remove(key);
    Some(value)
}

/// The `quarantine` object's entries, taken out of `map`; a `quarantine` that is not an object is left in place.
fn take_quarantine(map: &mut Map<String, Value>) -> BTreeMap<String, QuarantineRecord> {
    if !map.get("quarantine").is_some_and(Value::is_object) {
        return BTreeMap::new();
    }
    let Some(Value::Object(entries)) = map.shift_remove("quarantine") else {
        return BTreeMap::new();
    };
    entries
        .into_iter()
        .map(|(number, value)| (number, QuarantineRecord::read(value)))
        .collect()
}

/// The `paidFallback` key, taken out of `map` whatever it holds: a value not of [`PaidFallback`]'s shape reads as
/// `None`, so no paid state is ever read from a bad file, and is not written back; unless it is an object with an
/// `ownerExit` key, whose bar is kept ([`salvage_bar`]) so a bad file never lets the engine enter against the owner's
/// move. Each warning is logged once per process, since the state is read every tick until the next write.
fn take_paid_fallback(map: &mut Map<String, Value>) -> Option<PaidFallback> {
    static WARNED: AtomicBool = AtomicBool::new(false);
    static WARNED_BAR: AtomicBool = AtomicBool::new(false);
    let value = map.shift_remove("paidFallback")?;
    if let Ok(paid) = PaidFallback::deserialize(&value) {
        return Some(paid);
    }
    if let Some(barred) = salvage_bar(&value) {
        if !WARNED_BAR.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                file = "the auto-switch state file",
                "the paid fallback in autoswitch_state.json is malformed; its owner-exit bar is kept"
            );
        }
        return Some(barred);
    }
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            file = "the auto-switch state file",
            "the state file's paidFallback is malformed; it reads as no paid fallback and is dropped at the next write"
        );
    }
    None
}

/// The paid fallback of a malformed `paidFallback` object that has an `ownerExit` key, carrying that bar
/// ([`OwnerExit::salvage`]); each other field as written when it parses, else a default the barred key never acts on:
/// `number` 0 (no account's), `from` the `number`, `since` 0, and a `leaseOwner` that names no engine. `None` for a
/// value that is not an object or has no `ownerExit`, a null one included (no bar, as the derive reads it).
fn salvage_bar(value: &Value) -> Option<PaidFallback> {
    let object = value.as_object()?;
    let exit = object.get("ownerExit").filter(|exit| !exit.is_null())?;
    let number = parsed(object.get("number")).unwrap_or(0);
    Some(PaidFallback {
        number,
        from: parsed(object.get("from")).unwrap_or(number),
        since: parsed(object.get("since")).unwrap_or(0.0),
        owner_exit: Some(OwnerExit::salvage(exit)),
        lease_owner: parsed(object.get("leaseOwner"))
            .unwrap_or_else(|| LeaseOwner::new("", 0, 0.0)),
        extra: others(
            object,
            &["number", "from", "since", "ownerExit", "leaseOwner"],
        ),
    })
}

/// `value` read as a `T`, or `None` when it is missing or of another shape.
fn parsed<T: DeserializeOwned>(value: Option<&Value>) -> Option<T> {
    T::deserialize(value?).ok()
}

/// The entries of `object` whose key is not one of `known`.
fn others(object: &Map<String, Value>, known: &[&str]) -> Map<String, Value> {
    object
        .iter()
        .filter(|(key, _)| !known.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// Sets `key` to `value` when there is one, in place when the key is already there.
fn put(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), value);
    }
}

/// A number, or null for none.
fn nullable(value: Option<f64>) -> Value {
    value.map_or(Value::Null, Value::from)
}

/// The folders on the way to the state file and its lock, checked before either is opened
/// ([`AccountsRoot::check_folders`]); a linked folder is [`EngineError::Read`].
fn check_state_folders(root: &AccountsRoot) -> Result<(), EngineError> {
    root.check_folders(&root.autoswitch_state_lock())
        .map_err(EngineError::Read)?;
    root.check_folders(&root.autoswitch_state())
        .map_err(EngineError::Read)
}

/// The state file of `root`, read with its lock already held.
///
/// It holds no secret (cswap's engine state, kept as a `Value` to write back), so the parsed tree is dropped without
/// scrubbing.
fn read_unlocked(root: &AccountsRoot) -> Result<AutoswitchState, EngineError> {
    let path = root.autoswitch_state();
    let Some(bytes) = read_file(&path)? else {
        return Ok(AutoswitchState::default());
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(AutoswitchState::from_map(map)),
        Ok(_) => {
            tracing::warn!(
                file = "the auto-switch state file",
                "the auto-switch state file is not a JSON object; it reads as empty"
            );
            Ok(AutoswitchState::default())
        }
        Err(error) => {
            // The parser's message can quote the file's text, so only its category is logged.
            tracing::warn!(
                file = "the auto-switch state file",
                error = ?error.classify(),
                "the auto-switch state file is not JSON; it reads as empty"
            );
            Ok(AutoswitchState::default())
        }
    }
}
