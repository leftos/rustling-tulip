//! Device tokens and the pairing book.
//!
//! A tailnet device is approved once, by the owner at the PC: the device asks,
//! a loopback client approves, the daemon mints a short one-time code bound to
//! that request, and the device redeems it for a token. This module holds the
//! two halves of that flow and nothing that talks to a connection.
//!
//! [`DeviceStore`] is the persisted half: one record per approved device, each
//! holding only the hex SHA-256 of the device's token, written to
//! `devices.json` through a temp file and a rename. A file that cannot be read
//! or decoded verifies nothing and refuses to save, so a corrupt store fails
//! closed instead of being overwritten with a fresh one.
//!
//! [`PairingBook`] is the in-memory half: pending requests, their one-time
//! codes and the daemon-minted CLI codes. Nothing in it is written to disk — a
//! six-digit code on disk would be an invitation to brute-force it — and a
//! daemon restart drops every request and code, which is the safe default.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tailnet::PeerIdentity;

/// How long a request waits for the owner before it lapses.
pub const REQUEST_LIFETIME: Duration = Duration::from_mins(5);

/// How long an approved request's code stays redeemable.
pub const CODE_LIFETIME: Duration = Duration::from_mins(2);

/// Wrong codes a request survives before it is dropped.
pub const MAX_WRONG_ATTEMPTS: u32 = 5;

/// Requests that may wait at once.
pub const MAX_PENDING: usize = 8;

/// CLI codes that may be live at once; minting one more drops the oldest.
pub const MAX_CLI_CODES: usize = 4;

/// Wrong CLI redeems inside a rolling [`CODE_LIFETIME`] window that drop
/// every live CLI code.
pub const MAX_WRONG_CLI_REDEEMS: usize = 5;

/// One approved device.
///
/// The token itself is never kept: [`DeviceRecord::token_sha256`] is the
/// lower-case hex SHA-256 of the token's UTF-8 bytes.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// The record's id, a fresh UUID v4 on every issue.
    pub id: String,
    /// The device's Tailscale `StableNodeID`, the key a token is bound to.
    pub stable_id: String,
    /// The node's `MagicDNS` name at issue time, for display.
    pub name: String,
    /// The Tailscale login at issue time, for display.
    pub login: String,
    /// Lower-case hex SHA-256 of the device's token.
    pub token_sha256: String,
    /// When the token was issued.
    pub created_at: DateTime<Utc>,
    /// When the device last presented its token, if it has since issue.
    #[serde(default)]
    pub last_seen: Option<DateTime<Utc>>,
    /// Whether the record is blocked; a revoked token never verifies.
    #[serde(default)]
    pub revoked: bool,
}

impl fmt::Debug for DeviceRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceRecord")
            .field("id", &self.id)
            .field("stable_id", &self.stable_id)
            .field("name", &self.name)
            .field("login", &self.login)
            .field("token_sha256", &"<redacted>")
            .field("created_at", &self.created_at)
            .field("last_seen", &self.last_seen)
            .field("revoked", &self.revoked)
            .finish()
    }
}

/// On-disk shape of `devices.json`.
#[derive(Serialize, Deserialize)]
struct DevicesFile {
    #[serde(default)]
    devices: Vec<DeviceRecord>,
}

/// The approved devices, loaded from and saved to `devices.json`.
pub struct DeviceStore {
    path: PathBuf,
    devices: Vec<DeviceRecord>,
    /// The file could not be read or decoded: nothing verifies and nothing is
    /// written over it.
    corrupt: bool,
}

impl fmt::Debug for DeviceStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceStore")
            .field("path", &self.path)
            .field("devices", &self.devices)
            .field("corrupt", &self.corrupt)
            .finish()
    }
}

