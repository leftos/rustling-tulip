//! The account Claude Code is logged in as, fetched and refreshed as the
//! active account, under Claude Code's own locks.
//!
//! An unexpired live token is fetched as it is; when the server accepts it and its lineage differs from the backup's,
//! the backup is resynced once the profile oracle attributes the lineage to the account (Resync R). An expired token,
//! or one the server answered 401, is refreshed in Claude Code's own order: the store's consume lock, its account lock,
//! then Claude Code's two credential locks; the live file and the backup are re-read under them, a rotation Claude Code
//! already made is adopted without a POST, so is a successor an earlier pass left in the unclaimed stash, a backup that
//! already holds a fresh credential is restored without one, and otherwise the chosen grant is spent once and the
//! successor written to the backup (or, when that fails, to the unclaimed stash) and then to the live file, still
//! under the credential locks. Only the live file's `claudeAiOauth` is replaced, and before a restore or a spend
//! overwrites a live credential that is not the account's, that credential is stashed (an adopted stashed successor
//! only ever replaces the account's own lineage). Claude Code's config lock is taken only to clear `primaryApiKey`.
//! Every refusal before the POST defers to the next pass; nothing is spent that cannot be attributed or kept.

use std::cell::Cell;

use serde_json::Value;

use crate::claude_files::{
    ClaudeFileError, clear_primary_api_key, read_live_credentials, write_live_credentials,
};
use crate::locks::{
    AccountLock, AccountLockGuard, ClaudeCodeLocks, ClaudeCredentialsGuard, ConsumeLock,
};
use crate::poll::{
    Attempt, DeferReason, FetchError, FetchResult, Persisted, PollResult, PrepaidProbe,
    fetch_reading, fetch_usage, live_identity, read_prepaid_beside,
};
use crate::refresh::oracle::{LineageKey, Verdict, resolve, resolved_matches_slot_identity};
use crate::refresh::{
    GrantError, OauthShape, RefreshContext, RefreshError, RefreshedGrant, Slot, StashReason, copy,
    credential_fingerprint, has_refresh_token, newer_generation, refresh_grant_within, scrub,
    splice_oauth, stash,
};
use crate::secret::{AccessToken, CredentialBlob};
use crate::store::AccountRecord;
use crate::transport::{LOCKED_REFRESH_TIMEOUT, UsageTransport};
use crate::usage::Identity;

/// Fetches the logged-in account's usage with `creds`, the live credential the poll read, refreshing it under Claude
/// Code's locks when it has expired or the server refused it. Returns the attempt and what it wrote: when a credential
/// was made live, whether `primaryApiKey` is gone from `~/.claude.json` (`Some(false)` when clearing it failed), when
/// the backup was written, whether that write succeeded, and when the live write failed, whether its temp file was left.
/// The fetch that brings a reading reads the prepaid balance beside it when `prepaid` is due, with the same token.
pub(super) fn fetch<T: UsageTransport + ?Sized>(
    transport: &T,
    context: &RefreshContext<'_>,
    account: &AccountRecord,
    creds: &CredentialBlob,
    prepaid: &PrepaidProbe<'_>,
) -> (Attempt, Persisted) {
    let mut active = Active::new(context, account, creds, prepaid);
    let (attempt, primary_key_cleared) = active.attempt(transport);
    let persisted = Persisted {
        primary_key_cleared,
        backup_written: active.backup_written.get(),
        live_temp_left: active.live_temp_left.get(),
    };
    (attempt, persisted)
}

/// The logged-in account in one pass: its record, the live credential the poll read, the 401 that sent an unexpired
/// token to the refresh, if one did, whether a backup write the pass attempted succeeded, and, when its live write
/// failed, whether that write's temp file was left on disk.
struct Active<'a, 'c> {
    context: &'a RefreshContext<'c>,
    account: &'a AccountRecord,
    creds: &'a CredentialBlob,
    prepaid: &'a PrepaidProbe<'a>,
    slot: Slot<'a, 'c>,
    force: Option<FetchError>,
    backup_written: Cell<Option<bool>>,
    live_temp_left: Cell<Option<bool>>,
}

/// The account's backup as the refresh read it.
struct Backup {
    blob: Option<CredentialBlob>,
    fingerprint: Option<String>,
    /// It holds a refresh token, whatever its access token: a stored successor a degenerate grant left with an empty
    /// access token is still spent by its refresh token.
    refreshable: bool,
}

