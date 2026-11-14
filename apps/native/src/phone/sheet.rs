//! The sheet a long press on a Home row opens: the session it is for,
//! Stop's two-tap arm and the refusal it shows, and [`PhoneReplies`], the
//! replies to the requests the sheet sent.
//!
//! Plain Rust, so the arm, the dismissal and the reply record are
//! unit-testable without GPUI; `root_view` draws the sheet.

use std::collections::HashMap;

use protocol::DaemonMessage;

use super::home::Offer;
use crate::target::HostKey;

/// Stop's row before its first tap.
pub(crate) const STOP_LABEL: &str = "Stop";
/// Stop's row once armed: the next tap sends the stop.
pub(crate) const ARMED_STOP_LABEL: &str = "Tap again to stop";
/// The row that reruns a session that is not running.
pub(crate) const RESUME_LABEL: &str = "Resume";
/// The Resume row while the session's resume is out; a tap does nothing.
pub(crate) const RESUMING_LABEL: &str = "Resuming…";
/// What the sheet of a session on a closed host says in place of its rows.
pub(crate) const OFFLINE_TEXT: &str = "This machine is offline";
/// The row that closes the sheet.
pub(crate) const CANCEL_LABEL: &str = "Cancel";
/// What the sheet of a session it cannot rerun says in place of its rows.
pub(crate) const NO_RESUME_TEXT: &str = "This session can't be resumed";

/// A tap inside the sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SheetTap {
    Stop,
    Resume,
    /// Anywhere in the sheet that is not one of its rows.
    Elsewhere,
}

/// What a tap asks the phone to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SheetStep {
    Nothing,
    /// Send the session's stop and close the sheet.
    SendStop,
    /// Send the session's resume.
    SendResume,
}

/// The open sheet: the session it is for, whether Stop is armed, whether
/// its resume is out, and why the daemon refused the last one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sheet {
    pub(crate) host: HostKey,
    pub(crate) session_id: String,
    /// What the sheet offers as last drawn; a tap on a row it no longer
    /// offers does nothing.
    offer: Offer,
    armed: bool,
    resuming: bool,
    refusal: Option<String>,
}

impl Sheet {
    /// The sheet of `host`'s session `session_id` offering `offer`,
    /// nothing armed.
    pub(crate) fn new(host: HostKey, session_id: &str, offer: Offer) -> Self {
        Self {
            host,
            session_id: session_id.to_owned(),
            offer,
            armed: false,
            resuming: false,
            refusal: None,
        }
    }

    /// The session's offer is now `offer`: Stop's arm goes with a Stop
    /// offer that is gone, as when the session stopped on its own.
    pub(crate) fn see(&mut self, offer: Offer) {
        if offer != Offer::Stop {
            self.armed = false;
        }
        self.offer = offer;
    }

    /// Whether the sheet is `host`'s session `session_id`'s.
    pub(crate) fn is_for(&self, host: &HostKey, session_id: &str) -> bool {
        self.host == *host && self.session_id == session_id
    }

    /// Whether the next tap on Stop sends it; the view reads it as
    /// [`Self::stop_label`].
    #[cfg(test)]
    pub(crate) fn stop_armed(&self) -> bool {
        self.armed
    }

    /// What Stop's row reads now.
    pub(crate) fn stop_label(&self) -> &'static str {
        if self.armed {
            ARMED_STOP_LABEL
        } else {
            STOP_LABEL
        }
    }

    /// Takes `tap`: Stop arms on the first tap and sends on the second;
    /// any other tap disarms it; Resume sends unless one is already out. A
    /// tap on a row the sheet does not offer does nothing.
    pub(crate) fn tap(&mut self, tap: SheetTap) -> SheetStep {
        let armed = std::mem::replace(&mut self.armed, false);
        let stop = self.offer == Offer::Stop;
        let resume = self.offer == Offer::Resume;
        match tap {
            SheetTap::Stop if !stop => SheetStep::Nothing,
            SheetTap::Stop if armed => SheetStep::SendStop,
            SheetTap::Stop => {
                self.armed = true;
                SheetStep::Nothing
            }
            SheetTap::Resume if !resume || self.resuming => SheetStep::Nothing,
            SheetTap::Resume => {
                self.refusal = None;
                SheetStep::SendResume
            }
            SheetTap::Elsewhere => SheetStep::Nothing,
        }
    }

    /// The resume went out; another waits for its reply.
    pub(crate) fn resume_sent(&mut self) {
        self.resuming = true;
    }

    /// The daemon refused the resume with `message`.
    pub(crate) fn refused(&mut self, message: String) {
        self.resuming = false;
        self.refusal = Some(message);
    }

    /// The resume out was dropped unanswered, as when its host reconnected.
    pub(crate) fn resume_dropped(&mut self) {
        self.resuming = false;
    }

    /// What Resume's row reads now: inert while a resume is out.
    pub(crate) fn resume_label(&self) -> &'static str {
        if self.resuming {
            RESUMING_LABEL
        } else {
            RESUME_LABEL
        }
    }

    /// Whether a resume is out.
    pub(crate) fn resuming(&self) -> bool {
        self.resuming
    }

    /// Why the daemon refused the last resume, until the next one.
    pub(crate) fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
    }
}

