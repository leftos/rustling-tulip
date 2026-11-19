//! Which delivered Needs you notifications a push notice ends: the one rule
//! the app's background clear and the extension's alert-time cleanup share,
//! as plain Rust over the ids the extension put in each notification's user
//! info, with no iOS call in it.
//!
//! A clear always ends its own wait (the same host and session, a wait that
//! began no later than the cleared one). A clear or a Needs you alert that
//! names the host's sessions still `waiting` also ends every other Needs you
//! notification of that host whose session is not in the list and whose
//! wait began no later than the notice's `as_of`: so one delivered notice
//! repairs what a lost clear left. Each removal only ever removes, and never
//! a wait newer than the notice's `as_of`, so the rule can run on the same
//! drawer from several places close together.

use chrono::{DateTime, Utc};
use protocol::{PushNotice, PushNoticeKind};

/// The wire kind of a Needs you push notice, the only kind the rule removes.
pub const NEEDS_YOU_KIND: &str = "needs_you";

/// One delivered notification as the rule reads it: the ids the extension
/// put in its user info, each `None` when it did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drawn<'a> {
    /// The push notice's wire kind (`needs_you`, `finished`, …).
    pub kind: Option<&'a str>,
    /// The daemon's host name the push came from.
    pub host: Option<&'a str>,
    /// The session the push was about.
    pub session_id: Option<&'a str>,
    /// When the wait the push was about began, RFC 3339.
    pub waited_since: Option<&'a str>,
}

/// Whether `notice` ends the wait `shown` is about, so the notification is
/// removed. Only a Needs you notification is ever removed, and only by a
/// clear or a Needs you notice: a clear by its own wait, and either one,
/// when it names the host's sessions still waiting, by any other ended wait
/// of its host. A notice without the list keeps the per-session rule.
#[must_use]
pub fn ends(notice: &PushNotice, shown: Drawn<'_>) -> bool {
    if shown.kind != Some(NEEDS_YOU_KIND) {
        return false;
    }
    match notice.kind {
        PushNoticeKind::Cleared => own_wait(notice, shown) || ended_elsewhere(notice, shown),
        PushNoticeKind::NeedsYou => ended_elsewhere(notice, shown),
        PushNoticeKind::Finished | PushNoticeKind::Unknown => false,
    }
}

/// Whether `shown` is the clear `notice`'s own wait: the same host and
/// session, a wait that began no later than the cleared one.
fn own_wait(notice: &PushNotice, shown: Drawn<'_>) -> bool {
    shown.session_id == Some(notice.session_id.as_str())
        && shown.host == notice.host.as_deref()
        && not_after(shown.waited_since, notice.waited_since)
}

/// Whether `shown` is another session's wait on the notice's host that
/// ended by the notice's `as_of`: not in its `waiting` list. A notice with
/// no list or no host ends none, and the notice's own session is left to
/// [`own_wait`], so a newer wait of it stays.
fn ended_elsewhere(notice: &PushNotice, shown: Drawn<'_>) -> bool {
    let (Some(waiting), Some(host), Some(session)) =
        (&notice.waiting, notice.host.as_deref(), shown.session_id)
    else {
        return false;
    };
    shown.host == Some(host)
        && session != notice.session_id
        && !waiting.iter().any(|id| id == session)
        && not_after(shown.waited_since, notice.as_of)
}