impl DeviceStore {
    /// Load the store at `path`.
    ///
    /// An absent file — `NotFound` — is an empty store that will be created
    /// on the first write. Any other read failure, and any undecodable file,
    /// leaves a store that verifies nothing and refuses to save, so a file
    /// that exists but cannot be read is preserved instead of being
    /// overwritten.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Self {
                    path: path.to_path_buf(),
                    devices: Vec::new(),
                    corrupt: false,
                };
            }
            Err(err) => {
                tracing::warn!(
                    ?err,
                    path = %path.display(),
                    "could not read devices.json; no device token will verify"
                );
                return Self::unusable(path);
            }
        };
        match serde_json::from_slice::<DevicesFile>(&bytes) {
            Ok(file) => Self {
                path: path.to_path_buf(),
                devices: file.devices,
                corrupt: false,
            },
            Err(err) => {
                tracing::warn!(
                    ?err,
                    path = %path.display(),
                    "devices.json is corrupt; no device token will verify and the file is left as it is"
                );
                Self::unusable(path)
            }
        }
    }

    fn unusable(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            devices: Vec::new(),
            corrupt: true,
        }
    }

    /// Whether the file could not be read or decoded.
    #[cfg(test)]
    #[must_use]
    pub fn is_corrupt(&self) -> bool {
        self.corrupt
    }

    /// Issue a fresh token bound to `peer` and store only its hash.
    ///
    /// A device that already has an unblocked record has it replaced — new
    /// id, new token — so one device holds one token and the old token stops
    /// verifying at once. A blocked device gets nothing until its record is
    /// forgotten. Returns the record id and the plaintext token; the store
    /// keeps neither the token nor any copy of it.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::PermissionDenied`] when the device is blocked; the
    /// store is corrupt (the file is left as it is) or writing it failed. The
    /// store is then unchanged.
    pub fn issue(
        &mut self,
        peer: &PeerIdentity,
        now: DateTime<Utc>,
    ) -> io::Result<(String, String)> {
        self.refuse_if_corrupt()?;
        if self.is_blocked(&peer.stable_id) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the device is blocked; forget it before it pairs again",
            ));
        }
        let token = crate::secret::generate_device_token();
        let id = uuid::Uuid::new_v4().to_string();
        let mut next: Vec<DeviceRecord> = self
            .devices
            .iter()
            .filter(|record| record.stable_id != peer.stable_id)
            .cloned()
            .collect();
        next.push(DeviceRecord {
            id: id.clone(),
            stable_id: peer.stable_id.clone(),
            name: peer.name.clone(),
            login: peer.login.clone(),
            token_sha256: digest_hex(&token),
            created_at: now,
            last_seen: None,
            revoked: false,
        });
        write_file(&self.path, &next)?;
        self.devices = next;
        Ok((id, token))
    }

    /// Verify `token` as presented by the device with `stable_id`.
    ///
    /// Every record is compared, in constant time and without stopping at the
    /// first match, so how long the check takes says nothing about which
    /// record matched. Only an unrevoked record of that same device is
    /// accepted, so a token copied to another tailnet node fails.
    #[must_use]
    pub fn verify(&self, stable_id: &str, token: &str) -> Option<&DeviceRecord> {
        if self.corrupt {
            return None;
        }
        let digest = digest_hex(token);
        let mut found: Option<&DeviceRecord> = None;
        for record in &self.devices {
            let matches =
                crate::secret::constant_time_eq(record.token_sha256.as_bytes(), digest.as_bytes());
            if matches && record.stable_id == stable_id && !record.revoked && found.is_none() {
                found = Some(record);
            }
        }
        found
    }

    /// Stamp `device_id` as last seen at `now`.
    ///
    /// Returns whether a record matched. Only stamps the record when the
    /// write succeeded.
    ///
    /// # Errors
    ///
    /// The store is corrupt (the file is left as it is) or writing it failed.
    pub fn touch(&mut self, device_id: &str, now: DateTime<Utc>) -> io::Result<bool> {
        self.refuse_if_corrupt()?;
        let mut next = self.devices.clone();
        let Some(record) = next.iter_mut().find(|record| record.id == device_id) else {
            return Ok(false);
        };
        record.last_seen = Some(now);
        write_file(&self.path, &next)?;
        self.devices = next;
        Ok(true)
    }

    /// Block a device: its token stops verifying and its record stays, so the
    /// device is refused a new token ([`DeviceStore::issue`],
    /// [`DeviceStore::is_blocked`]) until the owner forgets it.
    ///
    /// Returns whether a record matched.
    ///
    /// # Errors
    ///
    /// The store is corrupt (the file is left as it is) or writing it failed.
    /// A failed write still blocks the device in memory, so the block holds
    /// until the daemon restarts.
    pub fn revoke(&mut self, device_id: &str) -> io::Result<bool> {
        self.refuse_if_corrupt()?;
        let Some(record) = self
            .devices
            .iter_mut()
            .find(|record| record.id == device_id)
        else {
            return Ok(false);
        };
        record.revoked = true;
        write_file(&self.path, &self.devices)?;
        Ok(true)
    }

    /// Whether the device with `stable_id` has a blocked record.
    #[must_use]
    pub fn is_blocked(&self, stable_id: &str) -> bool {
        self.devices
            .iter()
            .any(|record| record.stable_id == stable_id && record.revoked)
    }

    /// Forget a device: its record goes, block included, so its token stops
    /// verifying and the device may ask to pair again.
    ///
    /// Returns whether a record matched; an unknown id writes nothing.
    ///
    /// # Errors
    ///
    /// The store is corrupt (the file is left as it is) or writing it failed.
    /// A failed write still removes the record in memory, so the device stays
    /// forgotten until the daemon restarts.
    pub fn forget(&mut self, device_id: &str) -> io::Result<bool> {
        self.refuse_if_corrupt()?;
        if !self.devices.iter().any(|record| record.id == device_id) {
            return Ok(false);
        }
        self.devices.retain(|record| record.id != device_id);
        write_file(&self.path, &self.devices)?;
        Ok(true)
    }

    /// Every record, in the order it was written.
    #[must_use]
    pub fn list(&self) -> &[DeviceRecord] {
        &self.devices
    }

    fn refuse_if_corrupt(&self) -> io::Result<()> {
        if self.corrupt {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "devices.json is corrupt; the file is left as it is",
            ));
        }
        Ok(())
    }
}

fn write_file(path: &Path, devices: &[DeviceRecord]) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(&DevicesFile {
        devices: devices.to_vec(),
    })
    .map_err(|err| io::Error::other(format!("serializing devices.json: {err}")))?;
    crate::atomic_file::write_atomic(path, &bytes)
        .map_err(|err| io::Error::other(format!("writing {}: {err:#}", path.display())))
}

/// Lower-case hex of the SHA-256 of a token's UTF-8 bytes, the only form the
/// store keeps.
fn digest_hex(token: &str) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Identifies one waiting pairing request.
pub type RequestId = String;

/// Identifies one approved device's record ([`DeviceRecord::id`]).
pub type DeviceId = String;