impl Backup {
    /// The backup when its grant can be spent.
    fn spendable_blob(&self) -> Option<&CredentialBlob> {
        self.blob.as_ref().filter(|_| self.refreshable)
    }

    /// Whether `blob` is the backup, byte for byte.
    fn is(&self, blob: &CredentialBlob) -> bool {
        self.blob
            .as_ref()
            .is_some_and(|backup| backup.expose() == blob.expose())
    }
}

/// The file a grant was chosen from, re-read after an `invalid_grant` to tell a raced lineage from a dead one. The
/// poll's own read is the live file's.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Live,
    Backup,
}

/// What the refresh holds once its lock waits are over: the store's account lock and Claude Code's two credential locks,
/// and the live file and the account's backup as they were read under them.
struct Held<'h> {
    lock: &'h AccountLockGuard,
    claude: &'h ClaudeCredentialsGuard,
    backup: &'h Backup,
    live: &'h CredentialBlob,
}

/// The credential the account now works with, and whether `primaryApiKey` was cleared when it was made live.
struct Locked {
    working: CredentialBlob,
    cleared: Option<bool>,
}

impl<'a, 'c> Active<'a, 'c> {
    fn new(
        context: &'a RefreshContext<'c>,
        account: &'a AccountRecord,
        creds: &'a CredentialBlob,
        prepaid: &'a PrepaidProbe<'a>,
    ) -> Self {
        Self {
            context,
            account,
            creds,
            prepaid,
            slot: Slot::new(context, account.number),
            force: None,
            backup_written: Cell::new(None),
            live_temp_left: Cell::new(None),
        }
    }

    /// The pass: the unexpired live token is fetched as it is, and an expired or refused one refreshed first. Returns
    /// the attempt and, when a credential was made live, whether `primaryApiKey` was cleared.
    fn attempt<T: UsageTransport + ?Sized>(&mut self, transport: &T) -> (Attempt, Option<bool>) {
        let Some(oauth) = self.creds.oauth() else {
            // The poll hands over only a live credential with an OAuth block; kept so a caller that does not is refused.
            return (Attempt::Release(PollResult::NoCredentials), None);
        };
        if !oauth.is_expired((self.context.clock)()) {
            match fetch_usage(transport, oauth.access_token()) {
                Err(error @ FetchError::Http { status: 401, .. }) => self.force = Some(error),
                result => return (self.serve(transport, oauth.access_token(), result), None),
            }
        }
        match self.recover() {
            Ok(locked) => (
                self.fetch_working(transport, &locked.working),
                locked.cleared,
            ),
            Err(attempt) => (attempt, None),
        }
    }

    /// The result of a fetch with the unexpired live token: the backup is resynced after a reading, and a reading
    /// served off a lineage the oracle named as another account's is [`PollResult::ForeignCredential`]. Only a reading
    /// kept as the account's has its prepaid balance read beside it, with `token`, the one the fetch used.
    fn serve<T: UsageTransport + ?Sized>(
        &self,
        transport: &T,
        token: &AccessToken,
        result: FetchResult,
    ) -> Attempt {
        if matches!(result, Ok(Some(_))) {
            self.resync();
            if self.verdict(credential_fingerprint(self.creds).as_deref()) == Some(false) {
                return Attempt::Release(PollResult::ForeignCredential);
            }
        }
        read_prepaid_beside(transport, token, self.prepaid, &result);
        Attempt::Fetch(result)
    }

    /// A refusal before any grant is spent: the 401 that sent the token here when one did, else
    /// [`PollResult::Deferred`], which records nothing.
    fn defer(&self, reason: DeferReason) -> Attempt {
        match self.force {
            Some(first) => Attempt::Fetch(Err(first)),
            None => Attempt::Release(PollResult::Deferred(reason)),
        }
    }

    /// The stop drain refused the refresh before its POST: the 401 that sent the token here when one did, as the gate
    /// path keeps it, else [`PollResult::Draining`].
    fn draining(&self) -> Attempt {
        match self.force {
            Some(first) => Attempt::Fetch(Err(first)),
            None => Attempt::Release(PollResult::Draining),
        }
    }

    /// A refresh that bought nothing: recorded as `refresh-failed`, showing the 401 when one started it.
    fn refresh_failed(&self) -> Attempt {
        Attempt::Refused {
            kind: "refresh-failed",
            result: self
                .force
                .map_or(PollResult::TokenExpired, PollResult::Failed),
        }
    }

