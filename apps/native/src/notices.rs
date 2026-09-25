//! What the client tells the user outside the panes: short-lived toasts, the
//! blocking notice of a refused action, and the prompt before an in-place
//! checkout switches a dirty working tree.

use std::time::{Duration, Instant};

use protocol::CheckoutStrategy;

/// How long a toast stays before it goes by itself.
pub const TOAST_LIFETIME: Duration = Duration::from_secs(8);
/// The most toasts on screen; a newer one pushes the oldest out.
pub const MAX_TOASTS: usize = 3;
/// The checkout prompt's heading.
pub const CHECKOUT_TITLE: &str = "Switch branch in place?";

/// What a toast reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
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
    expires_at: Instant,
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
    checkout: Option<CheckoutPrompt>,
}

impl Notices {
    /// Shows a toast until [`TOAST_LIFETIME`] after `now`, pushing out the
    /// oldest when [`MAX_TOASTS`] are shown; returns its id.
    pub fn push(
        &mut self,
        kind: ToastKind,
        title: impl Into<String>,
        detail: Option<String>,
        now: Instant,
    ) -> u64 {
        if self.toasts.len() >= MAX_TOASTS {
            let excess = self.toasts.len() + 1 - MAX_TOASTS;
            self.toasts.drain(..excess);
        }
        self.next_id += 1;
        self.toasts.push(Toast {
            id: self.next_id,
            kind,
            title: title.into(),
            detail,
            expires_at: now + TOAST_LIFETIME,
        });
        self.next_id
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

    /// Drops the toasts whose time is up at `now`; returns whether any went.
    pub fn expire(&mut self, now: Instant) -> bool {
        let before = self.toasts.len();
        self.toasts.retain(|toast| toast.expires_at > now);
        self.toasts.len() != before
    }

    /// When the next toast goes.
    #[must_use]
    pub fn next_expiry(&self) -> Option<Instant> {
        self.toasts.iter().map(|toast| toast.expires_at).min()
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

    /// Shows the checkout prompt, Cancel focused, in place of the one shown.
    pub fn ask_checkout(
        &mut self,
        repo_id: String,
        branch: String,
        dirty_count: u32,
        request_id: Option<String>,
    ) {
        self.checkout = Some(CheckoutPrompt {
            repo_id,
            branch,
            dirty_count,
            request_id,
            focused: CheckoutChoice::Cancel,
        });
    }

    #[must_use]
    pub fn checkout(&self) -> Option<&CheckoutPrompt> {
        self.checkout.as_ref()
    }

    pub fn checkout_mut(&mut self) -> Option<&mut CheckoutPrompt> {
        self.checkout.as_mut()
    }

    pub fn close_checkout(&mut self) -> Option<CheckoutPrompt> {
        self.checkout.take()
    }

    /// Whether a modal notice is open.
    #[must_use]
    pub fn has_modal(&self) -> bool {
        self.action_failed.is_some() || self.checkout.is_some()
    }

    /// Closes both modal notices; returns whether either was open.
    pub fn close_modals(&mut self) -> bool {
        let failed = self.close_action_failed();
        let checkout = self.close_checkout().is_some();
        failed || checkout
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
        assert!(notices.toasts().is_empty());
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
    fn checkout_prompt_starts_on_cancel_and_cycles() {
        let mut notices = Notices::default();
        notices.ask_checkout("r1".to_owned(), "feature".to_owned(), 1, None);
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

        notices.ask_checkout("r1".to_owned(), "main".to_owned(), 3, Some("q1".to_owned()));
        let prompt = notices.checkout().expect("a prompt");
        assert!(
            prompt
                .message()
                .starts_with("This repo has 3 uncommitted changes and isn't on main.")
        );
        assert_eq!(prompt.focused(), CheckoutChoice::Cancel);
        assert!(notices.close_modals());
        assert!(notices.checkout().is_none());
    }
}