/// What [`PairingBook::request`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requested {
    /// The new request.
    pub id: RequestId,
    /// The same device's earlier request that this one replaced, if any.
    pub superseded: Option<RequestId>,
}

/// Why a pairing step was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PairError {
    /// Too many other devices are already waiting.
    #[error("Too many devices are waiting; try again in a few minutes.")]
    TooManyPending,
    /// The request or its code has lapsed.
    #[error("This request has expired; ask again from the device.")]
    Expired,
    /// The request used up its wrong-code attempts.
    #[error("Too many wrong codes; ask again from the device.")]
    TooManyAttempts,
    /// The code belongs to another device's request.
    #[error("This code is for another device.")]
    NotYours,
    /// The code does not match.
    #[error("That code is not right.")]
    WrongCode,
    /// The request has not been approved yet.
    #[error("This request has not been approved yet.")]
    NotApproved,
    /// No such request.
    #[error("There is no such request.")]
    Unknown,
}

/// One waiting request as the owner's list shows it. Carries no code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingView {
    /// The request's id.
    pub id: RequestId,
    /// The node's `MagicDNS` name, from whois.
    pub name: String,
    /// The Tailscale login, from whois.
    pub login: String,
    /// When the device asked.
    pub asked_at: DateTime<Utc>,
    /// Whether the owner has approved it and its code is out.
    pub approved: bool,
}

struct PairCode {
    value: String,
    expires_at: Instant,
}

impl fmt::Debug for PairCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairCode")
            .field("value", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Debug)]
struct PendingRequest {
    id: RequestId,
    stable_id: String,
    name: String,
    login: String,
    asked_at: DateTime<Utc>,
    expires_at: Instant,
    approved: bool,
    code: Option<PairCode>,
    wrong_attempts: u32,
}

struct CliCode {
    value: String,
    expires_at: Instant,
}