    /// The refresh, up to the credential the account now works with.
    fn recover(&self) -> Result<Locked, Attempt> {
        if self.context.securestorage_dir_set {
            return Err(Attempt::Refused {
                kind: GrantError::StoreUnmirrored.kind(),
                result: PollResult::StoreUnmirrored,
            });
        }
        let backup = self.read_backup()?;
        let attributable =
            backup.is(self.creds) || credential_fingerprint(self.creds) == backup.fingerprint;
        if !attributable && !backup.refreshable {
            return Err(self.defer(DeferReason::Unattributable));
        }
        let root = self.context.store.root();
        let _consume = ConsumeLock::acquire(root, self.account.number)
            .map_err(|_| self.defer(DeferReason::LocksBusy))?;
        let lock = AccountLock::acquire(root).map_err(|_| self.defer(DeferReason::LocksBusy))?;
        let claude =
            ClaudeCodeLocks::credentials_with(self.context.claude, self.context.claude_locks)
                .map_err(|_| self.defer(DeferReason::LocksBusy))?;
        self.locked(&lock, &claude)
    }

    /// The account's backup as it stands now. An unreadable one defers: nothing can be attributed against it.
    fn read_backup(&self) -> Result<Backup, Attempt> {
        let blob = self
            .context
            .store
            .read_credentials(self.account.number)
            .map_err(|_| self.defer(DeferReason::BackupUnreadable))?;
        Ok(Backup {
            fingerprint: blob.as_ref().and_then(credential_fingerprint),
            refreshable: blob.as_ref().is_some_and(has_refresh_token),
            blob,
        })
    }

    /// Under the consume, account and Claude Code credential locks: re-read the live file, check the identity, re-read
    /// the backup (a writer may have replaced it since the read before the locks, and every decision below uses this
    /// read), then adopt, restore or spend, and clear `primaryApiKey` once a credential was made live.
    ///
    /// The stop drain's guard is taken only now, with every lock held, just before the POST, and released before the
    /// config-lock wait of the `primaryApiKey` clear, so the server's bounded drain never waits out a lock wait. A
    /// foreign live credential is stashed before the guard is taken, so the guard covers the POST (6 s) and the
    /// successor's writes, of which only the one stash of a successor the backup refused may wait 10 s for its manifest
    /// lock: about 16 s at worst (the store's retries of a transient failure add under a second per file operation).
    fn locked(
        &self,
        lock: &AccountLockGuard,
        claude: &ClaudeCredentialsGuard,
    ) -> Result<Locked, Attempt> {
        let live = self
            .read_live()
            .ok_or_else(|| self.defer(DeferReason::LiveUnreadable))?;
        if live_identity(self.context.claude) != Some(Identity::of(self.account)) {
            return Err(self.defer(DeferReason::IdentityMismatch));
        }
        let backup = self.read_backup()?;
        let held = Held {
            lock,
            claude,
            backup: &backup,
            live: &live,
        };
        if let Some(adopted) = self.adopt(&held)? {
            return Ok(adopted);
        }
        let working = self.made_live(&held)?;
        Ok(Locked {
            working,
            cleared: Some(self.clear_primary_key()),
        })
    }

    /// The credential made live: a successor an earlier pass stashed, the backup when it already holds a fresh
    /// credential, or the successor of the chosen grant, spent once. A pass that will spend checks first that the store
    /// takes writes and the stop drain has not begun, so a pass that defers stashes nothing. Then a live credential
    /// that is not the account's lineage is stashed before the restore or the spend, outside the stop drain's guard;
    /// when that stash fails the pass defers with nothing spent, since its bytes may be another login's only copy.
    fn made_live(&self, held: &Held<'_>) -> Result<CredentialBlob, Attempt> {
        if let Some(successor) = self.adopt_stashed(held)? {
            return self.make_live(held, successor, None);
        }
        let (input, source) = self.choose_source(held.live, held.backup)?;
        let fresh = input
            .oauth()
            .is_some_and(|oauth| !oauth.is_expired((self.context.clock)()));
        // The backup already holds a fresh credential a refresh persisted when its live write failed: restore it.
        let restore = source == Source::Backup && self.force.is_none() && fresh;
        if !restore {
            self.may_spend()?;
        }
        if !self.owns(held.live, held.backup)
            && self
                .slot
                .stash_foreign(held.live, &self.account.org_uuid)
                .is_err()
        {
            return Err(self.defer(DeferReason::StashUnwritable));
        }
        if restore {
            return self.make_live(held, copy(input), None);
        }
        self.spend(held, input, source)
    }

