//! What the client tells the user outside the panes: short-lived toasts, the
//! blocking notice of a refused action, and the prompt before an in-place
//! checkout switches a dirty working tree.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use protocol::CheckoutStrategy;

/// How long a toast stays before it goes by itself.
pub const TOAST_LIFETIME: Duration = Duration::from_secs(8);
/// The most toasts on screen at once; a toast arriving while this many are
/// shown pushes out the timed one that has waited longest, and stickies alone
/// may exceed it.
pub const MAX_TOASTS: usize = 3;
/// The checkout prompt's heading.
pub const CHECKOUT_TITLE: &str = "Switch branch in place?";

/// What a toast reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Warning,
    Error,
}

/// A notice in the corner that goes by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    /// Unique among this run's toasts.
    pub id: u64,
    pub kind: ToastKind,
    pub title: String,
    pub detail: Option<String>,
    /// A later push of this key updates this toast in place.
    pub key: Option<String>,
    /// A sticky toast never goes by itself; only the × closes it.
    pub sticky: bool,
    /// The buttons under its text, in order.
    pub actions: Vec<ToastAction>,
    expires_at: Instant,
}

/// A button on a toast. A click hands the toast's key and the action's id
/// to whoever raised the toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToastAction {
    /// Names the action to its handler and in the button's selector.
    pub id: &'static str,
    pub label: &'static str,
    /// Drawn as a destructive action.
    pub danger: bool,
}

/// The selector of action `action`'s button on toast `toast_id`.
#[must_use]
pub fn toast_action_selector(toast_id: u64, action: &str) -> String {
    format!("toast-action-{toast_id}-{action}")
}

/// The toast for [`protocol::DaemonMessage::ResumeReport`]: `resumed`
/// sessions came back after the restart, `left` stayed for Recover. A report
/// that resumed nothing is a warning, since the sessions the last run lost
/// are still waiting.
#[must_use]
pub fn resume_report_toast(resumed: u32, left: u32) -> ToastSpec {
    let detail = match left {
        0 => None,
        1 => Some("1 stays in Recover".to_owned()),
        n => Some(format!("{n} stay in Recover")),
    };
    let (kind, title) = match resumed {
        0 => (
            ToastKind::Warning,
            "No sessions resumed after restart".to_owned(),
        ),
        1 => (
            ToastKind::Info,
            "Resumed 1 session after restart".to_owned(),
        ),
        n => (
            ToastKind::Info,
            format!("Resumed {n} sessions after restart"),
        ),
    };
    ToastSpec {
        kind,
        title,
        detail,
        key: Some("resume-report".to_owned()),
        sticky: false,
        actions: Vec::new(),
    }
}

/// The toast for [`protocol::DaemonMessage::SessionNotice`]: what the
/// daemon did, or would not do, to session `session_id`'s terminal. `label`
/// is the session agent's descriptor label; with none the toast says "The
/// agent". A notice this build does not know raises none.
#[must_use]
pub fn session_notice_toast(
    session_id: &str,
    label: Option<&str>,
    notice: &protocol::SessionNoticeKind,
) -> Option<ToastSpec> {
    let title = match notice {
        protocol::SessionNoticeKind::WaitingInMain => format!(
            "{} is waiting in the main conversation.",
            label.unwrap_or("The agent")
        ),
        protocol::SessionNoticeKind::DraftReturnedToMain { agent } => {
            format!("Your draft for {agent} is back in the main conversation.")
        }
        protocol::SessionNoticeKind::Unknown => return None,
    };
    Some(ToastSpec {
        kind: ToastKind::Info,
        title,
        detail: None,
        key: Some(format!("session-notice:{session_id}")),
        sticky: false,
        actions: Vec::new(),
    })
}

/// The key prefix of a spawn's "Spawning session…" toast.
const SPAWNING_PREFIX: &str = "spawning:";

/// The key of the "Spawning session…" toast of the spawn sent under
/// `request_id`: a resend of that spawn updates the toast in place, and the
/// daemon's answer to it clears that toast alone.
#[must_use]
pub fn spawning_key(request_id: &str) -> String {
    format!("{SPAWNING_PREFIX}{request_id}")
}

/// A toast to show: what it reports, and what it does when a toast of the
/// same key is already on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastSpec {
    pub kind: ToastKind,
    pub title: String,
    pub detail: Option<String>,
    /// Updates the toast of this key in place when one is shown; None appends
    /// a new toast.
    pub key: Option<String>,
    /// Never goes by itself.
    pub sticky: bool,
    /// The buttons under its text, in order.
    pub actions: Vec<ToastAction>,
}

/// A refused or failed action that blocks until the user dismisses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionFailedNotice {
    pub title: String,
    /// May span several lines; shown as written.
    pub detail: String,
    /// An actionable next step, shown muted.
    pub hint: Option<String>,
}

/// The notice an action shows when the spawn dialog it needs cannot open:
/// `title` names the action the user asked for, and the detail carries
/// `reason`, the blocker that kept the dialog shut.
#[must_use]
pub fn spawn_refusal(title: &str, reason: &str) -> ActionFailedNotice {
    ActionFailedNotice {
        title: title.to_owned(),
        detail: format!("Couldn't open the spawn dialog: {reason}"),
        hint: None,
    }
}

/// A button of the checkout prompt, in the order it shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutChoice {
    Cancel,
    Stash,
    Carry,
}

impl CheckoutChoice {
    pub const ALL: [Self; 3] = [Self::Cancel, Self::Stash, Self::Carry];