/// A reply to a request the phone registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PhoneReply {
    /// The new session's `SessionUpdated` echoed the request id.
    Created,
    /// An `Error` or `ActionFailed` carried it, with what it said.
    Refused(String),
}

/// The replies to the requests the phone has out, by request id. The root
/// view folds every daemon message through [`Self::on_message`], which
/// records only a reply to an id the phone registered with that id's host,
/// and forgets the id once it is answered or its host says `Welcome` again;
/// the phone drains what came. The phone keeps what each request was for.
#[derive(Debug, Default)]
pub(crate) struct PhoneReplies {
    out: HashMap<String, HostKey>,
    came: Vec<(String, PhoneReply)>,
}

impl PhoneReplies {
    /// The phone sent `host` a request carrying `request_id`.
    pub(crate) fn register(&mut self, host: &HostKey, request_id: &str) {
        self.out.insert(request_id.to_owned(), host.clone());
    }

    /// Forgets every id out to `host`, whose connection closed or started
    /// over, so no reply to them is recorded.
    pub(crate) fn forget_host(&mut self, host: &HostKey) {
        self.out.retain(|_, out_host| out_host != host);
    }

    /// Whether `request_id` is still waiting for its reply.
    pub(crate) fn is_out(&self, request_id: &str) -> bool {
        self.out.contains_key(request_id)
    }

    /// Folds `msg` from `host`; returns whether it recorded a reply.
    pub(crate) fn on_message(&mut self, host: &HostKey, msg: &DaemonMessage) -> bool {
        match msg {
            DaemonMessage::Welcome { .. } => {
                self.forget_host(host);
                false
            }
            DaemonMessage::SessionUpdated {
                request_id: Some(id),
                ..
            } => self.record(host, id, PhoneReply::Created),
            DaemonMessage::Error {
                message,
                request_id: Some(id),
                ..
            } => self.record(host, id, PhoneReply::Refused(message.clone())),
            DaemonMessage::ActionFailed {
                title,
                detail,
                request_id: Some(id),
                ..
            } => {
                let text = if detail.is_empty() { title } else { detail };
                self.record(host, id, PhoneReply::Refused(text.clone()))
            }
            _ => false,
        }
    }

    /// Records `reply` when `request_id` is out to `host`.
    fn record(&mut self, host: &HostKey, request_id: &str, reply: PhoneReply) -> bool {
        if self.out.get(request_id) != Some(host) {
            return false;
        }
        self.out.remove(request_id);
        self.came.push((request_id.to_owned(), reply));
        true
    }

