//! The remote hosts' updates this client asked for: one pending update per
//! host, from the request through the host's answer, its daemon's restart
//! and its next `Welcome`, to the toast that says how it went. Plain state
//! and the toasts to show; the view sends, draws and ticks
//! (`docs/plans/remote-update.md`, "Client to host, in the client").

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::connection_picker::version_against;
use crate::notices::{ToastKind, ToastSpec};
use crate::target::HostKey;

/// How long a host has to answer a request, `RemoteUpdateStarted` or a
/// refusal, before the client says it has not come back. The host answers
/// only after it downloads and checks the release, counts down 10 s and
/// waits up to 30 s for its own app, so the bound is generous.
pub const ASK_BOUND: Duration = Duration::from_mins(5);

/// How long a host that started its update has to come back with a
/// `Welcome` after its connection dropped, or after it started when no drop
/// is seen. The installer and the daemon's restart take seconds, and the
/// client redials every 10 s at most.
pub const RETURN_BOUND: Duration = Duration::from_secs(120);

/// Where a pending update stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// The request is sent; the host has not answered.
    Asking,
    /// The host launched the update to `version`.
    Started { version: String },
    /// The host's connection dropped after it started the update.
    Dropped { version: String },
    /// The host came back still on `build_version`; its update result may
    /// follow.
    BackOld {
        version: String,
        build_version: String,
    },
}

/// One host's update in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// The id the request went with, which the host's answer echoes.
    pub request_id: String,
    /// The host's name as its Connections row shows it.
    pub name: String,
    pub phase: Phase,
    /// When the update ends unanswered.
    pub deadline: Instant,
    /// Whether this app updates itself once the host runs the new release:
    /// recorded when the update was confirmed, for a release that breaks
    /// sessions or the protocol and is newer than this app.
    pub self_update_after: bool,
}

/// Every host's pending update, by host.
#[derive(Debug, Default)]
pub struct RemoteUpdates {
    pending: HashMap<HostKey, Pending>,
    /// The host and request id of a flagged update whose host now runs the
    /// new release: this app's own update is owed.
    self_update: Option<(HostKey, String)>,
}

/// The key of `host`'s update toast, which every step updates in place.
#[must_use]
pub fn toast_key(host: &HostKey) -> String {
    format!("remote-update-{}", host.wire().unwrap_or("this-pc"))
}

/// The toast of `host`'s update.
fn toast(host: &HostKey, kind: ToastKind, title: String, detail: Option<String>) -> ToastSpec {
    ToastSpec {
        kind,
        title,
        detail,
        key: Some(toast_key(host)),
        sticky: false,
        actions: Vec::new(),
    }
}

/// The sticky progress toast of `host`'s update.
fn progress(host: &HostKey, title: String) -> ToastSpec {
    ToastSpec {
        sticky: true,
        ..toast(host, ToastKind::Info, title, None)
    }
}

/// Whether a host back on `build_version` runs the update to `version`, or
/// a newer release.
fn reached(build_version: &str, version: &str) -> bool {
    build_version == version
        || version_against(Some(build_version), version).is_some_and(|(_, behind)| !behind)
}

impl RemoteUpdates {
    /// The request `request_id` went to `host`, named `name`: its update
    /// is pending, and the Asking toast shows. With `self_update_after`,
    /// this app's own update is owed once the host runs the new release.
    pub fn request(
        &mut self,
        host: &HostKey,
        name: &str,
        request_id: String,
        self_update_after: bool,
        now: Instant,
    ) -> ToastSpec {
        self.pending.insert(
            host.clone(),
            Pending {
                request_id,
                name: name.to_owned(),
                phase: Phase::Asking,
                deadline: now + ASK_BOUND,
                self_update_after,
            },
        );
        progress(host, format!("Asking {name} to update…"))
    }

    /// `host` answered `request_id`: it is updating to `version`.
    pub fn on_started(
        &mut self,
        host: &HostKey,
        request_id: &str,
        version: &str,
        now: Instant,
    ) -> Option<ToastSpec> {
        let pending = self.pending.get_mut(host)?;
        if pending.request_id != request_id || pending.phase != Phase::Asking {
            return None;
        }
        pending.phase = Phase::Started {
            version: version.to_owned(),
        };
        pending.deadline = now + RETURN_BOUND;
        Some(progress(
            host,
            format!("{} is updating to {version}…", pending.name),
        ))
    }