    /// Whether a grant may be spent now: not when the store would refuse the successor (it would be left in the live
    /// file alone), nor once the stop drain has begun. [`Self::spend`]'s guard stays the authority on the drain.
    fn may_spend(&self) -> Result<(), Attempt> {
        if self.context.store.write_refusal().is_some() {
            return Err(self.defer(DeferReason::StoreUnwritable));
        }
        if self.context.drain.is_draining() {
            return Err(self.draining());
        }
        Ok(())
    }

    /// Spends `input`'s grant once under the stop drain's guard and makes the successor live; the guard is released
    /// before the caller's `primaryApiKey` clear.
    fn spend(
        &self,
        held: &Held<'_>,
        input: &CredentialBlob,
        source: Source,
    ) -> Result<CredentialBlob, Attempt> {
        let Ok(work) = self.context.drain.begin() else {
            return Err(self.draining());
        };
        let working = self.post(input, source)?;
        let made = self.make_live(held, working, Some(input));
        drop(work);
        made
    }

    /// A rotation Claude Code made since the poll read the live file: a fresh, full pair that differs from that read
    /// is adopted without a POST, and written to the backup when its lineage is the backup's or the oracle's and it is
    /// a newer generation than the backup ([`newer_generation`]).
    fn adopt(&self, held: &Held<'_>) -> Result<Option<Locked>, Attempt> {
        let live = held.live;
        let full = OauthShape::of(live).is_some_and(OauthShape::full_pair);
        let fresh = live
            .oauth()
            .is_some_and(|oauth| !oauth.is_expired((self.context.clock)()));
        if live.expose() == self.creds.expose() || !full || !fresh {
            return Ok(None);
        }
        let live_fp = credential_fingerprint(live);
        let verdict = self.verdict(live_fp.as_deref());
        if verdict == Some(false) {
            return Err(Attempt::Release(PollResult::ForeignCredential));
        }
        let newer = held
            .backup
            .blob
            .as_ref()
            .is_none_or(|stored| newer_generation(live, stored));
        if newer && (verdict == Some(true) || live_fp == held.backup.fingerprint) {
            // A failed backup write is not fatal: the adopted credential is served, the failure is reported
            // on the outcome, and a later pass's resync writes the backup.
            self.backup_written
                .set(Some(self.slot.write(held.lock, live).is_ok()));
        }
        Ok(Some(Locked {
            working: copy(live),
            cleared: None,
        }))
    }

    /// A successor an earlier pass parked in the unclaimed stash after spending the live generation (its backup write
    /// failed), adopted here on the active path as the inactive refresh adopts it: when the live file holds the
    /// backup's generation and a stash row names that generation as spent, the row's successor is written to the backup
    /// and returned, and nothing is posted. A stash that cannot be read, or a successor the backup refuses, defers: the
    /// live generation may already be spent.
    fn adopt_stashed(&self, held: &Held<'_>) -> Result<Option<CredentialBlob>, Attempt> {
        let live_fp = credential_fingerprint(held.live);
        if live_fp.is_none() || live_fp != held.backup.fingerprint {
            return Ok(None);
        }
        match stash::adopt(&self.slot, held.lock, Some(held.live)) {
            Ok(successor) => {
                if successor.is_some() {
                    self.backup_written.set(Some(true));
                }
                Ok(successor)
            }
            Err(GrantError::StashUnreadable) => Err(self.defer(DeferReason::StashUnreadable)),
            Err(_) => {
                // The successor was read and the backup refused it; its row stays for the next pass.
                self.backup_written.set(Some(false));
                Err(self.defer(DeferReason::StoreUnwritable))
            }
        }
    }