    #[must_use]
    pub fn selector(self) -> &'static str {
        match self {
            Self::Cancel => "checkout-cancel",
            Self::Stash => "checkout-stash",
            Self::Carry => "checkout-carry",
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Cancel => "Cancel",
            Self::Stash => "Stash & switch",
            Self::Carry => "Carry changes",
        }
    }

    /// How the retried spawn resolves the dirty tree; Cancel retries nothing.
    #[must_use]
    pub fn strategy(self) -> Option<CheckoutStrategy> {
        match self {
            Self::Cancel => None,
            Self::Stash => Some(CheckoutStrategy::Stash),
            Self::Carry => Some(CheckoutStrategy::Carry),
        }
    }
}

/// A `CheckoutConfirmRequired` as the daemon sent it, before it is resolved
/// to a pending spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckoutAsk {
    pub repo_id: String,
    pub branch: String,
    pub dirty_count: u32,
    /// The declined spawn's request id; None from an older daemon.
    pub request_id: Option<String>,
}

impl CheckoutAsk {
    #[must_use]
    pub(crate) fn new(
        repo_id: String,
        branch: String,
        dirty_count: u32,
        request_id: Option<String>,
    ) -> Self {
        Self {
            repo_id,
            branch,
            dirty_count,
            request_id,
        }
    }
}

/// The daemon declined an in-place spawn that would switch a dirty working
/// tree to another branch, and asks how to go on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutPrompt {
    pub repo_id: String,
    pub branch: String,
    pub dirty_count: u32,
    /// The spawn this prompt answers, when a pending one matches it.
    pub request_id: Option<String>,
    focused: CheckoutChoice,
}

impl CheckoutPrompt {
    /// The button Enter and Space press.
    #[must_use]
    pub fn focused(&self) -> CheckoutChoice {
        self.focused
    }

    /// Moves the focus to the next button, or the previous one, wrapping.
    pub fn move_focus(&mut self, forward: bool) {
        let all = CheckoutChoice::ALL;
        let at = all.iter().position(|c| *c == self.focused).unwrap_or(0);
        let next = if forward {
            (at + 1) % all.len()
        } else {
            (at + all.len() - 1) % all.len()
        };
        self.focused = all[next];
    }

    /// What the switch would do.
    #[must_use]
    pub fn message(&self) -> String {
        let plural = if self.dirty_count == 1 { "" } else { "s" };
        format!(
            "This repo has {} uncommitted change{plural} and isn't on {}. Switching in place \
             changes what's checked out in your working directory.",
            self.dirty_count, self.branch
        )
    }

    /// What each way through does.
    #[must_use]
    pub fn choice_notes(&self) -> [String; 2] {
        [
            format!(
                "Carry changes — keep your edits and switch (git refuses if they'd conflict with \
                 {}).",
                self.branch
            ),
            "Stash & switch — stash your edits first, then switch; pop the stash yourself \
             afterward."
                .to_owned(),
        ]
    }
}

/// The toasts on screen and the two modal notices.
#[derive(Debug, Default)]
pub struct Notices {
    toasts: Vec<Toast>,
    next_id: u64,
    action_failed: Option<ActionFailedNotice>,
    /// The checkout prompt on screen. It and [`Self::reask`] share one slot:
    /// at most one of them is set.
    checkout: Option<CheckoutPrompt>,
    /// The spawn asked again for a fresh prompt, whose answer has not come
    /// back yet.
    reask: Option<String>,
    /// The spawns of the prompts waiting for the slot, oldest first. Their
    /// measured numbers are not kept: each is asked again in turn.
    waiting: VecDeque<String>,
    /// Prompts answered in this batch, for the heading's count.
    answered: usize,
}

impl Notices {
    /// Shows a toast until [`TOAST_LIFETIME`] after `now`, pushing out the
    /// timed toast that has waited longest when [`MAX_TOASTS`] are shown;
    /// returns its id.
    pub fn push(
        &mut self,
        kind: ToastKind,
        title: impl Into<String>,
        detail: Option<String>,
        now: Instant,
    ) -> u64 {
        self.push_spec(
            ToastSpec {
                kind,
                title: title.into(),
                detail,
                key: None,
                sticky: false,
                actions: Vec::new(),
            },
            now,
        )
    }

    /// Shows `spec`. A key matching a toast on screen updates that toast in
    /// place — same id, same place, its kind, title, detail, stickiness and
    /// actions replaced — and restarts its lifetime unless it is now sticky; any
    /// other push appends a toast, and one arriving while [`MAX_TOASTS`] are
    /// already shown evicts the timed toast among them that has waited
    /// longest, an update counting as a wait's end. A toast is never its own
    /// victim, so stickies alone may exceed the cap. Returns the toast's id.
    pub fn push_spec(&mut self, spec: ToastSpec, now: Instant) -> u64 {
        let ToastSpec {
            kind,
            title,
            detail,
            key,
            sticky,
            actions,
        } = spec;
        if let Some(at) = key.as_deref().and_then(|key| self.keyed_at(key)) {
            let toast = &mut self.toasts[at];
            toast.kind = kind;
            toast.title = title;
            toast.detail = detail;
            toast.sticky = sticky;
            toast.actions = actions;
            toast.expires_at = now + TOAST_LIFETIME;
            return toast.id;
        }
        if self.toasts.len() >= MAX_TOASTS {
            self.evict_oldest_timed();
        }
        self.next_id += 1;
        let id = self.next_id;
        self.toasts.push(Toast {
            id,
            kind,
            title,
            detail,
            key,
            sticky,
            actions,
            expires_at: now + TOAST_LIFETIME,
        });
        id
    }