    /// `host` refused `request_id`, or cancelled it, saying `message`: the
    /// toast when the request was this client's pending one, which ends.
    pub fn on_refused(
        &mut self,
        host: &HostKey,
        request_id: &str,
        message: &str,
    ) -> Option<ToastSpec> {
        if self.pending.get(host)?.request_id != request_id {
            return None;
        }
        let pending = self.pending.remove(host)?;
        Some(toast(
            host,
            ToastKind::Error,
            format!("{} didn't update", pending.name),
            Some(message.to_owned()),
        ))
    }

    /// `host`'s connection is not open: a started update waits
    /// [`RETURN_BOUND`] from the first drop for the host's `Welcome`; a
    /// request not yet answered keeps its own bound; a host back on its old
    /// release has nothing more to say.
    pub fn on_drop(&mut self, host: &HostKey, now: Instant) {
        let Some(pending) = self.pending.get_mut(host) else {
            return;
        };
        match &pending.phase {
            Phase::Started { version } => {
                pending.phase = Phase::Dropped {
                    version: version.clone(),
                };
                pending.deadline = now + RETURN_BOUND;
            }
            Phase::BackOld { .. } => {
                self.pending.remove(host);
            }
            Phase::Asking | Phase::Dropped { .. } => {}
        }
    }

    /// `host` said `Welcome` reporting `build_version`: after it started
    /// its update, it either runs the new release, which ends the update,
    /// or still the old one, whose result may follow. A host that reports
    /// no version is left to its bound.
    pub fn on_welcome(&mut self, host: &HostKey, build_version: Option<&str>) -> Option<ToastSpec> {
        let pending = self.pending.get_mut(host)?;
        let (Phase::Started { version } | Phase::Dropped { version }) = &pending.phase else {
            return None;
        };
        let build_version = build_version?;
        let version = version.clone();
        let name = pending.name.clone();
        if reached(build_version, &version) {
            let pending = self.pending.remove(host)?;
            return Some(self.updated(host, pending, &version));
        }
        pending.phase = Phase::BackOld {
            version,
            build_version: build_version.to_owned(),
        };
        Some(toast(
            host,
            ToastKind::Error,
            old_title(&name, build_version),
            None,
        ))
    }

    /// `host` refused this client's `Hello` saying `reason`: after a flagged
    /// update started, a refusal for sharing no protocol version means the
    /// host runs the new release, which ends the update as a `Welcome` on it
    /// does, and this app's own update is owed. Anything else changes
    /// nothing.
    pub fn on_auth_failed(&mut self, host: &HostKey, reason: &str) -> Option<ToastSpec> {
        let pending = self.pending.get(host)?;
        let (Phase::Started { version } | Phase::Dropped { version }) = &pending.phase else {
            return None;
        };
        if !pending.self_update_after || !reason.starts_with(protocol::NO_COMMON_PROTOCOL_REASON) {
            return None;
        }
        let version = version.clone();
        let pending = self.pending.remove(host)?;
        Some(self.updated(host, pending, &version))
    }

    /// `host`'s ended `pending` update reached `version`: the toast that
    /// says so, and this app's own update owed when the update was flagged.
    fn updated(&mut self, host: &HostKey, pending: Pending, version: &str) -> ToastSpec {
        if pending.self_update_after {
            self.self_update = Some((host.clone(), pending.request_id));
        }
        toast(
            host,
            ToastKind::Info,
            format!("{} updated to {version}", pending.name),
            None,
        )
    }

    /// The host and request id of the flagged update whose host now runs
    /// the new release, once: this app's own update is owed.
    pub fn take_self_update(&mut self) -> Option<(HostKey, String)> {
        self.self_update.take()
    }

