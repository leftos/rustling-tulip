//! Which delivered Needs you notifications the phone removes while it is in
//! front, and what its app badge shows: plain Rust over the drawer's
//! notifications and each saved host's live Needs you set, so the rule reads
//! and tests without a window. A background clear push ([`clear`]) removes
//! its own session's notification while the app is not in front.

use std::collections::BTreeSet;

use protocol::{PushNotice, PushNoticeKind};
use rustling_tulip_ios_notify::clearing::{self, Drawn};

use super::push::{Delivered, NEEDS_YOU_KIND};
use crate::needs_you::{NeedsYouRow, Reason};
use crate::target::HostKey;

/// One saved host as the sweep sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostNow {
    /// The host's wire key, the `host` a notification's user info names.
    pub(crate) host: String,
    /// Whether the host is connected and has sent its whole session list
    /// since the app last came to the foreground.
    pub(crate) loaded: bool,
    /// The ids of the host's sessions in Needs you.
    pub(crate) needs_you: BTreeSet<String>,
}

/// What a sweep does: the notifications to remove, and the badge to set,
/// `None` to leave it alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Sweep {
    /// The identifiers of the notifications to remove, in drawer order.
    pub(crate) remove: Vec<String>,
    /// The badge, or `None` when no host is connected and loaded.
    pub(crate) badge: Option<u32>,
}

/// The ids of `host`'s sessions waiting on the user among `rows`: the
/// Asking rows only, the daemon's own Needs you rule, so a session that
/// ended (an Ended row) is out of the set.
pub(crate) fn needs_you_ids(rows: &[NeedsYouRow], host: &HostKey) -> BTreeSet<String> {
    rows.iter()
        .filter(|row| row.host == *host && row.reason == Reason::Asking)
        .map(|row| row.session_id.clone())
        .collect()
}

/// The sweep over `delivered` against `hosts`: a notification goes only when
/// it is a Needs you notification, its host is connected and loaded, and its
/// session is not in that host's Needs you set, a session gone from the list
/// included. Any other notification stays: one of another kind (a Finished
/// alert) or of no kind (an older extension build), one with no `host` or
/// `session_id` (a fallback alert), one for a host not in `hosts`, and one
/// whose host is not loaded. The badge is the Needs you count over the
/// loaded hosts, or `None` when no host is loaded.
pub(crate) fn sweep(delivered: &[Delivered], hosts: &[HostNow]) -> Sweep {
    let remove = delivered
        .iter()
        .filter(|shown| ended(shown, hosts))
        .map(|shown| shown.id.clone())
        .collect();
    let loaded: Vec<&HostNow> = hosts.iter().filter(|host| host.loaded).collect();
    let badge = (!loaded.is_empty()).then(|| {
        let count: usize = loaded.iter().map(|host| host.needs_you.len()).sum();
        u32::try_from(count).unwrap_or(u32::MAX)
    });
    Sweep { remove, badge }
}

/// What a background push `notice` does to `delivered`, `None` unless it is
/// a clear: it removes each Needs you notification the extension's clearing
/// rule ([`clearing::ends`]) says the clear ends, and sets the badge to the
/// notice's count. That is the clear's own wait (the same host and session,
/// a wait that began no later than the cleared one; either side with no
/// wait start removes by session alone), and, when the clear names the
/// host's sessions still waiting, every other wait of that host that ended
/// by its `as_of`. A notification of a newer wait stays.
#[cfg_attr(
    all(not(target_os = "ios"), not(test)),
    expect(dead_code, reason = "only the iOS background wake calls it")
)]
pub(crate) fn clear(notice: &PushNotice, delivered: &[Delivered]) -> Option<Sweep> {
    if notice.kind != PushNoticeKind::Cleared {
        return None;
    }
    let remove = delivered
        .iter()
        .filter(|shown| cleared_by(shown, notice))
        .map(|shown| shown.id.clone())
        .collect();
    Some(Sweep {
        remove,
        badge: Some(notice.badge),
    })
}