    /// Replaces the buttons of the toast of `key`, leaving its lifetime as
    /// it runs; returns whether they changed.
    pub fn set_actions(&mut self, key: &str, actions: Vec<ToastAction>) -> bool {
        let Some(at) = self.keyed_at(key) else {
            return false;
        };
        let toast = &mut self.toasts[at];
        if toast.actions == actions {
            return false;
        }
        toast.actions = actions;
        true
    }

    /// Where the toast of `key` shows, if one does.
    fn keyed_at(&self, key: &str) -> Option<usize> {
        self.toasts
            .iter()
            .position(|toast| toast.key.as_deref() == Some(key))
    }

    /// Drops the timed toast that has waited longest — its last push, or its
    /// last update, is what the deadline measures. Stickies are left, so a
    /// new toast shows over a screenful of stickies rather than evicting one.
    fn evict_oldest_timed(&mut self) {
        let oldest = self
            .toasts
            .iter()
            .enumerate()
            .filter(|(_, toast)| !toast.sticky)
            .min_by_key(|(_, toast)| toast.expires_at)
            .map(|(at, _)| at);
        if let Some(at) = oldest {
            self.toasts.remove(at);
        }
    }

    /// Oldest first.
    #[must_use]
    pub fn toasts(&self) -> &[Toast] {
        &self.toasts
    }

    /// The × on toast `id`; returns whether it was shown.
    pub fn dismiss(&mut self, id: u64) -> bool {
        let before = self.toasts.len();
        self.toasts.retain(|toast| toast.id != id);
        self.toasts.len() != before
    }

    /// Drops the toast of `key`; returns whether it was shown.
    pub fn dismiss_key(&mut self, key: &str) -> bool {
        let before = self.toasts.len();
        self.toasts
            .retain(|toast| toast.key.as_deref() != Some(key));
        self.toasts.len() != before
    }

    /// Drops the timed toasts whose time is up at `now`; returns whether any
    /// went. A sticky toast has no time to be up.
    pub fn expire(&mut self, now: Instant) -> bool {
        let before = self.toasts.len();
        self.toasts
            .retain(|toast| toast.sticky || toast.expires_at > now);
        self.toasts.len() != before
    }

    /// When the next toast goes; None while every toast shown is sticky.
    #[must_use]
    pub fn next_expiry(&self) -> Option<Instant> {
        self.toasts
            .iter()
            .filter(|toast| !toast.sticky)
            .map(|toast| toast.expires_at)
            .min()
    }

    /// Shows the notice in place of the one shown.
    pub fn show_action_failed(
        &mut self,
        title: impl Into<String>,
        detail: String,
        hint: Option<String>,
    ) {
        self.action_failed = Some(ActionFailedNotice {
            title: title.into(),
            detail,
            hint,
        });
    }

    #[must_use]
    pub fn action_failed(&self) -> Option<&ActionFailedNotice> {
        self.action_failed.as_ref()
    }

    /// Returns whether it was shown.
    pub fn close_action_failed(&mut self) -> bool {
        self.action_failed.take().is_some()
    }

    /// The prompt that arrived, its `request_id` being the pending spawn it
    /// resolved to; returns whether it shows now, with Cancel focused. It
    /// shows when the slot is free, or when it answers the spawn asked again,
    /// which holds the slot; otherwise its spawn waits at the back of the
    /// queue, to be asked again in turn. A prompt that resolved to no spawn
    /// cannot be asked again, so it is dropped rather than queued.
    pub fn ask_checkout(&mut self, ask: CheckoutAsk) -> bool {
        let CheckoutAsk {
            repo_id,
            branch,
            dirty_count,
            request_id,
        } = ask;
        let answers_reask = request_id.is_some() && request_id == self.reask;
        let slot_free = self.checkout.is_none() && self.reask.is_none();
        if !answers_reask && !slot_free {
            if let Some(request_id) = request_id
                && !self.waiting.contains(&request_id)
            {
                self.waiting.push_back(request_id);
            }
            return false;
        }
        self.reask = None;
        self.waiting.retain(|id| Some(id) != request_id.as_ref());
        self.checkout = Some(CheckoutPrompt {
            repo_id,
            branch,
            dirty_count,
            request_id,
            focused: CheckoutChoice::Cancel,
        });
        true
    }

    #[must_use]
    pub fn checkout(&self) -> Option<&CheckoutPrompt> {
        self.checkout.as_ref()
    }

    pub fn checkout_mut(&mut self) -> Option<&mut CheckoutPrompt> {
        self.checkout.as_mut()
    }

    /// The prompt on screen is answered and frees the slot; the caller then
    /// runs [`Self::pump`].
    pub fn close_checkout(&mut self) -> Option<CheckoutPrompt> {
        let shown = self.checkout.take()?;
        self.answered += 1;
        Some(shown)
    }

    /// The spawn `request_id` was placed or failed: a prompt waiting for it
    /// leaves the queue, and a re-ask of it frees the slot. The caller then
    /// runs [`Self::pump`].
    pub fn spawn_settled(&mut self, request_id: &str) {
        self.waiting.retain(|id| id != request_id);
        if self.reask.as_deref() == Some(request_id) {
            self.reask = None;
        }
    }

