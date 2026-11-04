//! The engine lease, `engine_lease.json`: which auto-switch engine is live on an account store. An engine claims the lease
//! when it is free or stale and heartbeats it while it runs; another engine sees it held, while it is fresh and its
//! holder is still running, and stands by. Every look and write runs under `.engine_lease.lock`, and the time and the
//! process table are always the caller's.

use std::fs;
use std::io::ErrorKind;

use proc_table::{ProcessInfo, ProcessTable};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::engine::{EngineError, read_file, store_refusal};
use crate::locks::{ACCOUNT_LOCK_TIMEOUT, FileLockGuard, acquire_in_root};
use crate::paths::AccountsRoot;
use crate::store;
use crate::usage::secs_from_timestamp;

/// How many of its holder's intervals a lease stays fresh without a heartbeat.
const FRESH_INTERVALS: f64 = 3.0;

/// How long after a lease's `startedAt` its holder's process may have started and still be its holder, in seconds:
/// `startedAt` is written after the process starts, and this absorbs the rounding of the two clocks' readings.
const START_SLACK_SECONDS: f64 = 1.0;

/// An engine that holds or wants the lease: its name, its process id, and how often it heartbeats.
#[derive(Clone, Debug, PartialEq)]
pub struct LeaseHolder {
    owner: String,
    pid: u32,
    interval_seconds: f64,
}

impl LeaseHolder {
    /// The engine `owner` (such as `rustling-tulip`) in process `pid`, heartbeating every `interval_seconds`.
    #[must_use]
    pub fn new(owner: impl Into<String>, pid: u32, interval_seconds: f64) -> Self {
        Self {
            owner: owner.into(),
            pid,
            interval_seconds,
        }
    }

    /// The engine's name.
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The engine's process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// How often the engine heartbeats, in seconds; the lease it writes records it.
    #[must_use]
    pub fn interval_seconds(&self) -> f64 {
        self.interval_seconds
    }
}

/// Who holds the lease, as one engine sees it.
#[derive(Clone, Debug, PartialEq)]
pub enum LeaseState {
    /// No engine holds it: there is no lease file, the lease is stale or corrupt, or the process it names is not running
    /// (no process with its pid, or only one that started after the lease did).
    Free,
    /// The engine asking holds it: the lease names its owner and its pid.
    Ours,
    /// Another engine holds it: it heartbeated within three of its own intervals and its process is still running, or
    /// the process table could not say (an empty table, or a process whose start time is unknown); or the lease is of
    /// another version than [`LEASE_SCHEMA_VERSION`], whatever its times say.
    HeldByOther {
        /// The holder's name.
        owner: String,
        /// The holder's process id.
        pid: u32,
        /// Seconds since the holder's last heartbeat.
        age_seconds: f64,
    },
}

/// The lease file's `schemaVersion` this crate reads and writes; a lease of another version counts as held by another
/// engine and is never overwritten. A lease without one is this version's.
pub const LEASE_SCHEMA_VERSION: u64 = 1;

/// The lease file as written: times in seconds since the epoch.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeaseRecord {
    /// Always [`LEASE_SCHEMA_VERSION`]: a lease of another version is read as [`StoredLease::Unknown`].
    #[serde(default = "lease_schema_version")]
    schema_version: u64,
    owner: String,
    pid: u32,
    started_at: f64,
    heartbeat_at: f64,
    interval_seconds: f64,
}

impl LeaseRecord {
    fn is_held_by(&self, me: &LeaseHolder) -> bool {
        self.owner == me.owner && self.pid == me.pid
    }

    /// The lease's state for `me` at `now` among `processes`: as [`LeaseRecord::state_by_time`] says, except that a lease
    /// held by another engine whose holder is not running is free. Only a held lease takes a snapshot of `processes`,
    /// and an empty snapshot says nothing of the holder, so the time rule stands.
    fn state_for(&self, me: &LeaseHolder, now: f64, processes: &dyn ProcessTable) -> LeaseState {
        let state = self.state_by_time(me, now);
        if !matches!(state, LeaseState::HeldByOther { .. }) {
            return state;
        }
        let snapshot = processes.snapshot();
        if snapshot.is_empty() || self.holder_runs_in(&snapshot) {
            return state;
        }
        tracing::info!(
            owner = self.owner.as_str(),
            pid = self.pid,
            "the engine lease names a holder that is not running; it counts as free"
        );
        LeaseState::Free
    }

    /// Whether the lease's holder is in `snapshot`: a process with its pid whose start is unknown or no later than
    /// [`START_SLACK_SECONDS`] after the lease's `startedAt`. A later start is another process that reused the pid.
    fn holder_runs_in(&self, snapshot: &[ProcessInfo]) -> bool {
        snapshot.iter().any(|process| {
            process.pid == self.pid
                && process.created.is_none_or(|created| {
                    secs_from_timestamp(created) <= self.started_at + START_SLACK_SECONDS
                })
        })
    }