    /// `host` sent how its last update ended: a failure says why under
    /// the toast of a host back on its old release, and either ends that
    /// update. A result with no such update pending is ignored.
    pub fn on_result(&mut self, host: &HostKey, ok: bool, message: &str) -> Option<ToastSpec> {
        let Phase::BackOld { build_version, .. } = &self.pending.get(host)?.phase else {
            return None;
        };
        let build_version = build_version.clone();
        let pending = self.pending.remove(host)?;
        (!ok).then(|| {
            toast(
                host,
                ToastKind::Error,
                old_title(&pending.name, &build_version),
                Some(message.to_owned()),
            )
        })
    }

    /// Ends every update whose bound passed by `now`: the toast for each
    /// host that has not come back.
    pub fn expire(&mut self, now: Instant) -> Vec<ToastSpec> {
        let due: Vec<HostKey> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.deadline <= now)
            .map(|(host, _)| host.clone())
            .collect();
        due.into_iter()
            .filter_map(|host| {
                let pending = self.pending.remove(&host)?;
                (!matches!(pending.phase, Phase::BackOld { .. })).then(|| {
                    toast(
                        &host,
                        ToastKind::Warning,
                        format!(
                            "{} hasn't come back; it may still be updating.",
                            pending.name
                        ),
                        None,
                    )
                })
            })
            .collect()
    }

    /// Whether `host` has an update pending.
    #[must_use]
    pub fn is_pending(&self, host: &HostKey) -> bool {
        self.pending.contains_key(host)
    }

    /// Whether any host has an update pending.
    #[must_use]
    pub fn any_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// `host`'s pending update.
    #[must_use]
    pub fn pending(&self, host: &HostKey) -> Option<&Pending> {
        self.pending.get(host)
    }
}