impl fmt::Debug for CliCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CliCode")
            .field("value", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// The pending pairing requests and CLI codes, in memory only.
///
/// Times are injected so the lifetime rules are testable: expiry arithmetic
/// uses a [`std::time::Instant`], and the wall-clock [`DateTime<Utc>`] is kept
/// only for the timestamp the owner's list shows.
#[derive(Debug, Default)]
pub struct PairingBook {
    requests: Vec<PendingRequest>,
    cli_codes: Vec<CliCode>,
    /// When each recent wrong CLI redeem happened, oldest first; only the
    /// last [`CODE_LIFETIME`] counts.
    wrong_cli_redeems: VecDeque<Instant>,
}

impl PairingBook {
    /// An empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a request from `peer`, expiring after [`REQUEST_LIFETIME`].
    ///
    /// A device that already has a request has it replaced; the replaced id
    /// comes back in [`Requested::superseded`] so the caller can tell that
    /// connection. Identity comes only from `peer`, never from the wire.
    /// Lapsed requests are dropped first, so they never count toward the cap.
    ///
    /// # Errors
    ///
    /// [`PairError::TooManyPending`] when [`MAX_PENDING`] other devices are
    /// already waiting.
    pub fn request(
        &mut self,
        peer: &PeerIdentity,
        now: Instant,
        asked_at: DateTime<Utc>,
    ) -> Result<Requested, PairError> {
        self.expire(now);
        let others = self
            .requests
            .iter()
            .filter(|request| request.stable_id != peer.stable_id)
            .count();
        if others >= MAX_PENDING {
            return Err(PairError::TooManyPending);
        }
        let superseded = self.take_for(&peer.stable_id);
        let id = uuid::Uuid::new_v4().to_string();
        self.requests.push(PendingRequest {
            id: id.clone(),
            stable_id: peer.stable_id.clone(),
            name: peer.name.clone(),
            login: peer.login.clone(),
            asked_at,
            expires_at: now + REQUEST_LIFETIME,
            approved: false,
            code: None,
            wrong_attempts: 0,
        });
        Ok(Requested { id, superseded })
    }

    /// Approve `id` and mint the code its device redeems.
    ///
    /// The code expires [`CODE_LIFETIME`] after this call. Approving again
    /// mints a new code and invalidates the previous one.
    ///
    /// # Errors
    ///
    /// [`PairError::Unknown`] for an id the book does not hold, and
    /// [`PairError::Expired`] for a request that has lapsed, which also
    /// removes it.
    pub fn approve(&mut self, id: &str, now: Instant) -> Result<String, PairError> {
        let Some(index) = self.requests.iter().position(|request| request.id == id) else {
            return Err(PairError::Unknown);
        };
        if now >= self.requests[index].expires_at {
            self.requests.remove(index);
            return Err(PairError::Expired);
        }
        let code = crate::secret::generate_pairing_code();
        let expires_at = now + CODE_LIFETIME;
        let request = &mut self.requests[index];
        request.approved = true;
        request.code = Some(PairCode {
            value: code.clone(),
            expires_at,
        });
        request.expires_at = request.expires_at.max(expires_at);
        Ok(code)
    }

    /// Drop the request `id` and any code it holds, returning whether it was
    /// there.
    pub fn deny(&mut self, id: &str) -> bool {
        match self.requests.iter().position(|request| request.id == id) {
            Some(index) => {
                self.requests.remove(index);
                true
            }
            None => false,
        }
    }

    /// Redeem `code` for request `id` on behalf of the device `stable_id`.
    ///
    /// Succeeds only for an approved, unexpired request whose device is
    /// `stable_id` and whose code is unexpired and equal; success consumes
    /// the request, so the code works once.
    ///
    /// # Errors
    ///
    /// [`PairError::Unknown`] for an id the book does not hold,
    /// [`PairError::NotYours`] when the request belongs to another device
    /// (which costs no attempt), [`PairError::NotApproved`] before an approve,
    /// [`PairError::Expired`] when the request or its code has lapsed,
    /// [`PairError::WrongCode`] for a wrong code, and
    /// [`PairError::TooManyAttempts`] on the [`MAX_WRONG_ATTEMPTS`]th wrong
    /// code, which also drops the request.
    pub fn redeem(
        &mut self,
        id: &str,
        stable_id: &str,
        code: &str,
        now: Instant,
    ) -> Result<(), PairError> {
        let Some(index) = self.requests.iter().position(|request| request.id == id) else {
            return Err(PairError::Unknown);
        };
        if self.requests[index].stable_id != stable_id {
            return Err(PairError::NotYours);
        }
        if now >= self.requests[index].expires_at {
            self.requests.remove(index);
            return Err(PairError::Expired);
        }
        let (code_lapsed, matches) = {
            let Some(pair_code) = self.requests[index].code.as_ref() else {
                return Err(PairError::NotApproved);
            };
            (
                now >= pair_code.expires_at,
                crate::secret::constant_time_eq(pair_code.value.as_bytes(), code.as_bytes()),
            )
        };
        if !self.requests[index].approved {
            return Err(PairError::NotApproved);
        }
        if code_lapsed {
            let request = &mut self.requests[index];
            request.approved = false;
            request.code = None;
            return Err(PairError::Expired);
        }
        if !matches {
            let request = &mut self.requests[index];
            request.wrong_attempts += 1;
            if request.wrong_attempts >= MAX_WRONG_ATTEMPTS {
                self.requests.remove(index);
                return Err(PairError::TooManyAttempts);
            }
            return Err(PairError::WrongCode);
        }
        self.requests.remove(index);
        Ok(())
    }

    /// Mint a code any authorized unpaired device may redeem, expiring after
    /// [`CODE_LIFETIME`]. At most [`MAX_CLI_CODES`] live; minting one more
    /// drops the oldest.
    pub fn mint_cli_code(&mut self, now: Instant) -> String {
        self.drop_expired_cli_codes(now);
        if self.cli_codes.len() >= MAX_CLI_CODES {
            self.cli_codes.remove(0);
        }
        let code = crate::secret::generate_pairing_code();
        self.cli_codes.push(CliCode {
            value: code.clone(),
            expires_at: now + CODE_LIFETIME,
        });
        code
    }

    /// Redeem a CLI code for the calling device. The code works once, and any
    /// authorized device may use it since it is bound to no request.
    ///
    /// Wrong codes cost no per-code attempts, but [`MAX_WRONG_CLI_REDEEMS`]
    /// wrong redeems, each within [`CODE_LIFETIME`] of the others, drop every
    /// live CLI code. A correct redeem neither counts as nor clears a wrong
    /// one.
    ///
    /// # Errors
    ///
    /// [`PairError::Expired`] when no CLI code is live, and
    /// [`PairError::WrongCode`] for a code that matches none.
    pub fn redeem_cli_code(&mut self, code: &str, now: Instant) -> Result<(), PairError> {
        self.drop_expired_cli_codes(now);
        let mut hit: Option<usize> = None;
        for (index, live) in self.cli_codes.iter().enumerate() {
            let matches = crate::secret::constant_time_eq(live.value.as_bytes(), code.as_bytes());
            if matches && hit.is_none() {
                hit = Some(index);
            }
        }
        if let Some(index) = hit {
            self.cli_codes.remove(index);
            return Ok(());
        }
        if self.cli_codes.is_empty() {
            return Err(PairError::Expired);
        }
        self.wrong_cli_redeems
            .retain(|at| now.duration_since(*at) <= CODE_LIFETIME);
        self.wrong_cli_redeems.push_back(now);
        if self.wrong_cli_redeems.len() >= MAX_WRONG_CLI_REDEEMS {
            self.cli_codes.clear();
            self.wrong_cli_redeems.clear();
        }
        Err(PairError::WrongCode)
    }

    /// Drop every lapsed request and CLI code, returning the ids of the
    /// requests that lapsed so the caller can tell those devices.
    ///
    /// A request that is still waiting but whose code has lapsed keeps its
    /// place and loses the code, so it shows unapproved again and the owner
    /// can approve it afresh.
    pub fn expire(&mut self, now: Instant) -> Vec<RequestId> {
        self.drop_expired_cli_codes(now);
        let mut lapsed = Vec::new();
        self.requests.retain_mut(|request| {
            if now >= request.expires_at {
                lapsed.push(request.id.clone());
                return false;
            }
            if request
                .code
                .as_ref()
                .is_some_and(|code| now >= code.expires_at)
            {
                request.code = None;
                request.approved = false;
            }
            true
        });
        lapsed
    }

    /// The owner's list: one entry per waiting request, with no code in it.
    #[must_use]
    pub fn pending(&self) -> Vec<PendingView> {
        self.requests
            .iter()
            .map(|request| PendingView {
                id: request.id.clone(),
                name: request.name.clone(),
                login: request.login.clone(),
                asked_at: request.asked_at,
                approved: request.approved,
            })
            .collect()
    }

    /// The device a waiting request `id` came from, by its `StableNodeID`.
    #[must_use]
    pub fn stable_id_of(&self, id: &str) -> Option<&str> {
        self.requests
            .iter()
            .find(|request| request.id == id)
            .map(|request| request.stable_id.as_str())
    }

    fn take_for(&mut self, stable_id: &str) -> Option<RequestId> {
        let index = self
            .requests
            .iter()
            .position(|request| request.stable_id == stable_id)?;
        Some(self.requests.remove(index).id)
    }

    fn drop_expired_cli_codes(&mut self, now: Instant) {
        self.cli_codes.retain(|code| now < code.expires_at);
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use super::{
        CODE_LIFETIME, DeviceStore, MAX_PENDING, PairError, PairingBook, REQUEST_LIFETIME,
    };
    use chrono::Utc;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use tailnet::PeerIdentity;

    /// A scratch config dir holding one `devices.json`, removed on drop.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rt-devices-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Self { dir }
        }

        fn path(&self) -> PathBuf {
            self.dir.join("devices.json")
        }

        fn store(&self) -> DeviceStore {
            DeviceStore::load(&self.path())
        }

        fn text(&self) -> String {
            std::fs::read_to_string(self.path()).expect("read devices.json")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn peer(stable_id: &str, name: &str, login: &str) -> PeerIdentity {
        PeerIdentity {
            stable_id: stable_id.to_owned(),
            name: name.to_owned(),
            user_id: 1,
            login: login.to_owned(),
        }
    }

    fn phone() -> PeerIdentity {
        peer("node-a", "leftos-phone", "lefto@example.com")
    }

    /// A wrong code that is certainly not `code`.
    fn other_code(code: &str) -> String {
        if code == "000000" {
            "000001".to_owned()
        } else {
            "000000".to_owned()
        }
    }

    fn approved_request(book: &mut PairingBook, asked: Instant) -> (String, String) {
        let request = book
            .request(&phone(), asked, Utc::now())
            .expect("request a pairing");
        let code = book.approve(&request.id, asked).expect("approve it");
        (request.id, code)
    }

    /// The v0.5.1-alpha `devices.json` shape: two approved devices, the second
    /// one blocked. A device's token hash and its revoked flag must read back
    /// exactly as that release wrote them, since a lost hash revokes a device
    /// and a lost flag unblocks one.
    #[test]
    fn devices_json_written_by_v0_5_1_alpha_keeps_its_hashes_and_revoked_flags() {
        let scratch = Scratch::new("v051-shape");
        let json = r#"{
            "devices": [
                {
                    "id": "dev-1",
                    "stable_id": "node-aaaa1111",
                    "name": "leftos-phone",
                    "login": "lefto@example.com",
                    "token_sha256": "9f2c1d3e4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0",
                    "created_at": "2025-01-02T03:04:05Z",
                    "last_seen": "2025-02-03T04:05:06Z",
                    "revoked": false
                },
                {
                    "id": "dev-2",
                    "stable_id": "node-bbbb2222",
                    "name": "leftos-laptop",
                    "login": "lefto@example.com",
                    "token_sha256": "0a1b2c3d4e5f60718293a4b5c6d7e8f91a2b3c4d5e6f708192a3b4c5d6e7f809",
                    "created_at": "2025-01-03T05:06:07Z",
                    "last_seen": null,
                    "revoked": true
                }
            ]
        }"#;
        std::fs::write(scratch.path(), json).expect("write the v0.5.1 devices.json");

        let store = scratch.store();

        assert!(!store.is_corrupt(), "the released file decodes");
        let devices = store.list();
        assert_eq!(devices.len(), 2, "both records load");
        assert_eq!(
            devices[0].token_sha256,
            "9f2c1d3e4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0",
            "the approved device's hash is kept"
        );
        assert!(!devices[0].revoked, "an approved device stays approved");
        assert_eq!(devices[0].name, "leftos-phone");
        assert_eq!(devices[0].login, "lefto@example.com");
        assert_eq!(
            devices[0].last_seen.map(|at| at.to_rfc3339()),
            Some("2025-02-03T04:05:06+00:00".to_owned())
        );
        assert_eq!(
            devices[1].token_sha256,
            "0a1b2c3d4e5f60718293a4b5c6d7e8f91a2b3c4d5e6f708192a3b4c5d6e7f809",
            "the blocked device's hash is kept"
        );
        assert!(devices[1].revoked, "a blocked device stays blocked");
        assert_eq!(
            devices[1].last_seen, None,
            "a device that never reconnected keeps no last-seen time"
        );
    }

    #[test]
    fn issued_token_is_stored_only_as_its_hash() {
        let scratch = Scratch::new("hash");
        let mut store = scratch.store();
        let (id, token) = store
            .issue(&phone(), Utc::now())
            .expect("issue a device token");
        let text = scratch.text();
        assert!(
            !text.contains(&token),
            "the plaintext token is never written: {text}"
        );
        assert!(
            text.contains(&super::digest_hex(&token)),
            "the record holds the hex digest: {text}"
        );
        assert!(text.contains(&id), "the record id is written: {text}");
    }

    #[test]
    fn verify_accepts_the_issued_token_for_its_device_only() {
        let scratch = Scratch::new("verify");
        let mut store = scratch.store();
        let (id, token) = store
            .issue(&phone(), Utc::now())
            .expect("issue a device token");
        let record = store
            .verify("node-a", &token)
            .expect("the issued token verifies");
        assert_eq!(record.id, id);
        assert!(
            store.verify("node-a", "not-the-token").is_none(),
            "another token does not verify"
        );
    }

    #[test]
    fn verify_refuses_a_token_presented_by_another_stable_id() {
        let scratch = Scratch::new("other-node");
        let mut store = scratch.store();
        let (_, token) = store
            .issue(&phone(), Utc::now())
            .expect("issue a device token");
        assert!(
            store.verify("node-b", &token).is_none(),
            "a token copied to another node fails"
        );
    }

    #[test]
    fn verify_refuses_a_revoked_device() {
        let scratch = Scratch::new("revoked");
        let mut store = scratch.store();
        let (id, token) = store
            .issue(&phone(), Utc::now())
            .expect("issue a device token");
        assert!(store.revoke(&id).expect("revoke"), "the record exists");
        assert!(
            store.verify("node-a", &token).is_none(),
            "a revoked record never verifies"
        );
        assert_eq!(store.list().len(), 1, "the record stays for the block list");
        assert!(store.list()[0].revoked);
    }

    #[test]
    fn reissue_for_the_same_device_replaces_its_token() {
        let scratch = Scratch::new("reissue");
        let mut store = scratch.store();
        let (_, first) = store.issue(&phone(), Utc::now()).expect("first issue");
        let (_, second) = store.issue(&phone(), Utc::now()).expect("second issue");
        assert!(
            store.verify("node-a", &first).is_none(),
            "the replaced token stops verifying"
        );
        assert!(
            store.verify("node-a", &second).is_some(),
            "the new token verifies"
        );
        assert_eq!(store.list().len(), 1, "one device holds one record");
    }

    #[test]
    fn issue_refuses_a_revoked_device() {
        let scratch = Scratch::new("issue-blocked");
        let mut store = scratch.store();
        let (id, token) = store.issue(&phone(), Utc::now()).expect("first issue");
        assert!(store.revoke(&id).expect("revoke"), "the record exists");
        let text = scratch.text();

        let refused = store
            .issue(&phone(), Utc::now())
            .expect_err("a blocked device gets no new token");
        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(store.list().len(), 1, "the blocked record stays");
        assert!(store.list()[0].revoked, "and stays blocked");
        assert!(store.verify("node-a", &token).is_none());
        assert_eq!(scratch.text(), text, "nothing was written");

        assert!(store.forget(&id).expect("forget"), "the record exists");
        let (_, fresh) = store
            .issue(&phone(), Utc::now())
            .expect("a forgotten device pairs again");
        assert!(store.verify("node-a", &fresh).is_some());
    }

    #[test]
    fn corrupt_store_validates_nothing_and_is_not_overwritten() {
        let scratch = Scratch::new("corrupt");
        let bytes = b"{ this is not json".to_vec();
        std::fs::write(scratch.path(), &bytes).expect("write a corrupt store");
        let mut store = scratch.store();
        assert!(store.is_corrupt(), "the file could not be decoded");
        assert!(
            store.verify("node-a", "any-token").is_none(),
            "a corrupt store validates nothing"
        );
        assert!(
            store.issue(&phone(), Utc::now()).is_err(),
            "a corrupt store refuses to save"
        );
        assert_eq!(
            std::fs::read(scratch.path()).expect("read the store"),
            bytes,
            "the corrupt file is left exactly as it is"
        );
    }

    #[test]
    fn store_survives_reload() {
        let scratch = Scratch::new("reload");
        let (id, token) = scratch
            .store()
            .issue(&phone(), Utc::now())
            .expect("issue a device token");
        let reloaded = scratch.store();
        let record = reloaded
            .verify("node-a", &token)
            .expect("the token verifies after a reload");
        assert_eq!(record.id, id);
        assert!(!record.revoked);
        assert!(record.last_seen.is_none());
    }

    #[test]
    fn record_debug_redacts_the_hash() {
        let scratch = Scratch::new("debug");
        let mut store = scratch.store();
        let (_, token) = store.issue(&phone(), Utc::now()).expect("issue");
        let record = store.verify("node-a", &token).expect("verify");
        let printed = format!("{record:?}");
        assert!(
            printed.contains("<redacted>"),
            "the digest is redacted: {printed}"
        );
        assert!(
            !printed.contains(&super::digest_hex(&token)),
            "the digest does not print: {printed}"
        );
    }

    #[test]
    fn pairing_code_is_one_time() {
        let mut book = PairingBook::new();
        let now = Instant::now();
        let (id, code) = approved_request(&mut book, now);
        book.redeem(&id, "node-a", &code, now).expect("redeem");
        assert_eq!(
            book.redeem(&id, "node-a", &code, now),
            Err(PairError::Unknown),
            "the request is consumed"
        );
        assert!(book.pending().is_empty(), "the request is gone");
    }

    #[test]
    fn pairing_code_expires() {
        let mut book = PairingBook::new();
        let now = Instant::now();
        let (id, code) = approved_request(&mut book, now);
        assert_eq!(
            book.redeem(&id, "node-a", &code, now + CODE_LIFETIME),
            Err(PairError::Expired),
            "the code lapses CODE_LIFETIME after the approve"
        );
    }

    #[test]
    fn pairing_code_dies_after_five_wrong_attempts() {
        let mut book = PairingBook::new();
        let now = Instant::now();
        let (id, code) = approved_request(&mut book, now);
        let wrong = other_code(&code);
        for attempt in 1..super::MAX_WRONG_ATTEMPTS {
            assert_eq!(
                book.redeem(&id, "node-a", &wrong, now),
                Err(PairError::WrongCode),
                "attempt {attempt}"
            );
            assert_eq!(book.pending().len(), 1, "the request survives {attempt}");
        }
        assert_eq!(
            book.redeem(&id, "node-a", &wrong, now),
            Err(PairError::TooManyAttempts),
            "the fifth wrong code drops the request"
        );
        assert!(book.pending().is_empty(), "the request is gone");
        assert_eq!(
            book.redeem(&id, "node-a", &code, now),
            Err(PairError::Unknown),
            "the right code gets nothing afterwards"
        );
    }

    #[test]
    fn code_redeems_only_for_its_requests_stable_id() {
        let mut book = PairingBook::new();
        let now = Instant::now();
        let (id, code) = approved_request(&mut book, now);
        for _ in 0..super::MAX_WRONG_ATTEMPTS {
            assert_eq!(
                book.redeem(&id, "node-b", &code, now),
                Err(PairError::NotYours),
                "another device's token cannot redeem it"
            );
        }
        book.redeem(&id, "node-a", &code, now)
            .expect("a wrong stable_id costs no attempt, so the code still works");
    }

    #[test]
    fn redeem_before_approve_fails() {
        let mut book = PairingBook::new();
        let now = Instant::now();
        let request = book
            .request(&phone(), now, Utc::now())
            .expect("request a pairing");
        assert_eq!(
            book.redeem(&request.id, "node-a", "123456", now),
            Err(PairError::NotApproved)
        );
    }

    #[test]
    fn second_approve_invalidates_the_first_code() {
        let now = Instant::now();
        let (mut book, id, first, second) = loop {
            let mut book = PairingBook::new();
            let request = book
                .request(&phone(), now, Utc::now())
                .expect("request a pairing");
            let first = book.approve(&request.id, now).expect("approve");
            let second = book.approve(&request.id, now).expect("re-approve");
            if first != second {
                break (book, request.id, first, second);
            }
            // Six random digits collide one time in a million, and the check
            // below says nothing when they do, so draw again.
        };
        assert_eq!(
            book.redeem(&id, "node-a", &first, now),
            Err(PairError::WrongCode),
            "the first code is dead"
        );
        book.redeem(&id, "node-a", &second, now)
            .expect("the second code works");
    }

    #[test]
    fn deny_removes_the_request_and_code() {
        let mut book = PairingBook::new();
        let now = Instant::now();
        let (id, code) = approved_request(&mut book, now);
        assert!(book.deny(&id), "the request was there");
        assert!(!book.deny(&id), "a second deny finds nothing");
        assert!(book.pending().is_empty(), "the request is gone");
        assert_eq!(
            book.redeem(&id, "node-a", &code, now),
            Err(PairError::Unknown),
            "the code dies with the request"
        );
    }

    #[test]
    fn second_request_from_the_same_device_supersedes_the_first() {
        let mut book = PairingBook::new();
        let now = Instant::now();
        let first = book
            .request(&phone(), now, Utc::now())
            .expect("first request");
        assert_eq!(first.superseded, None);
        let second = book
            .request(&phone(), now, Utc::now())
            .expect("second request");
        assert_eq!(
            second.superseded,
            Some(first.id.clone()),
            "the caller gets the replaced id back"
        );
        assert_eq!(book.pending().len(), 1, "one request per device");
        assert_eq!(book.pending()[0].id, second.id);
        assert_eq!(
            book.approve(&first.id, now),
            Err(PairError::Unknown),
            "the superseded request is gone"
        );
    }

    #[test]
    fn pending_cap_refuses_the_ninth_request() {
        let mut book = PairingBook::new();
        let now = Instant::now();
        for n in 0..MAX_PENDING {
            book.request(
                &peer(&format!("node-{n}"), "phone", "lefto@example.com"),
                now,
                Utc::now(),
            )
            .expect("a request within the cap");
        }
        assert_eq!(
            book.request(
                &peer("node-late", "phone", "lefto@example.com"),
                now,
                Utc::now()
            ),
            Err(PairError::TooManyPending),
            "the ninth device is refused"
        );
        book.request(
            &peer("node-0", "phone", "lefto@example.com"),
            now,
            Utc::now(),
        )
        .expect("a device that already waits may ask again");
    }

    #[test]
    fn approving_an_expired_request_fails() {
        let mut book = PairingBook::new();
        let now = Instant::now();
        let request = book
            .request(&phone(), now, Utc::now())
            .expect("request a pairing");
        assert_eq!(
            book.approve(&request.id, now + REQUEST_LIFETIME),
            Err(PairError::Expired)
        );
        assert!(book.pending().is_empty(), "the lapsed request is dropped");
    }

    #[test]
    fn cli_code_redeems_once() {
        let mut book = PairingBook::new();
        let now = Instant::now();
        let code = book.mint_cli_code(now);
        book.redeem_cli_code(&code, now).expect("redeem the code");
        assert_eq!(
            book.redeem_cli_code(&code, now),
            Err(PairError::Expired),
            "the code is consumed"
        );
    }

    #[test]
    fn cli_code_expires() {
        let mut book = PairingBook::new();
        let now = Instant::now();
        let code = book.mint_cli_code(now);
        assert_eq!(
            book.redeem_cli_code(&code, now + CODE_LIFETIME),
            Err(PairError::Expired)
        );
    }

    #[test]
    fn codes_are_never_written_to_disk() {
        let scratch = Scratch::new("no-code-on-disk");
        let mut store = scratch.store();
        store
            .issue(&phone(), Utc::now())
            .expect("issue a device token");
        let mut book = PairingBook::new();
        let now = Instant::now();
        let (_, code) = approved_request(&mut book, now);
        let cli = book.mint_cli_code(now);
        let text = scratch.text();
        assert!(!text.contains(&code), "the pairing code is memory only");
        assert!(!text.contains(&cli), "the CLI code is memory only");
    }

    #[test]
    fn pending_view_carries_no_code() {
        let mut book = PairingBook::new();
        let now = Instant::now();
        let (_, code) = approved_request(&mut book, now);
        let view = book.pending();
        assert_eq!(view.len(), 1);
        assert!(view[0].approved);
        assert_eq!(view[0].name, "leftos-phone");
        assert_eq!(view[0].login, "lefto@example.com");
        let printed = format!("{view:?}");
        assert!(
            !printed.contains(&code),
            "the owner's list never shows a code: {printed}"
        );
    }

    #[test]
    fn cli_lockout_is_a_rolling_window() {
        let mut book = PairingBook::new();
        let t0 = Instant::now();
        let at = |millis: u64| t0 + Duration::from_millis(millis);
        let first = book.mint_cli_code(t0);
        let wrong = other_code(&first);
        assert_eq!(
            book.redeem_cli_code(&wrong, at(0)),
            Err(PairError::WrongCode)
        );
        for _ in 0..3 {
            assert_eq!(
                book.redeem_cli_code(&wrong, at(119_000)),
                Err(PairError::WrongCode)
            );
        }
        // Minted late enough that the check at 2:00.5 still finds it live.
        let live = book.mint_cli_code(at(119_500));
        let wrong = other_code(&live);
        assert_eq!(
            book.redeem_cli_code(&wrong, at(120_500)),
            Err(PairError::WrongCode)
        );
        assert_eq!(
            book.redeem_cli_code(&wrong, at(120_500)),
            Err(PairError::WrongCode),
            "the fifth wrong redeem within two minutes"
        );
        assert_eq!(
            book.redeem_cli_code(&live, at(120_500)),
            Err(PairError::Expired),
            "the live CLI code was dropped"
        );
    }

    #[test]
    fn a_correct_cli_redeem_does_not_reset_the_lockout() {
        let mut book = PairingBook::new();
        let t0 = Instant::now();
        let first = book.mint_cli_code(t0);
        let wrong = other_code(&first);
        for _ in 0..4 {
            assert_eq!(book.redeem_cli_code(&wrong, t0), Err(PairError::WrongCode));
        }
        let live = loop {
            let candidate = book.mint_cli_code(t0);
            if candidate != wrong {
                break candidate;
            }
        };
        book.redeem_cli_code(&first, t0)
            .expect("the right code still redeems");
        assert_eq!(
            book.redeem_cli_code(&wrong, t0),
            Err(PairError::WrongCode),
            "the fifth wrong redeem inside the window"
        );
        assert_eq!(
            book.redeem_cli_code(&live, t0),
            Err(PairError::Expired),
            "a correct redeem must not reset the lockout"
        );
    }

    #[test]
    fn an_unreadable_store_is_corrupt_not_empty() {
        let scratch = Scratch::new("unreadable");
        let path = scratch.path();
        std::fs::create_dir_all(&path).expect("make a directory where the store goes");
        let mut store = DeviceStore::load(&path);
        assert!(
            store.is_corrupt(),
            "an unreadable store is corrupt, not empty"
        );
        assert!(
            store.issue(&phone(), Utc::now()).is_err(),
            "and it refuses to save over it"
        );
    }

    #[test]
    fn approve_near_the_request_deadline_keeps_the_code_two_minutes() {
        let mut book = PairingBook::new();
        let t0 = Instant::now();
        let request = book
            .request(&phone(), t0, Utc::now())
            .expect("request a pairing");
        let code = book
            .approve(&request.id, t0 + Duration::from_secs(298))
            .expect("approve at 4:58");
        book.redeem(&request.id, "node-a", &code, t0 + Duration::from_secs(360))
            .expect("the code outlives the request's own deadline");
    }

    #[test]
    fn expire_unapproves_a_request_whose_code_lapsed() {
        let mut book = PairingBook::new();
        let t0 = Instant::now();
        let (id, code) = approved_request(&mut book, t0);
        let lapsed = book.expire(t0 + CODE_LIFETIME);
        assert!(lapsed.is_empty(), "the request itself has not lapsed");
        let view = book.pending();
        assert_eq!(view.len(), 1);
        assert!(
            !view[0].approved,
            "a lapsed code leaves the request unapproved"
        );
        assert_eq!(
            book.redeem(&id, "node-a", &code, t0 + CODE_LIFETIME),
            Err(PairError::NotApproved),
            "and the lapsed code is gone"
        );
    }

    #[test]
    fn lapsed_requests_do_not_count_toward_the_cap() {
        let mut book = PairingBook::new();
        let t0 = Instant::now();
        for n in 0..MAX_PENDING {
            book.request(
                &peer(&format!("node-{n}"), "phone", "lefto@example.com"),
                t0,
                Utc::now(),
            )
            .expect("a request within the cap");
        }
        book.request(
            &peer("node-late", "phone", "lefto@example.com"),
            t0 + REQUEST_LIFETIME,
            Utc::now(),
        )
        .expect("the eight earlier requests have lapsed");
    }
}