    /// The lease's state for `me` at `now` by time alone: ours when it names `me`, else held while it heartbeated within
    /// [`FRESH_INTERVALS`] of the interval its holder recorded, else free.
    fn state_by_time(&self, me: &LeaseHolder, now: f64) -> LeaseState {
        if self.is_held_by(me) {
            return LeaseState::Ours;
        }
        let age_seconds = now - self.heartbeat_at;
        if age_seconds < FRESH_INTERVALS * self.interval_seconds {
            LeaseState::HeldByOther {
                owner: self.owner.clone(),
                pid: self.pid,
                age_seconds,
            }
        } else {
            LeaseState::Free
        }
    }
}

/// The version a lease written before its `schemaVersion` field reads as.
fn lease_schema_version() -> u64 {
    LEASE_SCHEMA_VERSION
}

/// The lease file as read.
enum StoredLease {
    /// A lease of [`LEASE_SCHEMA_VERSION`], or one written before the field.
    Known(LeaseRecord),
    /// A JSON object whose `schemaVersion` is not the integer [`LEASE_SCHEMA_VERSION`]: a newer or unknown engine's,
    /// held by it whatever its times say, so it is never claimed over. Its holder is read as far as the object names it.
    Unknown {
        /// The object's `owner`, `unknown` when it names none.
        owner: String,
        /// The object's `pid`, 0 when it names none.
        pid: u32,
        /// The object's `heartbeatAt` when it is a number.
        heartbeat_at: Option<f64>,
    },
}

impl StoredLease {
    /// The bytes of a lease file read as a lease, `None` when they are not JSON, not an object, or an object of this
    /// version (or without one) that is not a lease; that corrupt file is logged and counts as stale.
    fn parse(bytes: &[u8]) -> Option<Self> {
        let value = match serde_json::from_slice::<Value>(bytes) {
            Ok(value) => value,
            Err(error) => return corrupt(&error),
        };
        let Value::Object(object) = &value else {
            tracing::warn!(
                file = "the engine lease",
                "the engine lease file is not a JSON object; it counts as stale and may be claimed"
            );
            return None;
        };
        if object
            .get("schemaVersion")
            .is_some_and(|version| version.as_u64() != Some(LEASE_SCHEMA_VERSION))
        {
            tracing::info!(
                "the engine lease is of a newer or unknown version; it counts as held by another engine"
            );
            return Some(Self::Unknown {
                owner: object
                    .get("owner")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned(),
                pid: object
                    .get("pid")
                    .and_then(Value::as_u64)
                    .and_then(|pid| u32::try_from(pid).ok())
                    .unwrap_or(0),
                heartbeat_at: object.get("heartbeatAt").and_then(Value::as_f64),
            });
        }
        match LeaseRecord::deserialize(&value) {
            Ok(record) => Some(Self::Known(record)),
            Err(error) => corrupt(&error),
        }
    }

    /// The lease's state for `me` at `now` among `processes`: a known lease's by [`LeaseRecord::state_for`], an unknown
    /// version's always held by another engine, aged from its `heartbeatAt` (0 without one).
    fn state_for(&self, me: &LeaseHolder, now: f64, processes: &dyn ProcessTable) -> LeaseState {
        match self {
            Self::Known(record) => record.state_for(me, now, processes),
            Self::Unknown {
                owner,
                pid,
                heartbeat_at,
            } => LeaseState::HeldByOther {
                owner: owner.clone(),
                pid: *pid,
                age_seconds: heartbeat_at.map_or(0.0, |at| now - at),
            },
        }
    }

    /// The lease, when it is of this crate's version.
    fn known(self) -> Option<LeaseRecord> {
        match self {
            Self::Known(record) => Some(record),
            Self::Unknown { .. } => None,
        }
    }
}

/// A lease file that does not parse: logged and read as none, so it counts as stale.
fn corrupt(error: &serde_json::Error) -> Option<StoredLease> {
    // The parser's message can quote the file's text, so only its category is logged.
    tracing::warn!(
        file = "the engine lease",
        error = ?error.classify(),
        "the engine lease file is not a lease; it counts as stale and may be claimed"
    );
    None
}

/// The engine lease of one account store.
#[derive(Clone, Debug)]
pub struct Lease {
    root: AccountsRoot,
}

impl Lease {
    /// The lease of `root`, `engine_lease.json` under `.engine_lease.lock`.
    #[must_use]
    pub fn new(root: &AccountsRoot) -> Self {
        Self { root: root.clone() }
    }