    /// Fills a free slot: the oldest waiting prompt's spawn is asked again by
    /// `resend`, which returns the message for a spawn still pending and None
    /// for one that is gone, dropped here and the next one tried. Returns the
    /// message to send. Once the slot is free and nothing waits, the batch
    /// is over and the count starts again.
    pub fn pump<M>(&mut self, mut resend: impl FnMut(&str) -> Option<M>) -> Option<M> {
        if self.checkout.is_some() || self.reask.is_some() {
            return None;
        }
        while let Some(request_id) = self.waiting.pop_front() {
            if let Some(msg) = resend(&request_id) {
                self.reask = Some(request_id);
                return Some(msg);
            }
        }
        self.answered = 0;
        None
    }

    /// The heading of the prompt on screen, counting this batch: the prompts
    /// answered, the shown one and those waiting. None when none is shown. A
    /// lone prompt reads [`CHECKOUT_TITLE`] alone.
    #[must_use]
    pub fn checkout_title(&self) -> Option<String> {
        self.checkout.as_ref()?;
        let total = self.answered + 1 + self.waiting.len();
        Some(if total > 1 {
            format!("{CHECKOUT_TITLE} ({} of {total})", self.answered + 1)
        } else {
            CHECKOUT_TITLE.to_owned()
        })
    }

    /// Whether a modal notice is open or a re-ask holds the checkout prompt's
    /// place: either way the notice focus holds the keyboard.
    #[must_use]
    pub fn has_modal(&self) -> bool {
        self.action_failed.is_some() || self.checkout.is_some() || self.reask.is_some()
    }

    /// Whether a spawn is being asked again for a fresh prompt; its keys go
    /// nowhere until the answer comes back.
    #[must_use]
    pub fn reasking(&self) -> bool {
        self.reask.is_some()
    }