/// The title of a host back on `build_version`.
fn old_title(name: &str, build_version: &str) -> String {
    format!("{name} didn't update; it is still on {build_version}")
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;

    const ID: &str = "req-1";

    fn host() -> HostKey {
        HostKey::Remote("box.tail1234.ts.net".to_owned())
    }

    /// Updates with `host()`'s request sent at `now`.
    fn asked(now: Instant) -> RemoteUpdates {
        let mut updates = RemoteUpdates::default();
        updates.request(&host(), "box", ID.to_owned(), false, now);
        updates
    }

    /// Updates whose host started its update to 0.12.0 at `now`.
    fn started(now: Instant) -> RemoteUpdates {
        let mut updates = asked(now);
        updates
            .on_started(&host(), ID, "0.12.0", now)
            .expect("the answer is ours");
        updates
    }

    /// Updates with `host()`'s flagged request sent at `now`: this app
    /// updates itself after the host.
    fn flagged_asked(now: Instant) -> RemoteUpdates {
        let mut updates = RemoteUpdates::default();
        updates.request(&host(), "box", ID.to_owned(), true, now);
        updates
    }

    /// Updates whose flagged host started its update to 0.12.0 at `now`
    /// and then dropped.
    fn flagged_dropped(now: Instant) -> RemoteUpdates {
        let mut updates = flagged_asked(now);
        updates
            .on_started(&host(), ID, "0.12.0", now)
            .expect("the answer is ours");
        updates.on_drop(&host(), now);
        updates
    }

    const NO_COMMON: &str = "no compatible protocol version: client advertises scalar=30 \
                             + range=[30], daemon supports [31]";

    #[test]
    fn a_flagged_update_owes_this_apps_update_once_the_host_welcomes_on_the_new_version() {
        let now = Instant::now();
        let mut updates = flagged_dropped(now);
        assert_eq!(
            updates.take_self_update(),
            None,
            "not before the host is back"
        );
        assert!(updates.on_welcome(&host(), Some("0.12.0")).is_some());
        assert_eq!(updates.take_self_update(), Some((host(), ID.to_owned())));
        assert_eq!(updates.take_self_update(), None, "taken once");
    }

    #[test]
    fn an_unflagged_update_owes_nothing_after_the_host_welcomes() {
        let now = Instant::now();
        let mut updates = started(now);
        updates.on_drop(&host(), now);
        assert!(updates.on_welcome(&host(), Some("0.12.0")).is_some());
        assert_eq!(updates.take_self_update(), None);
    }

    #[test]
    fn no_common_protocol_after_a_flagged_drop_counts_as_updated() {
        let now = Instant::now();
        let mut updates = flagged_dropped(now);
        let spec = updates
            .on_auth_failed(&host(), NO_COMMON)
            .expect("the host runs the new release");
        assert_eq!(spec.title, "box updated to 0.12.0");
        assert_eq!(spec.kind, ToastKind::Info);
        assert!(!spec.sticky);
        assert!(!updates.is_pending(&host()));
        assert_eq!(updates.take_self_update(), Some((host(), ID.to_owned())));
    }

    #[test]
    fn no_common_protocol_after_an_unflagged_drop_changes_nothing() {
        let now = Instant::now();
        let mut updates = started(now);
        updates.on_drop(&host(), now);
        assert_eq!(updates.on_auth_failed(&host(), NO_COMMON), None);
        assert!(updates.is_pending(&host()));
        assert_eq!(updates.take_self_update(), None);
    }

    #[test]
    fn another_auth_failure_after_a_flagged_drop_changes_nothing() {
        let now = Instant::now();
        let mut updates = flagged_dropped(now);
        for reason in ["bad token", "device not approved"] {
            assert_eq!(updates.on_auth_failed(&host(), reason), None, "{reason}");
        }
        assert!(updates.is_pending(&host()));
        assert_eq!(updates.take_self_update(), None);
    }

    #[test]
    fn no_common_protocol_while_asking_changes_nothing() {
        let now = Instant::now();
        let mut updates = flagged_asked(now);
        assert_eq!(updates.on_auth_failed(&host(), NO_COMMON), None);
        assert!(updates.is_pending(&host()));
        assert_eq!(updates.take_self_update(), None);
    }

    #[test]
    fn a_flagged_update_that_fails_or_vanishes_owes_nothing() {
        let now = Instant::now();
        let mut refused = flagged_asked(now);
        assert!(refused.on_refused(&host(), ID, "no").is_some());
        assert_eq!(refused.take_self_update(), None, "a refusal");

        let mut old = flagged_dropped(now);
        assert!(old.on_welcome(&host(), Some("0.11.0")).is_some());
        assert!(old.on_result(&host(), false, "failed").is_some());
        assert_eq!(old.take_self_update(), None, "back on the old release");

        let mut gone = flagged_dropped(now);
        assert_eq!(gone.expire(now + RETURN_BOUND).len(), 1);
        assert_eq!(gone.take_self_update(), None, "never came back");
    }

    #[test]
    fn a_request_shows_asking() {
        let now = Instant::now();
        let mut updates = RemoteUpdates::default();
        let spec = updates.request(&host(), "box", ID.to_owned(), false, now);
        assert_eq!(spec.title, "Asking box to update…");
        assert_eq!(spec.kind, ToastKind::Info);
        assert!(spec.sticky);
        assert_eq!(
            spec.key.as_deref(),
            Some("remote-update-box.tail1234.ts.net")
        );
        assert!(updates.is_pending(&host()));
    }

    #[test]
    fn started_shows_updating_to_the_version() {
        let now = Instant::now();
        let mut updates = asked(now);
        assert_eq!(updates.on_started(&host(), "other", "0.12.0", now), None);
        let spec = updates
            .on_started(&host(), ID, "0.12.0", now)
            .expect("the answer is ours");
        assert_eq!(spec.title, "box is updating to 0.12.0…");
        assert!(spec.sticky);
        assert_eq!(
            updates.pending(&host()).map(|pending| &pending.phase),
            Some(&Phase::Started {
                version: "0.12.0".to_owned()
            })
        );
    }

    #[test]
    fn a_refusal_with_our_id_is_ours_and_ends_the_update() {
        let mut updates = asked(Instant::now());
        let spec = updates
            .on_refused(&host(), ID, "This host is already on the newest release.")
            .expect("the refusal is ours");
        assert_eq!(spec.title, "box didn't update");
        assert_eq!(
            spec.detail.as_deref(),
            Some("This host is already on the newest release.")
        );
        assert_eq!(spec.kind, ToastKind::Error);
        assert!(!spec.sticky);
        assert!(!updates.is_pending(&host()));
    }

    #[test]
    fn a_refusal_with_another_id_is_not_ours() {
        let mut updates = asked(Instant::now());
        assert_eq!(updates.on_refused(&host(), "other", "no"), None);
        let elsewhere = HostKey::Remote("den.tail1234.ts.net".to_owned());
        assert_eq!(updates.on_refused(&elsewhere, ID, "no"), None);
        assert!(updates.is_pending(&host()));
    }

    #[test]
    fn welcome_on_the_new_version_succeeds() {
        let now = Instant::now();
        let mut updates = started(now);
        updates.on_drop(&host(), now);
        let spec = updates
            .on_welcome(&host(), Some("0.12.0"))
            .expect("the host is back");
        assert_eq!(spec.title, "box updated to 0.12.0");
        assert_eq!(spec.kind, ToastKind::Info);
        assert!(!spec.sticky);
        assert!(!updates.is_pending(&host()));
        let mut newer = started(now);
        assert!(newer.on_welcome(&host(), Some("0.12.1")).is_some());
        assert!(!newer.is_pending(&host()), "a newer release counts too");
    }

    #[test]
    fn welcome_on_an_old_version_says_it_did_not_update() {
        let now = Instant::now();
        let mut updates = started(now);
        updates.on_drop(&host(), now);
        let spec = updates
            .on_welcome(&host(), Some("0.11.0"))
            .expect("the host is back");
        assert_eq!(spec.title, "box didn't update; it is still on 0.11.0");
        assert_eq!(spec.kind, ToastKind::Error);
        assert!(!spec.sticky);
        assert_eq!(asked(now).on_welcome(&host(), Some("0.11.0")), None);
    }

    #[test]
    fn a_failed_result_after_an_old_welcome_adds_its_message() {
        let now = Instant::now();
        let mut updates = started(now);
        updates.on_welcome(&host(), Some("0.11.0"));
        let spec = updates
            .on_result(&host(), false, "The installer exited with code 3.")
            .expect("the result explains the old release");
        assert_eq!(spec.title, "box didn't update; it is still on 0.11.0");
        assert_eq!(
            spec.detail.as_deref(),
            Some("The installer exited with code 3.")
        );
        assert!(!updates.is_pending(&host()));
    }

    #[test]
    fn a_result_with_no_pending_update_is_ignored() {
        let now = Instant::now();
        let mut updates = RemoteUpdates::default();
        assert_eq!(updates.on_result(&host(), false, "failed"), None);
        let mut asking = asked(now);
        assert_eq!(asking.on_result(&host(), false, "failed"), None);
        assert!(asking.is_pending(&host()), "a request still waits");
    }

    #[test]
    fn no_answer_within_the_ask_bound_expires() {
        let now = Instant::now();
        let mut updates = asked(now);
        assert_eq!(updates.expire(now + Duration::from_secs(299)), []);
        let specs = updates.expire(now + ASK_BOUND);
        assert_eq!(
            specs
                .iter()
                .map(|spec| spec.title.as_str())
                .collect::<Vec<_>>(),
            ["box hasn't come back; it may still be updating."]
        );
        assert_eq!(
            specs.first().map(|spec| spec.kind),
            Some(ToastKind::Warning)
        );
        assert!(!updates.is_pending(&host()));
    }

    #[test]
    fn no_welcome_within_the_return_bound_after_a_drop_expires() {
        let now = Instant::now();
        let mut updates = started(now);
        let dropped = now + Duration::from_secs(30);
        updates.on_drop(&host(), dropped);
        updates.on_drop(&host(), dropped + Duration::from_secs(10));
        assert_eq!(updates.expire(dropped + Duration::from_secs(119)), []);
        assert_eq!(updates.expire(dropped + RETURN_BOUND).len(), 1);
        assert!(!updates.is_pending(&host()));
    }

    #[test]
    fn a_drop_before_started_does_not_start_the_return_bound() {
        let now = Instant::now();
        let mut updates = asked(now);
        updates.on_drop(&host(), now);
        assert_eq!(updates.expire(now + RETURN_BOUND), []);
        assert_eq!(
            updates.pending(&host()).map(|pending| &pending.phase),
            Some(&Phase::Asking)
        );
        assert_eq!(updates.expire(now + ASK_BOUND).len(), 1);
    }
}