/// Whether a wait that began at `shown` (RFC 3339) began no later than
/// `bound`; true when either is unknown, a start that does not parse
/// included.
fn not_after(shown: Option<&str>, bound: Option<DateTime<Utc>>) -> bool {
    let (Some(shown), Some(bound)) = (shown, bound) else {
        return true;
    };
    DateTime::parse_from_rfc3339(shown).map_or(true, |shown| shown.with_timezone(&Utc) <= bound)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use chrono::{DateTime, Utc};
    use protocol::{PUSH_NOTICE_VERSION, PushNotice, PushNoticeKind};

    use super::{Drawn, NEEDS_YOU_KIND, ends};

    /// A tailnet host's wire key.
    const BOX: &str = "box.tail1234.ts.net";
    /// A second tailnet host's wire key.
    const LAB: &str = "lab.tail1234.ts.net";

    /// One delivered notification: its id and what the rule reads.
    struct Shown {
        id: &'static str,
        kind: &'static str,
        session: &'static str,
        host: &'static str,
        since: &'static str,
    }

    /// A Needs you notification `id` for `session` on `host`, its wait begun
    /// at `since`.
    fn waited(
        id: &'static str,
        session: &'static str,
        host: &'static str,
        since: &'static str,
    ) -> Shown {
        Shown {
            id,
            kind: NEEDS_YOU_KIND,
            session,
            host,
            since,
        }
    }

    fn instant(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("a valid instant")
            .with_timezone(&Utc)
    }

    /// A notice of `kind` about `session` on [`BOX`], of the wait begun at
    /// `since`, naming `waiting` as of `as_of` when given.
    fn notice(
        kind: PushNoticeKind,
        session: &str,
        since: &str,
        waiting: Option<(&[&str], &str)>,
    ) -> PushNotice {
        PushNotice {
            v: PUSH_NOTICE_VERSION,
            kind,
            title: String::new(),
            body: String::new(),
            session_id: session.to_owned(),
            host: Some(BOX.to_owned()),
            badge: 1,
            waited_since: Some(instant(since)),
            waiting: waiting.map(|(ids, _)| ids.iter().map(|id| (*id).to_owned()).collect()),
            as_of: waiting.map(|(_, as_of)| instant(as_of)),
        }
    }

    /// The ids among `delivered` that `notice` ends.
    fn ended(notice: &PushNotice, delivered: &[Shown]) -> Vec<&'static str> {
        delivered
            .iter()
            .filter(|shown| {
                ends(
                    notice,
                    Drawn {
                        kind: Some(shown.kind),
                        host: Some(shown.host),
                        session_id: Some(shown.session),
                        waited_since: Some(shown.since),
                    },
                )
            })
            .map(|shown| shown.id)
            .collect()
    }

    #[test]
    fn a_clear_without_waiting_keeps_todays_rule() {
        let clear = notice(PushNoticeKind::Cleared, "s2", "2026-11-14T10:05:00Z", None);
        let delivered = [
            waited("n1", "s1", BOX, "2026-11-14T10:00:00Z"),
            waited("n2", "s2", BOX, "2026-11-14T10:05:00Z"),
            waited("n3", "s2", BOX, "2026-11-14T10:06:00Z"),
            waited("n4", "s2", LAB, "2026-11-14T10:05:00Z"),
        ];
        assert_eq!(
            ended(&clear, &delivered),
            vec!["n2"],
            "only its own wait: not another session's, a newer wait or another host's"
        );
        let alert = PushNotice {
            kind: PushNoticeKind::NeedsYou,
            ..clear
        };
        assert_eq!(
            ended(&alert, &delivered),
            Vec::<&str>::new(),
            "an alert without the list removes nothing"
        );
    }

    #[test]
    fn a_wait_that_began_after_as_of_stays() {
        let waiting: &[&str] = &["s3"];
        let delivered = [
            waited("n1", "s1", BOX, "2026-11-14T10:00:00Z"),
            waited("n4", "s4", BOX, "2026-11-14T10:08:00Z"),
            waited("n5", "s5", BOX, "2026-11-14T10:07:00Z"),
        ];
        for kind in [PushNoticeKind::Cleared, PushNoticeKind::NeedsYou] {
            let read = notice(
                kind,
                "s2",
                "2026-11-14T10:05:00Z",
                Some((waiting, "2026-11-14T10:07:00Z")),
            );
            assert_eq!(
                ended(&read, &delivered),
                vec!["n1", "n5"],
                "{kind:?}: a wait begun at as_of is covered, one begun after it is not"
            );
        }
    }

    #[test]
    fn another_hosts_notifications_stay() {
        let clear = notice(
            PushNoticeKind::Cleared,
            "s2",
            "2026-11-14T10:05:00Z",
            Some((&[], "2026-11-14T10:07:00Z")),
        );
        let delivered = [
            waited("n1", "s1", LAB, "2026-11-14T10:00:00Z"),
            waited("n2", "s2", LAB, "2026-11-14T10:05:00Z"),
            waited("n3", "s1", BOX, "2026-11-14T10:00:00Z"),
        ];
        assert_eq!(ended(&clear, &delivered), vec!["n3"]);
        let hostless = PushNotice {
            host: None,
            ..clear
        };
        assert_eq!(
            ended(&hostless, &delivered),
            Vec::<&str>::new(),
            "a notice with no host names no host's waits"
        );
    }

    #[test]
    fn finished_notifications_are_never_removed_by_the_rule() {
        let finished = |id, session| Shown {
            kind: "finished",
            ..waited(id, session, BOX, "2026-11-14T10:00:00Z")
        };
        let newer = |id, session| Shown {
            kind: "something_new",
            ..waited(id, session, BOX, "2026-11-14T10:00:00Z")
        };
        let delivered = [
            finished("n1", "s1"),
            finished("n2", "s2"),
            newer("n3", "s1"),
        ];
        for kind in [
            PushNoticeKind::Cleared,
            PushNoticeKind::NeedsYou,
            PushNoticeKind::Finished,
        ] {
            let read = notice(
                kind,
                "s2",
                "2026-11-14T10:05:00Z",
                Some((&[], "2026-11-14T10:07:00Z")),
            );
            assert_eq!(ended(&read, &delivered), Vec::<&str>::new(), "{kind:?}");
        }
        let finished_notice = notice(
            PushNoticeKind::Finished,
            "s1",
            "2026-11-14T10:05:00Z",
            Some((&[], "2026-11-14T10:07:00Z")),
        );
        assert_eq!(
            ended(
                &finished_notice,
                &[waited("n4", "s1", BOX, "2026-11-14T10:00:00Z")]
            ),
            Vec::<&str>::new(),
            "a Finished notice removes nothing"
        );
    }
}