    /// Who holds the lease at `now`, as `me` sees it, with `processes` telling whether another holder still runs. Writes
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`EngineError::Lock`] when the lease lock is not taken within [`ACCOUNT_LOCK_TIMEOUT`],
    /// [`EngineError::Read`] when the lease file exists but cannot be read.
    pub fn observe(
        &self,
        me: &LeaseHolder,
        now: jiff::Timestamp,
        processes: &dyn ProcessTable,
    ) -> Result<LeaseState, EngineError> {
        let _lock = self.take_lock()?;
        let now = secs_from_timestamp(now);
        Ok(self.read_record()?.map_or(LeaseState::Free, |stored| {
            stored.state_for(me, now, processes)
        }))
    }

    /// When `me`'s hold on the lease began (the lease file's `startedAt`, kept across heartbeats and set anew at each
    /// claim), in seconds since the epoch; `None` when the lease is not `me`'s. A process that reused `me`'s pid claims a
    /// lease that begins later. Writes nothing.
    ///
    /// # Errors
    ///
    /// As [`Lease::observe`].
    pub fn started_at(&self, me: &LeaseHolder) -> Result<Option<f64>, EngineError> {
        let _lock = self.take_lock()?;
        Ok(self
            .read_record()?
            .and_then(StoredLease::known)
            .filter(|record| record.is_held_by(me))
            .map(|record| record.started_at))
    }

    /// Claims the lease for `me` at `now` when it is free, stale or names a holder that is not running in `processes`,
    /// or heartbeats it when it is already ours, and returns [`LeaseState::Ours`]; the lease written carries
    /// [`LEASE_SCHEMA_VERSION`]. A lease another engine holds fresh and still runs, or one of another version, is left
    /// alone and returned as it is.
    ///
    /// # Errors
    ///
    /// As [`Lease::observe`], [`EngineError::StoreRefused`] when `accounts.json` refuses writes, and
    /// [`EngineError::Write`] when the lease cannot be written; nothing is written then.
    pub fn claim_or_heartbeat(
        &self,
        me: &LeaseHolder,
        now: jiff::Timestamp,
        processes: &dyn ProcessTable,
    ) -> Result<LeaseState, EngineError> {
        let _lock = self.take_lock()?;
        let now = secs_from_timestamp(now);
        let stored = self.read_record()?;
        let state = stored.as_ref().map_or(LeaseState::Free, |stored| {
            stored.state_for(me, now, processes)
        });
        let started_at = match (&state, stored) {
            (LeaseState::HeldByOther { .. }, _) => return Ok(state),
            (LeaseState::Ours, Some(StoredLease::Known(record))) => record.started_at,
            _ => now,
        };
        store_refusal(&self.root)?;
        let claimed = LeaseRecord {
            schema_version: LEASE_SCHEMA_VERSION,
            owner: me.owner.clone(),
            pid: me.pid,
            started_at,
            heartbeat_at: now,
            interval_seconds: me.interval_seconds,
        };
        store::write_json_atomic(&self.root.engine_lease(), &claimed)
            .map_err(EngineError::Write)?;
        Ok(LeaseState::Ours)
    }

    /// Removes the lease when it is `me`'s, whether fresh or stale; any other lease, one of another version included, is
    /// left alone. Returns whether a lease was removed. It is never refused on the store's version, since it only removes
    /// this engine's own record.
    ///
    /// # Errors
    ///
    /// As [`Lease::observe`], and [`EngineError::Write`] when our lease cannot be removed.
    pub fn release(&self, me: &LeaseHolder) -> Result<bool, EngineError> {
        let _lock = self.take_lock()?;
        if !self
            .read_record()?
            .and_then(StoredLease::known)
            .is_some_and(|record| record.is_held_by(me))
        {
            return Ok(false);
        }
        match fs::remove_file(self.root.engine_lease()) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(EngineError::Write(error)),
        }
    }

    /// Takes the lease lock, once the folders on the way to the lock and the lease are checked
    /// ([`AccountsRoot::check_folders`]): every look and write starts here. A linked folder is [`EngineError::Read`].
    fn take_lock(&self) -> Result<FileLockGuard, EngineError> {
        let lock = self.root.engine_lease_lock();
        self.root.check_folders(&lock).map_err(EngineError::Read)?;
        self.root
            .check_folders(&self.root.engine_lease())
            .map_err(EngineError::Read)?;
        acquire_in_root(&self.root, &lock, ACCOUNT_LOCK_TIMEOUT).map_err(EngineError::Lock)
    }

    /// The lease file, `None` when there is none or it is corrupt; a corrupt one is logged and counts as stale
    /// ([`StoredLease::parse`]).
    ///
    /// It holds no secret (the owner's name, its pid, its start and heartbeat times and its interval), so nothing is
    /// scrubbed.
    fn read_record(&self) -> Result<Option<StoredLease>, EngineError> {
        let Some(bytes) = read_file(&self.root.engine_lease())? else {
            return Ok(None);
        };
        Ok(StoredLease::parse(&bytes))
    }
}