/// Whether the clear `notice` removes `shown`.
#[cfg_attr(
    all(not(target_os = "ios"), not(test)),
    expect(dead_code, reason = "only the iOS background wake calls it")
)]
fn cleared_by(shown: &Delivered, notice: &PushNotice) -> bool {
    clearing::ends(
        notice,
        Drawn {
            kind: shown.kind.as_deref(),
            host: shown.host.as_deref(),
            session_id: shown.session_id.as_deref(),
            waited_since: shown.waited_since.as_deref(),
        },
    )
}

/// Whether `shown` is a Needs you notification whose wait has ended: its
/// host is loaded and no longer lists its session in Needs you.
fn ended(shown: &Delivered, hosts: &[HostNow]) -> bool {
    if shown.kind.as_deref() != Some(NEEDS_YOU_KIND) {
        return false;
    }
    let (Some(host), Some(session_id)) = (&shown.host, &shown.session_id) else {
        return false;
    };
    hosts
        .iter()
        .find(|now| now.host == *host)
        .is_some_and(|now| now.loaded && !now.needs_you.contains(session_id))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use std::collections::BTreeSet;

    use chrono::{DateTime, Utc};
    use protocol::{PUSH_NOTICE_VERSION, PushNotice, PushNoticeKind};

    use super::{HostNow, Sweep, clear, needs_you_ids, sweep};
    use crate::needs_you::{NeedsYouRow, Reason};
    use crate::phone::push::Delivered;
    use crate::target::HostKey;

    /// A tailnet host's wire key.
    const BOX: &str = "box.tail1234.ts.net";
    /// A second tailnet host's wire key.
    const LAB: &str = "lab.tail1234.ts.net";

    /// A Needs you notification `id` for session `session` on `host`.
    fn shown(id: &str, session: &str, host: &str) -> Delivered {
        Delivered {
            id: id.to_owned(),
            host: Some(host.to_owned()),
            session_id: Some(session.to_owned()),
            kind: Some("needs_you".to_owned()),
            waited_since: None,
        }
    }

    /// `host`, `loaded` or not, with `needs_you` in Needs you.
    fn host(host: &str, loaded: bool, needs_you: &[&str]) -> HostNow {
        HostNow {
            host: host.to_owned(),
            loaded,
            needs_you: needs_you.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    /// The identifiers a sweep removes.
    fn removed(result: &Sweep) -> Vec<&str> {
        result.remove.iter().map(String::as_str).collect()
    }

    /// [`BOX`]'s Needs you row for `session`, listed for `reason`.
    fn row(session: &str, reason: Reason) -> NeedsYouRow {
        NeedsYouRow {
            host: HostKey::Remote(BOX.to_owned()),
            session_id: session.to_owned(),
            container_name: "repo".to_owned(),
            accent: 0,
            label: session.to_owned(),
            reason,
            since: None,
            detail: String::new(),
        }
    }

    #[test]
    fn a_working_session_on_a_loaded_host_is_removed() {
        let result = sweep(&[shown("n1", "s1", BOX)], &[host(BOX, true, &[])]);
        assert_eq!(removed(&result), vec!["n1"]);
    }

    #[test]
    fn a_session_still_in_needs_you_is_kept() {
        let result = sweep(&[shown("n1", "s1", BOX)], &[host(BOX, true, &["s1"])]);
        assert_eq!(removed(&result), Vec::<&str>::new());
    }

    #[test]
    fn a_session_gone_from_the_list_is_removed() {
        let result = sweep(&[shown("n1", "s1", BOX)], &[host(BOX, true, &["s2"])]);
        assert_eq!(removed(&result), vec!["n1"]);
    }

    #[test]
    fn an_ended_session_is_out_of_needs_you_and_removed() {
        let rows = [row("s1", Reason::Ended), row("s2", Reason::Asking)];
        let needs_you = needs_you_ids(&rows, &HostKey::Remote(BOX.to_owned()));
        assert_eq!(needs_you, BTreeSet::from(["s2".to_owned()]));
        let loaded = HostNow {
            host: BOX.to_owned(),
            loaded: true,
            needs_you,
        };
        let result = sweep(&[shown("n1", "s1", BOX)], &[loaded]);
        assert_eq!(removed(&result), vec!["n1"]);
        assert_eq!(result.badge, Some(1), "the ended session is not counted");
    }

    #[test]
    fn a_host_connecting_and_not_loaded_keeps_its_notifications() {
        let result = sweep(&[shown("n1", "s1", BOX)], &[host(BOX, false, &[])]);
        assert_eq!(removed(&result), Vec::<&str>::new());
    }

    #[test]
    fn an_offline_host_keeps_its_notifications() {
        let offline = HostNow {
            host: BOX.to_owned(),
            loaded: false,
            needs_you: BTreeSet::new(),
        };
        let result = sweep(&[shown("n1", "s1", BOX)], &[offline]);
        assert_eq!(removed(&result), Vec::<&str>::new());
    }

    #[test]
    fn a_host_with_no_list_since_the_foreground_keeps_its_notifications() {
        // Loaded before the suspension, but no list since the return: the
        // stale list it held says nothing, so the host reads as not loaded.
        let stale = host(BOX, false, &[]);
        let result = sweep(&[shown("n1", "s1", BOX)], &[stale]);
        assert_eq!(result, Sweep::default());
    }

    #[test]
    fn a_notification_without_keys_is_kept() {
        let fallback = Delivered {
            id: "n1".to_owned(),
            host: None,
            session_id: None,
            kind: None,
            waited_since: None,
        };
        let no_host = Delivered {
            host: None,
            id: "n2".to_owned(),
            ..shown("n2", "s1", BOX)
        };
        let no_session = Delivered {
            session_id: None,
            ..shown("n3", "s1", BOX)
        };
        for hosts in [
            vec![host(BOX, true, &[])],
            vec![host(BOX, false, &[])],
            vec![],
        ] {
            let result = sweep(
                &[fallback.clone(), no_host.clone(), no_session.clone()],
                &hosts,
            );
            assert_eq!(removed(&result), Vec::<&str>::new(), "{hosts:?}");
        }
    }

    #[test]
    fn a_finished_notification_is_kept() {
        let finished = Delivered {
            kind: Some("finished".to_owned()),
            ..shown("n1", "s1", BOX)
        };
        let result = sweep(&[finished], &[host(BOX, true, &[])]);
        assert_eq!(removed(&result), Vec::<&str>::new());
    }

    #[test]
    fn a_notification_of_no_kind_is_kept() {
        let older = Delivered {
            kind: None,
            ..shown("n1", "s1", BOX)
        };
        let result = sweep(&[older], &[host(BOX, true, &[])]);
        assert_eq!(removed(&result), Vec::<&str>::new());
    }

    #[test]
    fn a_notification_for_an_unknown_host_is_kept() {
        let result = sweep(
            &[shown("n1", "s1", "unknown.tail1234.ts.net")],
            &[host(BOX, true, &[])],
        );
        assert_eq!(removed(&result), Vec::<&str>::new());
    }

    #[test]
    fn the_badge_counts_the_loaded_hosts_needs_you() {
        let result = sweep(
            &[],
            &[host(BOX, true, &["s1", "s2"]), host(LAB, false, &["s9"])],
        );
        assert_eq!(result.badge, Some(2));
    }

    /// A Needs you notification for [`shown`] whose wait began at `since`.
    fn waited(id: &str, session: &str, host: &str, since: &str) -> Delivered {
        Delivered {
            waited_since: Some(since.to_owned()),
            ..shown(id, session, host)
        }
    }

    /// A clear of `session` on `host`, of the wait that began at `since`,
    /// setting the badge to 4.
    fn clear_of(session: &str, host: &str, since: Option<&str>) -> PushNotice {
        PushNotice {
            v: PUSH_NOTICE_VERSION,
            kind: PushNoticeKind::Cleared,
            title: String::new(),
            body: String::new(),
            session_id: session.to_owned(),
            host: Some(host.to_owned()),
            badge: 4,
            waited_since: since.map(|since| {
                DateTime::parse_from_rfc3339(since)
                    .expect("a valid instant")
                    .with_timezone(&Utc)
            }),
            waiting: None,
            as_of: None,
        }
    }

    /// The identifiers a clear removes, or `None` when it does nothing.
    fn cleared(notice: &PushNotice, delivered: &[Delivered]) -> Option<Vec<String>> {
        clear(notice, delivered).map(|result| result.remove)
    }

    #[test]
    fn a_clear_removes_its_sessions_needs_you() {
        let notice = clear_of("s1", BOX, Some("2026-11-14T10:00:00Z"));
        let result = clear(&notice, &[waited("n1", "s1", BOX, "2026-11-14T10:00:00Z")]);
        assert_eq!(
            result,
            Some(Sweep {
                remove: vec!["n1".to_owned()],
                badge: Some(4),
            })
        );
    }

    #[test]
    fn a_clear_keeps_a_newer_wait_of_the_same_session() {
        let notice = clear_of("s1", BOX, Some("2026-11-14T10:00:00Z"));
        let result = clear(&notice, &[waited("n1", "s1", BOX, "2026-11-14T10:05:00Z")]);
        assert_eq!(
            result,
            Some(Sweep {
                remove: Vec::new(),
                badge: Some(4),
            }),
            "the badge still follows the clear"
        );
    }

    #[test]
    fn a_clear_keeps_other_sessions_and_kinds() {
        let notice = clear_of("s1", BOX, None);
        let finished = Delivered {
            kind: Some("finished".to_owned()),
            ..shown("n1", "s1", BOX)
        };
        let delivered = [
            finished,
            shown("n2", "s2", BOX),
            shown("n3", "s1", LAB),
            shown("n4", "s1", BOX),
        ];
        assert_eq!(cleared(&notice, &delivered), Some(vec!["n4".to_owned()]));
        let alert = PushNotice {
            kind: PushNoticeKind::NeedsYou,
            ..notice
        };
        assert_eq!(clear(&alert, &delivered), None, "only a clear acts");
    }

    #[test]
    fn a_clear_without_waited_since_removes_by_session() {
        let since = clear_of("s1", BOX, Some("2026-11-14T10:00:00Z"));
        let unknown = clear_of("s1", BOX, None);
        assert_eq!(
            cleared(&since, &[shown("n1", "s1", BOX)]),
            Some(vec!["n1".to_owned()]),
            "a notification with no wait start goes"
        );
        assert_eq!(
            cleared(&unknown, &[waited("n1", "s1", BOX, "2026-11-14T10:05:00Z")]),
            Some(vec!["n1".to_owned()]),
            "a clear with no wait start removes by session"
        );
    }

    #[test]
    fn a_clear_removes_every_ended_wait_of_its_host() {
        // iOS keeps only the newest held background push, so s1's clear can be
        // discarded; s2's clear says what still waits on BOX and clears s1 too.
        let notice: PushNotice = serde_json::from_str(&format!(
            r#"{{"v":{PUSH_NOTICE_VERSION},"kind":"cleared","title":"","body":"","session_id":"s2","host":"{BOX}","badge":1,"waited_since":"2026-11-14T10:05:00Z","waiting":["s3"],"as_of":"2026-11-14T10:07:00Z"}}"#
        ))
        .expect("a clear parses");
        let delivered = [
            waited("n1", "s1", BOX, "2026-11-14T10:00:00Z"), // ended; its own clear was lost
            waited("n2", "s2", BOX, "2026-11-14T10:05:00Z"), // this clear's wait
            waited("n3", "s3", BOX, "2026-11-14T10:06:00Z"), // still waiting
            waited("n4", "s4", BOX, "2026-11-14T10:08:00Z"), // began after the clear was built
            waited("n5", "s1", LAB, "2026-11-14T10:00:00Z"), // another host
        ];
        assert_eq!(
            cleared(&notice, &delivered),
            Some(vec!["n1".to_owned(), "n2".to_owned()])
        );
    }

    #[test]
    fn no_loaded_host_leaves_the_badge_alone() {
        let result = sweep(
            &[shown("n1", "s1", BOX)],
            &[host(BOX, false, &["s1"]), host(LAB, false, &[])],
        );
        assert_eq!(result, Sweep::default());
    }
}