    /// The replies that came since the last drain, oldest first.
    pub(crate) fn drain(&mut self) -> Vec<(String, PhoneReply)> {
        std::mem::take(&mut self.came)
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test fixtures")]
mod tests {
    use super::*;
    use crate::target::Target;
    use protocol::{DaemonMessage, SessionSnapshot};
    use serde_json::json;

    fn host() -> HostKey {
        HostKey::of(&Target::tailnet("box.tail1234.ts.net", Some(8787)))
    }

    fn other_host() -> HostKey {
        HostKey::of(&Target::tailnet("den.tail1234.ts.net", Some(8787)))
    }

    #[test]
    fn an_armed_stop_clears_when_the_session_stops_on_its_own() {
        let mut sheet = Sheet::new(host(), "s1", Offer::Stop);
        sheet.tap(SheetTap::Stop);
        assert!(sheet.stop_armed());

        sheet.see(Offer::Resume);

        assert!(!sheet.stop_armed(), "the arm goes with the Stop offer");
        assert_eq!(
            sheet.tap(SheetTap::Stop),
            SheetStep::Nothing,
            "a tap on the old Stop slot does nothing"
        );
        assert!(!sheet.stop_armed(), "nor arms again");
        assert_eq!(sheet.tap(SheetTap::Resume), SheetStep::SendResume);
    }

    #[test]
    fn a_closed_host_offers_no_row() {
        let mut sheet = Sheet::new(host(), "s1", Offer::Offline);
        assert_eq!(sheet.tap(SheetTap::Stop), SheetStep::Nothing);
        assert_eq!(sheet.tap(SheetTap::Stop), SheetStep::Nothing);
        assert_eq!(sheet.tap(SheetTap::Resume), SheetStep::Nothing);
    }

    #[test]
    fn stop_needs_two_taps() {
        let mut sheet = Sheet::new(host(), "s1", Offer::Stop);
        assert_eq!(sheet.stop_label(), STOP_LABEL);

        assert_eq!(
            sheet.tap(SheetTap::Stop),
            SheetStep::Nothing,
            "the first arms"
        );
        assert!(sheet.stop_armed());
        assert_eq!(sheet.stop_label(), ARMED_STOP_LABEL);

        assert_eq!(
            sheet.tap(SheetTap::Stop),
            SheetStep::SendStop,
            "the second sends"
        );
        assert!(!sheet.stop_armed());
    }

    #[test]
    fn another_tap_disarms_stop() {
        let mut sheet = Sheet::new(host(), "s1", Offer::Stop);
        sheet.tap(SheetTap::Stop);

        assert_eq!(sheet.tap(SheetTap::Elsewhere), SheetStep::Nothing);
        assert!(!sheet.stop_armed(), "a tap elsewhere disarms");
        assert_eq!(
            sheet.tap(SheetTap::Stop),
            SheetStep::Nothing,
            "so Stop arms again"
        );

        assert_eq!(
            sheet.tap(SheetTap::Resume),
            SheetStep::Nothing,
            "a running session has no Resume to send"
        );
        assert!(!sheet.stop_armed(), "but the tap disarms too");
    }

    #[test]
    fn a_resume_out_ignores_another_and_a_refusal_shows() {
        let mut sheet = Sheet::new(host(), "s1", Offer::Resume);
        assert_eq!(sheet.tap(SheetTap::Resume), SheetStep::SendResume);
        sheet.resume_sent();
        assert_eq!(
            sheet.tap(SheetTap::Resume),
            SheetStep::Nothing,
            "one resume at a time"
        );

        sheet.refused("no such session".to_owned());
        assert_eq!(sheet.refusal(), Some("no such session"));
        assert_eq!(
            sheet.tap(SheetTap::Resume),
            SheetStep::SendResume,
            "a refusal lets the owner try again"
        );
        assert_eq!(sheet.refusal(), None, "and the new try clears it");
    }

    fn updated(request_id: Option<&str>) -> DaemonMessage {
        let session: SessionSnapshot = serde_json::from_value(json!({
            "id": "new",
            "label": "new",
            "kind": "single",
            "members": [],
            "status": "working",
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cost_usd": 0.0,
                "last_activity_at": null,
            },
            "recent_actions": [],
            "agent": "claude",
        }))
        .expect("session fixture");
        DaemonMessage::SessionUpdated {
            session,
            request_id: request_id.map(str::to_owned),
        }
    }

    fn refused(request_id: &str) -> DaemonMessage {
        DaemonMessage::Error {
            message: "refused".to_owned(),
            request_id: Some(request_id.to_owned()),
            code: None,
        }
    }

    #[test]
    fn a_registered_reply_is_recorded() {
        let mut replies = PhoneReplies::default();
        replies.register(&host(), "r1");

        assert!(replies.on_message(&host(), &updated(Some("r1"))));
        assert!(!replies.is_out("r1"), "an answered id is no longer out");
        assert_eq!(replies.drain(), [("r1".to_owned(), PhoneReply::Created)]);
    }

    #[test]
    fn an_unregistered_reply_is_ignored() {
        let mut replies = PhoneReplies::default();
        replies.register(&host(), "r1");

        assert!(!replies.on_message(&host(), &updated(None)));
        assert!(!replies.on_message(&host(), &updated(Some("other"))));
        assert!(!replies.on_message(&host(), &refused("other")));
        assert!(
            !replies.on_message(&other_host(), &updated(Some("r1"))),
            "another host's echo of the id is not the reply"
        );
        assert_eq!(replies.drain(), Vec::new());
        assert!(replies.is_out("r1"));
    }

    #[test]
    fn an_error_is_recorded_as_a_refusal() {
        let mut replies = PhoneReplies::default();
        replies.register(&host(), "r1");

        assert!(replies.on_message(&host(), &refused("r1")));
        assert_eq!(
            replies.drain(),
            [("r1".to_owned(), PhoneReply::Refused("refused".to_owned()))]
        );
    }

    #[test]
    fn replies_drain_once() {
        let mut replies = PhoneReplies::default();
        replies.register(&host(), "r1");
        replies.on_message(&host(), &updated(Some("r1")));

        assert_eq!(replies.drain().len(), 1);
        assert_eq!(replies.drain(), Vec::new(), "a drained reply is gone");
        assert!(
            !replies.on_message(&host(), &updated(Some("r1"))),
            "and its id is forgotten"
        );
    }

    #[test]
    fn a_welcome_forgets_the_hosts_ids() {
        let mut replies = PhoneReplies::default();
        replies.register(&host(), "r1");
        replies.register(&other_host(), "r2");

        let welcome: DaemonMessage =
            serde_json::from_value(json!({ "type": "welcome", "protocol_version": 1 }))
                .expect("welcome fixture");
        assert!(!replies.on_message(&host(), &welcome));

        assert!(
            !replies.is_out("r1"),
            "the reconnected host's id is forgotten"
        );
        assert!(replies.is_out("r2"), "another host's stays");
    }
}