    /// Closes the checkout prompts `closes` claims — the one on screen, the
    /// re-ask on its way and those waiting — leaving the rest in place and in
    /// order; returns whether any closed. A prompt with no request id is a
    /// prompt no pending spawn answers, and `closes` is asked with None.
    pub fn close_checkouts(&mut self, mut closes: impl FnMut(Option<&str>) -> bool) -> bool {
        let mut closed = false;
        if self
            .checkout
            .as_ref()
            .is_some_and(|ask| closes(ask.request_id.as_deref()))
        {
            self.checkout = None;
            closed = true;
        }
        if self.reask.as_deref().is_some_and(|id| closes(Some(id))) {
            self.reask = None;
            closed = true;
        }
        let before = self.waiting.len();
        self.waiting.retain(|id| !closes(Some(id)));
        closed |= self.waiting.len() != before;
        if closed && !self.has_modal() {
            self.answered = 0;
        }
        closed
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn titles(notices: &Notices) -> Vec<&str> {
        notices.toasts().iter().map(|t| t.title.as_str()).collect()
    }

    /// A toast spec of `kind` and `title`, with no detail, key or stickiness.
    fn spec(kind: ToastKind, title: &str) -> ToastSpec {
        ToastSpec {
            kind,
            title: title.to_owned(),
            detail: None,
            key: None,
            sticky: false,
            actions: Vec::new(),
        }
    }

    /// A keyed toast spec, to be updated in place by a later push of the key.
    fn keyed(title: &str) -> ToastSpec {
        ToastSpec {
            key: Some("k".to_owned()),
            ..spec(ToastKind::Info, title)
        }
    }

    /// A sticky toast spec of `title`.
    fn sticky(title: &str) -> ToastSpec {
        ToastSpec {
            sticky: true,
            ..spec(ToastKind::Info, title)
        }
    }

    #[test]
    fn resume_report_toast_plural_with_left() {
        let toast = resume_report_toast(2, 1);
        assert_eq!(toast.kind, ToastKind::Info);
        assert_eq!(toast.title, "Resumed 2 sessions after restart");
        assert_eq!(toast.detail.as_deref(), Some("1 stays in Recover"));
        assert_eq!(
            toast.key.as_deref(),
            Some("resume-report"),
            "a later report updates this toast in place"
        );
        assert!(!toast.sticky, "it goes by itself");
        assert!(toast.actions.is_empty(), "it offers no action");
    }

    #[test]
    fn resume_report_toast_singular_without_left() {
        let toast = resume_report_toast(1, 0);
        assert_eq!(toast.kind, ToastKind::Info);
        assert_eq!(toast.title, "Resumed 1 session after restart");
        assert_eq!(toast.detail, None, "nothing stayed in Recover");
    }

    #[test]
    fn resume_report_toast_nothing_resumed_is_a_warning() {
        let toast = resume_report_toast(0, 3);
        assert_eq!(toast.kind, ToastKind::Warning);
        assert_eq!(toast.title, "No sessions resumed after restart");
        assert_eq!(toast.detail.as_deref(), Some("3 stay in Recover"));
    }

    #[test]
    fn session_notice_toast_titles() {
        use protocol::SessionNoticeKind;
        let waiting =
            session_notice_toast("s1", Some("Test agent"), &SessionNoticeKind::WaitingInMain)
                .expect("a known notice raises a toast");
        assert_eq!(
            waiting.title,
            "Test agent is waiting in the main conversation."
        );
        assert_eq!(waiting.kind, ToastKind::Info);
        assert_eq!(waiting.key.as_deref(), Some("session-notice:s1"));
        assert!(!waiting.sticky && waiting.actions.is_empty());
        assert_eq!(
            session_notice_toast("s1", None, &SessionNoticeKind::WaitingInMain)
                .expect("a toast")
                .title,
            "The agent is waiting in the main conversation."
        );
        assert_eq!(
            session_notice_toast(
                "s2",
                Some("Test agent"),
                &SessionNoticeKind::DraftReturnedToMain {
                    agent: "Sleeper A".to_owned()
                }
            )
            .expect("a toast")
            .title,
            "Your draft for Sleeper A is back in the main conversation."
        );
        assert_eq!(
            session_notice_toast("s1", Some("Test agent"), &SessionNoticeKind::Unknown),
            None
        );
    }

    #[test]
    fn toasts_expire_after_eight_seconds() {
        let start = Instant::now();
        let mut notices = Notices::default();
        notices.push(
            ToastKind::Error,
            "Daemon error",
            Some("boom".to_owned()),
            start,
        );
        assert_eq!(notices.next_expiry(), Some(start + TOAST_LIFETIME));

        assert!(!notices.expire(start + Duration::from_millis(7_999)));
        assert_eq!(titles(&notices), ["Daemon error"]);
        let toast = notices.toasts().first().expect("one toast");
        assert_eq!(toast.kind, ToastKind::Error);
        assert_eq!(toast.detail.as_deref(), Some("boom"));

        assert!(notices.expire(start + Duration::from_secs(8)));
        assert_eq!(notices.toasts(), []);
        assert_eq!(notices.next_expiry(), None);
    }

    #[test]
    fn fourth_toast_drops_the_oldest() {
        let start = Instant::now();
        let mut notices = Notices::default();
        for (i, title) in ["one", "two", "three", "four"].into_iter().enumerate() {
            let at = start + Duration::from_secs(u64::try_from(i).expect("small"));
            notices.push(ToastKind::Info, title, None, at);
        }
        assert_eq!(titles(&notices), ["two", "three", "four"]);
        assert_eq!(
            notices.next_expiry(),
            Some(start + Duration::from_secs(1) + TOAST_LIFETIME),
            "the oldest shown toast goes first"
        );
    }

    #[test]
    fn dismissed_toast_is_removed() {
        let now = Instant::now();
        let mut notices = Notices::default();
        let first = notices.push(ToastKind::Info, "one", None, now);
        let second = notices.push(ToastKind::Info, "two", None, now);
        assert_ne!(first, second);

        assert!(notices.dismiss(first));
        assert_eq!(titles(&notices), ["two"]);
        assert!(!notices.dismiss(first), "a toast goes once");
    }

    #[test]
    fn dismiss_key_drops_only_the_keyed_toast() {
        let now = Instant::now();
        let mut notices = Notices::default();
        let keyed_as = |key: &str, title: &str| ToastSpec {
            key: Some(key.to_owned()),
            ..spec(ToastKind::Info, title)
        };
        notices.push_spec(keyed_as("a", "one"), now);
        notices.push(ToastKind::Info, "plain", None, now + Duration::from_secs(1));
        notices.push_spec(keyed_as("b", "two"), now + Duration::from_secs(2));

        assert!(notices.dismiss_key("a"));
        assert_eq!(titles(&notices), ["plain", "two"], "the rest stay");
        assert_eq!(
            notices.next_expiry(),
            Some(now + Duration::from_secs(1) + TOAST_LIFETIME),
            "the next deadline is the oldest toast left"
        );
        assert!(!notices.dismiss_key("a"), "a toast goes once");
        assert!(!notices.dismiss_key("missing"), "no toast has that key");
        assert_eq!(titles(&notices), ["plain", "two"]);
    }

    #[test]
    fn spawning_key_is_per_request() {
        assert_eq!(spawning_key("r1"), "spawning:r1");
        assert_ne!(spawning_key("r1"), spawning_key("r2"));

        let now = Instant::now();
        let mut notices = Notices::default();
        let spawning = |request_id: &str| ToastSpec {
            key: Some(spawning_key(request_id)),
            ..spec(ToastKind::Info, request_id)
        };
        let first = notices.push_spec(spawning("r1"), now);
        notices.push_spec(spawning("r2"), now);
        assert_eq!(
            notices.push_spec(spawning("r1"), now),
            first,
            "a resend of r1 updates its toast"
        );
        notices.push(ToastKind::Error, "Daemon error", None, now);
        assert_eq!(titles(&notices), ["r1", "r2", "Daemon error"]);
    }

    #[test]
    fn sticky_toast_never_expires() {
        let start = Instant::now();
        let mut notices = Notices::default();
        notices.push_spec(sticky("running"), start);

        assert_eq!(notices.next_expiry(), None, "a sticky toast sets no timer");
        assert!(!notices.expire(start + Duration::from_secs(60)));
        assert_eq!(titles(&notices), ["running"], "it outlasts any lifetime");
        assert!(notices.dismiss(1), "× still closes it");
        assert_eq!(notices.toasts(), []);
    }

    #[test]
    fn keyed_push_updates_in_place() {
        let start = Instant::now();
        let mut notices = Notices::default();
        let first = notices.push_spec(
            ToastSpec {
                detail: Some("one".to_owned()),
                sticky: true,
                ..keyed("A")
            },
            start,
        );
        notices.push_spec(spec(ToastKind::Info, "B"), start + Duration::from_secs(1));
        assert_eq!(titles(&notices), ["A", "B"]);

        let again = notices.push_spec(
            ToastSpec {
                kind: ToastKind::Warning,
                title: "A again".to_owned(),
                detail: Some("two".to_owned()),
                ..keyed("A again")
            },
            start + Duration::from_secs(2),
        );
        assert_eq!(again, first, "a keyed push keeps the toast's id");
        assert_eq!(titles(&notices), ["A again", "B"], "and its place");
        assert_eq!(notices.toasts().len(), 2, "and appends nothing");
        let updated = notices.toasts().first().expect("the updated toast");
        assert_eq!(updated.kind, ToastKind::Warning);
        assert_eq!(updated.detail.as_deref(), Some("two"));
        assert_eq!(updated.key.as_deref(), Some("k"));
        assert!(!updated.sticky, "the update replaces the stickiness");
    }

    const GO: ToastAction = ToastAction {
        id: "go",
        label: "Go",
        danger: false,
    };
    const STOP: ToastAction = ToastAction {
        id: "stop",
        label: "Stop",
        danger: true,
    };

    #[test]
    fn a_keyed_push_replaces_the_actions() {
        let start = Instant::now();
        let mut notices = Notices::default();
        let id = notices.push_spec(
            ToastSpec {
                actions: vec![GO, STOP],
                ..keyed("A")
            },
            start,
        );
        assert_eq!(notices.toasts()[0].actions, [GO, STOP]);
        notices.push_spec(
            ToastSpec {
                actions: vec![GO],
                ..keyed("A")
            },
            start,
        );
        assert_eq!(
            notices.toasts()[0].actions,
            [GO],
            "the update replaces them"
        );
        assert_eq!(
            toast_action_selector(id, "go"),
            format!("toast-action-{id}-go")
        );
    }

    #[test]
    fn setting_actions_keeps_the_toasts_lifetime() {
        let start = Instant::now();
        let mut notices = Notices::default();
        notices.push_spec(keyed("A"), start);
        assert!(notices.set_actions("k", vec![STOP]));
        assert!(!notices.set_actions("k", vec![STOP]), "nothing changed");
        assert!(
            !notices.set_actions("other", vec![GO]),
            "no toast of that key"
        );
        assert_eq!(notices.toasts()[0].actions, [STOP]);
        assert_eq!(
            notices.next_expiry(),
            Some(start + TOAST_LIFETIME),
            "the timer runs on from the push"
        );
    }

    #[test]
    fn keyed_update_restarts_lifetime() {
        let start = Instant::now();
        let mut notices = Notices::default();
        notices.push_spec(keyed("job"), start);
        assert_eq!(notices.next_expiry(), Some(start + TOAST_LIFETIME));

        let update = start + Duration::from_secs(6);
        notices.push_spec(keyed("job still"), update);
        assert_eq!(notices.next_expiry(), Some(update + TOAST_LIFETIME));

        assert!(
            !notices.expire(start + Duration::from_secs(13)),
            "the update restarted its time"
        );
        assert_eq!(titles(&notices), ["job still"]);
        assert!(notices.expire(start + Duration::from_millis(14_100)));
        assert_eq!(notices.toasts(), []);
    }

    #[test]
    fn keyed_push_after_expiry_appends() {
        let start = Instant::now();
        let mut notices = Notices::default();
        let first = notices.push_spec(keyed("one"), start);
        assert!(notices.expire(start + TOAST_LIFETIME));

        let second = notices.push_spec(keyed("two"), start + TOAST_LIFETIME);
        assert_ne!(
            second, first,
            "the keyed toast is gone, so this is a new one"
        );
        assert_eq!(titles(&notices), ["two"]);
    }

    #[test]
    fn cap_evicts_oldest_non_sticky() {
        let start = Instant::now();
        let mut notices = Notices::default();
        notices.push_spec(sticky("pinned"), start);
        for (i, title) in ["one", "two", "three"].into_iter().enumerate() {
            let at = start + Duration::from_secs(u64::try_from(i).expect("small") + 1);
            notices.push_spec(spec(ToastKind::Info, title), at);
        }

        assert_eq!(
            titles(&notices),
            ["pinned", "two", "three"],
            "the sticky one stays and the oldest timed one goes"
        );
    }

    #[test]
    fn cap_counts_a_keyed_update_as_recent() {
        let start = Instant::now();
        let mut notices = Notices::default();
        let first = notices.push_spec(keyed("one"), start);
        notices.push_spec(spec(ToastKind::Info, "two"), start + Duration::from_secs(1));
        notices.push_spec(
            spec(ToastKind::Info, "three"),
            start + Duration::from_secs(2),
        );
        assert_eq!(
            notices.push_spec(keyed("one again"), start + Duration::from_secs(3)),
            first,
            "the update is not a new toast"
        );

        notices.push_spec(
            spec(ToastKind::Info, "four"),
            start + Duration::from_secs(4),
        );
        assert_eq!(
            titles(&notices),
            ["one again", "three", "four"],
            "the update made one the freshest, so two went"
        );
    }

    #[test]
    fn all_sticky_exceeds_cap() {
        let start = Instant::now();
        let mut notices = Notices::default();
        for (i, title) in ["a", "b", "c", "d"].into_iter().enumerate() {
            let at = start + Duration::from_secs(u64::try_from(i).expect("small"));
            notices.push_spec(sticky(title), at);
        }

        assert_eq!(
            titles(&notices),
            ["a", "b", "c", "d"],
            "stickies may exceed the cap"
        );
        assert_eq!(notices.next_expiry(), None);
    }

    #[test]
    fn timed_toast_over_three_stickies_is_shown() {
        let start = Instant::now();
        let mut notices = Notices::default();
        for (i, title) in ["a", "b", "c"].into_iter().enumerate() {
            let at = start + Duration::from_secs(u64::try_from(i).expect("small"));
            notices.push_spec(sticky(title), at);
        }
        notices.push_spec(
            spec(ToastKind::Info, "timed"),
            start + Duration::from_secs(3),
        );

        assert_eq!(
            titles(&notices),
            ["a", "b", "c", "timed"],
            "a new toast is never its own eviction victim"
        );
        assert_eq!(
            notices.next_expiry(),
            Some(start + Duration::from_secs(3) + TOAST_LIFETIME),
            "the timed toast is the one with a time"
        );
    }

    #[test]
    fn action_failed_replaces_the_shown_one() {
        let mut notices = Notices::default();
        assert!(!notices.has_modal());
        notices.show_action_failed("First", "one".to_owned(), None);
        notices.show_action_failed("Second", "two".to_owned(), Some("try again".to_owned()));
        assert_eq!(
            notices.action_failed(),
            Some(&ActionFailedNotice {
                title: "Second".to_owned(),
                detail: "two".to_owned(),
                hint: Some("try again".to_owned()),
            })
        );
        assert!(notices.has_modal());
        assert!(notices.close_action_failed());
        assert_eq!(notices.action_failed(), None);
        assert!(!notices.close_action_failed());
    }

    #[test]
    fn spawn_refusal_names_the_reason() {
        let notice = spawn_refusal(
            "Couldn't review the trusted launch",
            "another dialog is open",
        );
        assert_eq!(
            notice,
            ActionFailedNotice {
                title: "Couldn't review the trusted launch".to_owned(),
                detail: "Couldn't open the spawn dialog: another dialog is open".to_owned(),
                hint: None,
            }
        );
        assert_eq!(
            spawn_refusal(
                "Couldn't edit the launch",
                "the repo is no longer registered"
            ),
            ActionFailedNotice {
                title: "Couldn't edit the launch".to_owned(),
                detail: "Couldn't open the spawn dialog: the repo is no longer registered"
                    .to_owned(),
                hint: None,
            },
            "the title belongs to the caller and the reason to the refusal"
        );
    }

    #[test]
    fn checkout_prompt_starts_on_cancel_and_cycles() {
        let mut notices = Notices::default();
        assert!(notices.ask_checkout(CheckoutAsk::new(
            "r1".to_owned(),
            "feature".to_owned(),
            1,
            None
        )));
        let prompt = notices.checkout_mut().expect("a prompt");
        assert_eq!(prompt.focused(), CheckoutChoice::Cancel);
        assert_eq!(
            prompt.message(),
            "This repo has 1 uncommitted change and isn't on feature. Switching in place changes \
             what's checked out in your working directory."
        );
        prompt.move_focus(true);
        assert_eq!(prompt.focused(), CheckoutChoice::Stash);
        prompt.move_focus(true);
        prompt.move_focus(true);
        assert_eq!(prompt.focused(), CheckoutChoice::Cancel, "Tab wraps");
        prompt.move_focus(false);
        assert_eq!(
            prompt.focused(),
            CheckoutChoice::Carry,
            "Shift+Tab wraps back"
        );

        let closed = notices.close_checkout().expect("the prompt closes");
        assert_eq!(closed.branch, "feature");
        assert_eq!(closed.focused(), CheckoutChoice::Carry, "as it was left");
        assert!(notices.close_checkout().is_none(), "it closes once");

        assert!(notices.ask_checkout(CheckoutAsk::new(
            "r1".to_owned(),
            "main".to_owned(),
            3,
            Some("q1".to_owned())
        )));
        let prompt = notices.checkout().expect("a prompt");
        assert!(
            prompt
                .message()
                .starts_with("This repo has 3 uncommitted changes and isn't on main.")
        );
        assert_eq!(prompt.focused(), CheckoutChoice::Cancel);
        assert!(notices.close_checkouts(|_| true));
        assert!(notices.checkout().is_none());
    }

    /// A prompt of one change for branch `id` of `r1`, resolved to spawn `id`.
    fn ask(notices: &mut Notices, id: &str) -> bool {
        notices.ask_checkout(CheckoutAsk::new(
            "r1".to_owned(),
            id.to_owned(),
            1,
            Some(id.to_owned()),
        ))
    }

    /// Runs `pump` with every spawn still pending; returns the spawn asked
    /// again.
    fn pump_all(notices: &mut Notices) -> Option<String> {
        notices.pump(|id| Some(id.to_owned()))
    }

    fn title(notices: &Notices) -> Option<String> {
        notices.checkout_title()
    }

    #[test]
    fn a_prompt_waits_while_the_slot_is_taken() {
        let mut notices = Notices::default();
        assert!(ask(&mut notices, "a"), "the slot is free");
        assert!(!ask(&mut notices, "b"), "the slot holds A");
        assert_eq!(
            notices.checkout().expect("A shows").branch,
            "a",
            "the prompt on screen is untouched"
        );
        assert_eq!(
            title(&notices).as_deref(),
            Some("Switch branch in place? (1 of 2)")
        );
        assert!(!notices.ask_checkout(CheckoutAsk::new("r1".to_owned(), "z".to_owned(), 1, None)));
        assert_eq!(
            title(&notices).as_deref(),
            Some("Switch branch in place? (1 of 2)"),
            "a prompt of no spawn cannot be asked again, so it is not queued"
        );
        assert_eq!(pump_all(&mut notices), None, "the slot is taken");
        assert_eq!(notices.checkout().expect("A shows").branch, "a");
    }

    #[test]
    fn pump_asks_the_oldest_waiting_spawn_again_and_holds_the_slot() {
        let mut notices = Notices::default();
        ask(&mut notices, "a");
        ask(&mut notices, "b");
        ask(&mut notices, "c");
        assert_eq!(pump_all(&mut notices), None, "A is on screen");

        assert!(notices.close_checkout().is_some());
        assert_eq!(pump_all(&mut notices).as_deref(), Some("b"));
        assert!(title(&notices).is_none(), "nothing on screen");
        assert!(notices.reasking());
        assert!(
            notices.has_modal(),
            "a re-ask on its way holds the keyboard"
        );
        assert_eq!(pump_all(&mut notices), None, "the re-ask holds the slot");

        assert!(!ask(&mut notices, "d"), "D arrives while B's re-ask is out");
        assert!(
            !notices.ask_checkout(CheckoutAsk::new(
                "r1".to_owned(),
                "c".to_owned(),
                9,
                Some("c".to_owned())
            )),
            "C's own prompt still waits its turn"
        );
        assert!(
            notices.ask_checkout(CheckoutAsk::new(
                "r1".to_owned(),
                "b".to_owned(),
                5,
                Some("b".to_owned())
            )),
            "B's fresh prompt shows at once, ahead of C and D"
        );
        assert_eq!(notices.checkout().expect("B shows").dirty_count, 5);
        assert_eq!(
            title(&notices).as_deref(),
            Some("Switch branch in place? (2 of 4)")
        );

        notices.close_checkout();
        assert_eq!(pump_all(&mut notices).as_deref(), Some("c"), "C is older");
        ask(&mut notices, "c");
        notices.close_checkout();
        assert_eq!(pump_all(&mut notices).as_deref(), Some("d"));
        ask(&mut notices, "d");
        assert_eq!(
            title(&notices).as_deref(),
            Some("Switch branch in place? (4 of 4)")
        );
    }

    #[test]
    fn pump_skips_spawns_that_are_gone() {
        let mut notices = Notices::default();
        ask(&mut notices, "a");
        ask(&mut notices, "b");
        ask(&mut notices, "c");
        notices.close_checkout();

        let mut asked = Vec::new();
        let sent = notices.pump(|id| {
            asked.push(id.to_owned());
            (id == "c").then(|| id.to_owned())
        });
        assert_eq!(sent.as_deref(), Some("c"));
        assert_eq!(asked, ["b", "c"], "B is gone, so C is asked");
        ask(&mut notices, "c");
        assert_eq!(
            title(&notices).as_deref(),
            Some("Switch branch in place? (2 of 2)"),
            "B is no longer counted"
        );
    }

    #[test]
    fn a_reask_that_settles_frees_the_slot_for_the_next() {
        let mut notices = Notices::default();
        ask(&mut notices, "a");
        ask(&mut notices, "b");
        ask(&mut notices, "c");
        notices.close_checkout();
        assert_eq!(pump_all(&mut notices).as_deref(), Some("b"));

        notices.spawn_settled("b");
        assert_eq!(
            pump_all(&mut notices).as_deref(),
            Some("c"),
            "B came back as a session, so C's turn comes"
        );
        assert!(ask(&mut notices, "c"));
        assert_eq!(
            title(&notices).as_deref(),
            Some("Switch branch in place? (2 of 2)")
        );
    }

    #[test]
    fn a_waiting_spawn_that_settles_leaves_the_queue() {
        let mut notices = Notices::default();
        ask(&mut notices, "a");
        ask(&mut notices, "b");
        ask(&mut notices, "c");
        notices.spawn_settled("b");
        assert_eq!(
            title(&notices).as_deref(),
            Some("Switch branch in place? (1 of 2)"),
            "B no longer counts"
        );
        notices.close_checkout();
        assert_eq!(
            pump_all(&mut notices).as_deref(),
            Some("c"),
            "B gets no turn"
        );
        notices.spawn_settled("c");
        assert_eq!(pump_all(&mut notices), None, "nothing waits");
    }

    #[test]
    fn the_batch_starts_over_once_the_slot_frees_and_nothing_waits() {
        let mut notices = Notices::default();
        assert!(title(&notices).is_none(), "nothing on screen");
        ask(&mut notices, "a");
        assert_eq!(
            title(&notices).as_deref(),
            Some(CHECKOUT_TITLE),
            "a single prompt"
        );
        ask(&mut notices, "b");
        notices.close_checkout();
        assert_eq!(pump_all(&mut notices).as_deref(), Some("b"));
        notices.spawn_settled("b");
        assert_eq!(pump_all(&mut notices), None);

        ask(&mut notices, "c");
        assert_eq!(
            title(&notices).as_deref(),
            Some(CHECKOUT_TITLE),
            "B failed while re-asked, which ended the batch"
        );
        notices.close_checkout();
        assert_eq!(pump_all(&mut notices), None);

        ask(&mut notices, "d");
        ask(&mut notices, "e");
        assert_eq!(
            title(&notices).as_deref(),
            Some("Switch branch in place? (1 of 2)"),
            "answering the last prompt ended that batch too"
        );
    }

    #[test]
    fn close_checkouts_clears_the_queue_and_the_reask() {
        let mut notices = Notices::default();
        ask(&mut notices, "a");
        ask(&mut notices, "b");
        notices.close_checkout();
        assert_eq!(pump_all(&mut notices).as_deref(), Some("b"));
        ask(&mut notices, "c");
        assert!(
            notices.close_checkouts(|_| true),
            "a re-ask on its way counts as open"
        );
        assert!(!notices.close_checkouts(|_| true), "nothing is open now");

        let mut asked = Vec::new();
        let sent = notices.pump(|id| {
            asked.push(id.to_owned());
            Some(())
        });
        assert_eq!(sent, None);
        assert!(asked.is_empty(), "C's queued spawn went with the rest");

        assert!(ask(&mut notices, "b"), "the slot is free again");
        assert_eq!(
            title(&notices).as_deref(),
            Some(CHECKOUT_TITLE),
            "the counter starts over"
        );
    }
}