    /// The credential whose grant may be spent: the live file when it is the backup's lineage (and has a refresh
    /// token), the backup, when it holds a refresh token, if the live file was cleared or is an older generation, the
    /// poll's read as the last resort. A stored successor with no access token is spent by its refresh token, not the
    /// spent live predecessor, while the live copy's `expiresAt` is no later than the successor's stand-in expiry
    /// (which makes it no newer a generation). A newer live generation outside the
    /// backup's lineage is spent only on a `true` verdict; live bytes that moved since the poll read them, to another
    /// lineage, defer. Returns the credential with the file it came from.
    fn choose_source<'b>(
        &'b self,
        live: &'b CredentialBlob,
        backup: &'b Backup,
    ) -> Result<(&'b CredentialBlob, Source), Attempt> {
        let fallback = backup
            .spendable_blob()
            .map_or((self.creds, Source::Live), |blob| (blob, Source::Backup));
        let live_shape = OauthShape::of(live);
        let live_fp = credential_fingerprint(live);
        if live_shape.is_some() && live_fp == backup.fingerprint {
            return Ok(if live_shape.is_some_and(|shape| shape.refresh) {
                (live, Source::Live)
            } else {
                fallback
            });
        }
        if live.expose().is_empty() {
            return Ok(fallback);
        }
        if live.expose() != self.creds.expose() {
            return Err(self.defer(DeferReason::LiveMoved));
        }
        let newer = backup
            .spendable_blob()
            .is_none_or(|stored| newer_generation(live, stored));
        if !(live_shape.is_some_and(|shape| shape.refresh) && newer) {
            return Ok(fallback);
        }
        if self.verdict(live_fp.as_deref()) == Some(true) {
            Ok((live, Source::Live))
        } else {
            Err(self.defer(DeferReason::Unattributable))
        }
    }

    /// Spends `input`'s grant once. A dead lineage strikes the generation posted, unless its source moved on during
    /// the POST (then a writer raced it, and the outcome is `refresh-failed`); any other failure is `refresh-failed`.
    /// A grant the endpoint honoured under this identity is the account's lineage: the generation spent and its
    /// successor are both remembered as the account's own.
    fn post(&self, input: &CredentialBlob, source: Source) -> Result<CredentialBlob, Attempt> {
        match refresh_grant_within(
            self.context.transport,
            input,
            (self.context.clock)(),
            LOCKED_REFRESH_TIMEOUT,
        ) {
            Ok(RefreshedGrant {
                credentials: successor,
                ..
            }) => {
                let lineage = [
                    credential_fingerprint(input),
                    credential_fingerprint(&successor),
                ];
                for fingerprint in lineage.iter().flatten() {
                    if let Some(key) = self.key(Some(fingerprint.as_str())) {
                        self.context.verdicts.affirm(key);
                    }
                }
                Ok(successor)
            }
            Err(error @ (RefreshError::InvalidGrant | RefreshError::NoRefreshToken)) => {
                if self.source_moved(input, source) {
                    return Err(self.refresh_failed());
                }
                Err(Attempt::Dead {
                    kind: error.kind(),
                    struck_fp: credential_fingerprint(input),
                })
            }
            Err(_) => Err(self.refresh_failed()),
        }
    }

    /// Whether `source`, the file `input` came from, now holds another, non-empty lineage. A file that exists and cannot
    /// be read or decoded counts as moved: the source cannot be seen, so its generation is never declared dead. Only an
    /// absent file is absent.
    fn source_moved(&self, input: &CredentialBlob, source: Source) -> bool {
        let read = if source == Source::Backup {
            self.context
                .store
                .read_credentials(self.account.number)
                .ok()
        } else {
            self.read_live().map(Some)
        };
        let Some(now) = read else {
            return true;
        };
        now.is_some_and(|now| {
            !now.expose().is_empty()
                && credential_fingerprint(&now) != credential_fingerprint(input)
        })
    }

    /// Makes `working` live and returns it. After a POST (`spent` is the generation posted) it is first written to the
    /// backup, and when that fails, parked in the unclaimed stash keyed to `spent`, where the next refresh adopts it
    /// (the inactive path's, or this path's under the locks): a spent generation's successor is never left to the live file alone.
    /// A live write that fails is [`PollResult::LiveWriteFailed`].
    fn make_live(
        &self,
        held: &Held<'_>,
        working: CredentialBlob,
        spent: Option<&CredentialBlob>,
    ) -> Result<CredentialBlob, Attempt> {
        if let Some(spent) = spent {
            let written = self.slot.write(held.lock, &working).is_ok();
            self.backup_written.set(Some(written));
            if !written {
                // A stash that fails too leaves the successor to the live write alone.
                let _stashed = self
                    .slot
                    .stash(
                        &working,
                        credential_fingerprint(spent).as_deref(),
                        StashReason::PersistFailed,
                        &self.account.org_uuid,
                    )
                    .is_ok();
            }
        }
        self.write_live(held, &working)
            .ok_or(Attempt::Release(PollResult::LiveWriteFailed))?;
        Ok(working)
    }

    /// The live write, under Claude Code's credential locks already held. `None` when it failed; a write that failed
    /// notes whether its temp file was left on disk.
    fn write_live(&self, held: &Held<'_>, working: &CredentialBlob) -> Option<()> {
        let text = live_text(held.live, working)?;
        let written = write_live_credentials(self.context.claude, held.claude, &text);
        if let Err(error) = &written {
            if matches!(error, ClaudeFileError::LockLost) {
                tracing::warn!(
                    primary = "Claude Code's OAuth refresh lock",
                    legacy = "Claude Code's credentials lock",
                    "Claude Code's credential locks were lost during the refresh; the live credential file keeps its old token"
                );
            }
            self.live_temp_left
                .set(Some(matches!(error, ClaudeFileError::WriteLeftTemp(_))));
        }
        written.ok()
    }

    /// Clears `primaryApiKey` from `~/.claude.json` under Claude Code's config lock, taken for the clear alone: whether
    /// the key is gone, `false` when the config lock was not had.
    fn clear_primary_key(&self) -> bool {
        ClaudeCodeLocks::config_with(self.context.claude, self.context.claude_locks)
            .is_ok_and(|config| clear_primary_api_key(self.context.claude, &config).is_ok())
    }

    /// Whether `live` is the account's to overwrite: empty, a lineage the memo names as the account's, or, with no
    /// verdict, the backup's lineage.
    fn owns(&self, live: &CredentialBlob, backup: &Backup) -> bool {
        let Some(fingerprint) = credential_fingerprint(live) else {
            return true;
        };
        self.verdict(Some(&fingerprint))
            .unwrap_or_else(|| backup.fingerprint.as_deref() == Some(fingerprint.as_str()))
    }

    /// The fetch after the locks, with the credential the account now works with.
    fn fetch_working<T: UsageTransport + ?Sized>(
        &self,
        transport: &T,
        working: &CredentialBlob,
    ) -> Attempt {
        match working.oauth() {
            Some(credentials) => Attempt::Fetch(fetch_reading(
                transport,
                credentials.access_token(),
                self.prepaid,
            )),
            None => self.refresh_failed(),
        }
    }

    /// The live credential file: empty text when it is absent, `None` when it cannot be read or is not UTF-8.
    fn read_live(&self) -> Option<CredentialBlob> {
        match read_live_credentials(self.context.claude) {
            Ok(None) => Some(CredentialBlob::new(String::new())),
            Ok(Some(bytes)) => std::str::from_utf8(&bytes)
                .ok()
                .map(|text| CredentialBlob::new(text.to_owned())),
            Err(_) => None,
        }
    }

    /// The account's record as `accounts.json` holds it now, `None` when it cannot be read or is gone.
    fn current_record(&self) -> Option<AccountRecord> {
        self.context
            .store
            .read_sequence()
            .ok()
            .flatten()?
            .account(self.account.number)
    }

    /// The lineage key of `fingerprint` (empty for none), built from the account's record as it is now; `None` when
    /// the record cannot be read, so the lookup misses.
    fn key(&self, fingerprint: Option<&str>) -> Option<LineageKey> {
        let own = self.current_record()?;
        Some(LineageKey::new(
            &self.account.email,
            &own,
            fingerprint.unwrap_or(""),
        ))
    }

    fn verdict(&self, fingerprint: Option<&str>) -> Option<bool> {
        self.key(fingerprint)
            .and_then(|key| self.context.verdicts.get(&key))
    }

    /// The rotated backup's resync: after the server accepted the live credential, write it to the backup when
    /// its lineage differs from the backup's and is the account's own. Best effort; never fails the fetch.
    fn resync(&self) {
        if let Some(fingerprint) = self.resync_verdict() {
            self.resync_locked(&fingerprint);
        }
    }

    /// Before any lock: the live lineage's fingerprint when it is a full pair, differs from the backup's, and is the
    /// account's own by the memo or by the oracle, asked now. An oracle failure or an unverifiable answer skips the
    /// pass and is not remembered.
    fn resync_verdict(&self) -> Option<String> {
        if !OauthShape::of(self.creds).is_some_and(OauthShape::full_pair) {
            return None;
        }
        let fingerprint = credential_fingerprint(self.creds)?;
        // An unreadable backup cannot be compared, so the pass is skipped.
        let backup = self
            .context
            .store
            .read_credentials(self.account.number)
            .ok()?;
        if backup.is_some_and(|backup| {
            credential_fingerprint(&backup).as_deref() == Some(fingerprint.as_str())
        }) {
            return None;
        }
        let owned = match self.verdict(Some(&fingerprint)) {
            Some(verdict) => verdict,
            None => self.probe(&fingerprint)?,
        };
        owned.then_some(fingerprint)
    }

    /// Asks the oracle whose the live access token is, and remembers a definite answer under the key of the record as
    /// it was read before the match, with the resolved uuid when an email match recorded it on the account. A record
    /// that changed before the uuid could be recorded remembers nothing.
    fn probe(&self, fingerprint: &str) -> Option<bool> {
        let oauth = self.creds.oauth()?;
        let resolved = resolve(
            &self
                .context
                .profile
                .get_profile(oauth.access_token())
                .ok()?,
        )?;
        let own = self.current_record()?;
        let verdict = resolved_matches_slot_identity(&own, &resolved);
        if verdict == Verdict::Unverifiable {
            return None;
        }
        let backfilled = if verdict == Verdict::SameWithoutUuid {
            Some(self.backfill(&own, &resolved.uuid)?)
        } else {
            None
        };
        let key = LineageKey::after_match(&self.account.email, &own, backfilled, fingerprint);
        self.context.verdicts.remember(key, verdict)
    }

    /// Records `uuid` on `own`, the account's record as the match read it, under the account lock: `uuid` once it is
    /// recorded, `None` when the lock was not had, the write failed or the record changed meanwhile.
    fn backfill<'u>(&self, own: &AccountRecord, uuid: &'u str) -> Option<&'u str> {
        let lock = AccountLock::acquire(self.context.store.root()).ok()?;
        let recorded = self
            .context
            .store
            .backfill_uuid(&lock, own.number, uuid, &Identity::of(own))
            .ok()?;
        recorded.then_some(uuid)
    }

    /// Under the account lock and Claude Code's credential locks: the identity and the verdict still hold, the live
    /// file still carries the lineage as a full pair, and it is a newer generation than the backup
    /// ([`newer_generation`]), which a backup that cannot be read is not; then the backup is written with it.
    fn resync_locked(&self, fingerprint: &str) {
        let Ok(lock) = AccountLock::acquire(self.context.store.root()) else {
            return;
        };
        let Ok(_claude) =
            ClaudeCodeLocks::credentials_with(self.context.claude, self.context.claude_locks)
        else {
            return;
        };
        if live_identity(self.context.claude) != Some(Identity::of(self.account))
            || self.verdict(Some(fingerprint)) != Some(true)
        {
            return;
        }
        let Some(live) = self.read_live() else {
            return;
        };
        let full = OauthShape::of(&live).is_some_and(OauthShape::full_pair);
        if !full || credential_fingerprint(&live).as_deref() != Some(fingerprint) {
            return;
        }
        let Ok(stored) = self.context.store.read_credentials(self.account.number) else {
            return;
        };
        if stored.is_none_or(|stored| newer_generation(&live, &stored)) {
            // A failed write leaves the backup stale and is reported on the outcome; the refresh's attribution checks
            // still guard the next expiry.
            self.backup_written
                .set(Some(self.slot.write(&lock, &live).is_ok()));
        }
    }
}

/// What the live file becomes: `live`, as read under the locks, with only `claudeAiOauth` replaced by `working`'s, or
/// `working` whole when the live file was absent, is empty or is not a JSON object. `None` when the live file is an
/// object and `working` holds no `claudeAiOauth` object: written whole, it would drop the live file's own keys
/// (`mcpOAuth`, plugin secrets).
fn live_text(live: &CredentialBlob, working: &CredentialBlob) -> Option<CredentialBlob> {
    if !is_json_object(live) {
        return Some(copy(working));
    }
    splice_oauth(live, working)
}

/// Whether `blob` parses as a JSON object; the parsed tree's strings, which may hold tokens, are zeroed before it drops.
fn is_json_object(blob: &CredentialBlob) -> bool {
    let Ok(mut parsed) = serde_json::from_str::<Value>(blob.expose()) else {
        return false;
    };
    let object = parsed.is_object();
    scrub(&mut parsed);
    object
}
