//! The app's own toast stack: which toasts are up, newest last, how long each
//! one stays and where it sits on the monitor's work area.
//!
//! Plain Rust, with no GPUI and no Win32: the view that draws the stack comes
//! with the app's own toast windows, and every rule it needs to decide is
//! testable here.

use std::time::{Duration, Instant};

use protocol::PendingInput;

use crate::dashboard_rows::AskBlock;
use crate::target::HostKey;

/// A session as the notifications know it: its host and its id.
pub type SessionKey = (HostKey, String);

/// The key of session `id` on `host`.
#[must_use]
pub fn key_of(host: &HostKey, id: &str) -> SessionKey {
    (host.clone(), id.to_owned())
}

/// How long a Finished toast stays up.
pub const FINISHED_FOR: Duration = Duration::from_secs(6);

/// How long an account toast stays up.
pub const ACCOUNT_FOR: Duration = Duration::from_secs(8);

/// The second half of every account toast's key: a host has one account
/// toast at a time, and a later one replaces it in place.
pub const ACCOUNTS_TOAST_KEY: &str = "accounts";

/// How long a Needs you toast without answer buttons stays up.
pub const NEEDS_YOU_FOR: Duration = Duration::from_secs(30);

/// The longest a Needs you toast with answer buttons stays up.
pub const NEEDS_YOU_ANSWERABLE_CAP: Duration = Duration::from_secs(600);

/// The most answers a toast offers; a prompt with more is answered from its
/// pane.
pub const MAX_TOAST_CHOICES: usize = 3;

/// How wide a toast is, in logical px.
pub const WIDTH: f32 = 360.0;

/// The gap between two toasts, in logical px.
pub const GAP: f32 = 8.0;

/// The margin from the work area's edges, in logical px.
pub const MARGIN: f32 = 16.0;

/// The most toasts shown at once.
pub const MAX_SHOWN: usize = 4;

/// What a toast reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppToastKind {
    /// The session waits on the owner; `has_answers` when the prompt's
    /// options are drawn as buttons.
    NeedsYou {
        /// Whether the toast draws the prompt's answers.
        has_answers: bool,
    },
    /// The session's turn ended with no background work left.
    Finished,
    /// Something happened to the host's accounts; keyed by the host with
    /// [`ACCOUNTS_TOAST_KEY`], as it reports no session.
    Account,
}

impl AppToastKind {
    /// How long a toast of this kind runs before it expires.
    fn lasts(self) -> Duration {
        match self {
            Self::NeedsYou { has_answers: true } => NEEDS_YOU_ANSWERABLE_CAP,
            Self::NeedsYou { has_answers: false } => NEEDS_YOU_FOR,
            Self::Finished => FINISHED_FOR,
            Self::Account => ACCOUNT_FOR,
        }
    }
}

/// One toast the client draws itself.
#[derive(Clone, Debug)]
pub struct AppToast {
    /// The session it reports.
    pub key: SessionKey,
    /// What it reports.
    pub kind: AppToastKind,
    /// Its title line.
    pub title: String,
    /// Its body, at most two lines.
    pub body: String,
    /// Whether it stands for a wait its session was in when it was raised,
    /// so it goes when that wait ends.
    pub waiting: bool,
    /// The prompt the session waits on, when its hooks reported one.
    pub ask: Option<AskBlock>,
    /// The prompt `ask` was read from, to tell when the session's moves on.
    pub pending: Option<PendingInput>,
    /// When it was raised.
    pub raised_at: Instant,
    /// The countdown that ends it.
    timer: Timer,
}

impl AppToast {
    /// A toast raised at `now`, its countdown set by its kind.
    #[must_use]
    pub fn new(
        key: SessionKey,
        kind: AppToastKind,
        title: String,
        body: String,
        now: Instant,
    ) -> Self {
        Self {
            key,
            kind,
            title,
            body,
            waiting: false,
            ask: None,
            pending: None,
            raised_at: now,
            timer: Timer::started(kind, now),
        }
    }

    /// This toast standing for a wait its session is in, on `ask`, read from
    /// `pending`, when the prompt is known: its choices make it a toast with
    /// answers.
    #[must_use]
    pub fn waiting_on(mut self, ask: Option<AskBlock>, pending: Option<PendingInput>) -> Self {
        self.waiting = true;
        self.kind = kind_of(ask.as_ref());
        self.ask = ask;
        self.pending = pending;
        self.timer = Timer::started(self.kind, self.raised_at);
        self
    }
}

/// The kind of a Needs you toast on `ask`.
fn kind_of(ask: Option<&AskBlock>) -> AppToastKind {
    let has_answers = ask.is_some_and(|ask| !ask.choices.is_empty());
    AppToastKind::NeedsYou { has_answers }
}

/// A toast's countdown, held still while the pointer rests on it.
#[derive(Clone, Copy, Debug)]
struct Timer {
    /// The time left as of `since`.
    left: Duration,
    /// When `left` was last brought up to date.
    since: Instant,
    /// Whether the countdown is held still.
    held: bool,
}

impl Timer {
    /// A timer for a `kind` toast starting at `now`.
    fn started(kind: AppToastKind, now: Instant) -> Self {
        Self {
            left: kind.lasts(),
            since: now,
            held: false,
        }
    }

    /// The time left at `now`.
    fn left_at(&self, now: Instant) -> Duration {
        if self.held {
            return self.left;
        }
        self.left
            .saturating_sub(now.saturating_duration_since(self.since))
    }

    /// When the toast expires if nothing holds it still.
    fn deadline(&self) -> Instant {
        self.since + self.left
    }

    /// Holds the countdown still at `now`.
    fn hold(&mut self, now: Instant) {
        if !self.held {
            self.left = self.left_at(now);
            self.since = now;
            self.held = true;
        }
    }

    /// Moves a countdown that ran for `from` to run for `to` as of `now`,
    /// keeping the time already spent, so the toast's time runs from its
    /// raise.
    fn retarget(&mut self, from: Duration, to: Duration, now: Instant) {
        let spent = from.saturating_sub(self.left_at(now));
        self.left = to.saturating_sub(spent);
        self.since = now;
    }

    /// Lets the countdown run on from `now`.
    fn release(&mut self, now: Instant) {
        if self.held {
            self.held = false;
            self.since = now;
        }
    }
}

/// The toasts that are up, newest last.
#[derive(Debug, Default)]
pub struct ToastStack {
    items: Vec<AppToast>,
    /// The toasts the pointer rests on, by key; no countdown runs while any
    /// is here.
    hovered: Vec<SessionKey>,
}

impl ToastStack {
    /// Whether the pointer rests on any toast, which holds every countdown.
    fn paused(&self) -> bool {
        !self.hovered.is_empty()
    }

    /// Drops the keys of toasts that are gone, and, once nothing is hovered,
    /// lets every countdown run on from `now`.
    fn sync(&mut self, now: Instant) {
        let items = &self.items;
        self.hovered
            .retain(|key| items.iter().any(|item| &item.key == key));
        if !self.paused() {
            for item in &mut self.items {
                item.timer.release(now);
            }
        }
    }

    /// Raises `toast` at `now`: a toast for a session already up replaces
    /// that one in place and restarts its countdown, otherwise it joins the
    /// end. Past [`MAX_SHOWN`], the oldest Finished toast goes first, else
    /// the oldest toast of any kind. Returns the keys of the toasts dropped
    /// to make room.
    pub fn push_or_update(&mut self, mut toast: AppToast, now: Instant) -> Vec<SessionKey> {
        self.sync(now);
        toast.raised_at = now;
        toast.timer = Timer::started(toast.kind, now);
        if self.paused() {
            toast.timer.hold(now);
        }
        let mut removed = Vec::new();
        if let Some(existing) = self.items.iter_mut().find(|item| item.key == toast.key) {
            *existing = toast;
        } else {
            self.items.push(toast);
        }
        while self.items.len() > MAX_SHOWN {
            let index = self
                .items
                .iter()
                .position(|item| item.kind == AppToastKind::Finished)
                .unwrap_or_default();
            removed.push(self.items.remove(index).key);
        }
        self.sync(now);
        removed
    }

    /// Gives `key`'s toast the prompt `ask`, read from `pending`, at `now`:
    /// its kind follows, and its countdown keeps the time already spent.
    pub fn update_ask(
        &mut self,
        key: &SessionKey,
        ask: Option<AskBlock>,
        pending: Option<PendingInput>,
        now: Instant,
    ) {
        let Some(toast) = self.items.iter_mut().find(|item| &item.key == key) else {
            return;
        };
        let kind = kind_of(ask.as_ref());
        toast.timer.retarget(toast.kind.lasts(), kind.lasts(), now);
        toast.kind = kind;
        toast.ask = ask;
        toast.pending = pending;
    }

    /// Removes the toast for `key` at `now`, and the pointer's rest on it
    /// with it; returns whether one was there.
    pub fn dismiss(&mut self, key: &SessionKey, now: Instant) -> bool {
        let Some(index) = self.items.iter().position(|item| &item.key == key) else {
            return false;
        };
        self.items.remove(index);
        self.sync(now);
        true
    }

    /// Holds every countdown still from `now`, while the pointer rests on
    /// `key`.
    pub fn hover(&mut self, key: &SessionKey, now: Instant) {
        self.sync(now);
        if self.hovered.contains(key) || !self.items.iter().any(|item| &item.key == key) {
            return;
        }
        let was_paused = self.paused();
        self.hovered.push(key.clone());
        if !was_paused {
            for item in &mut self.items {
                item.timer.hold(now);
            }
        }
    }

    /// Lets the countdowns run on from `now`, once the pointer has left the
    /// last toast it rested on, each with the time it had left then.
    pub fn unhover(&mut self, key: &SessionKey, now: Instant) {
        if let Some(index) = self.hovered.iter().position(|hovered| hovered == key) {
            self.hovered.remove(index);
        }
        self.sync(now);
    }

    /// Removes every toast whose countdown has run out by `now`, and returns
    /// their keys.
    pub fn expire(&mut self, now: Instant) -> Vec<SessionKey> {
        self.sync(now);
        let mut gone = Vec::new();
        self.items.retain(|item| {
            if !item.timer.left_at(now).is_zero() {
                return true;
            }
            gone.push(item.key.clone());
            false
        });
        self.hovered.retain(|key| !gone.contains(key));
        gone
    }

    /// Removes every toast of `host` at `now`, and returns their keys.
    pub fn drop_host(&mut self, host: &HostKey, now: Instant) -> Vec<SessionKey> {
        let mut gone = Vec::new();
        self.items.retain(|item| {
            if &item.key.0 != host {
                return true;
            }
            gone.push(item.key.clone());
            false
        });
        self.sync(now);
        gone
    }

    /// The toasts that are up, newest last.
    #[must_use]
    pub fn items(&self) -> &[AppToast] {
        &self.items
    }

    /// The soonest countdown deadline in the stack, for the view's timer;
    /// never earlier than `now`, and `None` while the stack is held.
    #[must_use]
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if self.paused() {
            return None;
        }
        self.items
            .iter()
            .map(|item| item.timer.deadline())
            .min()
            .map(|deadline| deadline.max(now))
    }
}

/// A monitor's work area, in logical px.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorkArea {
    /// The left edge.
    pub left: f32,
    /// The top edge.
    pub top: f32,
    /// The right edge.
    pub right: f32,
    /// The bottom edge.
    pub bottom: f32,
}

/// Where each toast of `toasts` (its kind and height, newest last) sits in
/// `area`: the newest nearest the bottom-right corner, older ones stacked
/// upward a [`GAP`] apart, each [`WIDTH`] wide and [`MARGIN`] from the right
/// edge. Returns each kept toast's index in `toasts` with its top-left, in
/// the order given.
///
/// A stack too tall for the work area leaves out its oldest Finished toast
/// first, so a Needs you is never hidden while a Finished toast could go;
/// only when no Finished toast is left does the oldest toast of any kind go.
#[must_use]
pub fn place(area: WorkArea, toasts: &[(AppToastKind, f32)]) -> Vec<(usize, (f32, f32))> {
    let mut kept: Vec<usize> = (0..toasts.len()).collect();
    loop {
        if let Some(placed) = layout(area, toasts, &kept) {
            return kept.into_iter().zip(placed).collect();
        }
        let victim = kept
            .iter()
            .position(|&index| toasts[index].0 == AppToastKind::Finished)
            .unwrap_or_default();
        kept.remove(victim);
    }
}

/// Each kept toast's top-left, or `None` when one of them falls above the
/// work area.
fn layout(
    area: WorkArea,
    toasts: &[(AppToastKind, f32)],
    kept: &[usize],
) -> Option<Vec<(f32, f32)>> {
    let left = area.right - MARGIN - WIDTH;
    let mut bottom = area.bottom - MARGIN;
    let mut placed = Vec::with_capacity(kept.len());
    for &index in kept.iter().rev() {
        let top = bottom - toasts[index].1;
        if top < area.top + MARGIN {
            return None;
        }
        placed.push((left, top));
        bottom = top - GAP;
    }
    placed.reverse();
    Some(placed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(session: &str) -> SessionKey {
        key_of(&HostKey::Local, session)
    }

    fn boxed_key(session: &str) -> SessionKey {
        key_of(&box_host(), session)
    }

    fn box_host() -> HostKey {
        HostKey::Remote("box".to_owned())
    }

    fn toast(session: &str, kind: AppToastKind, now: Instant) -> AppToast {
        AppToast::new(
            key(session),
            kind,
            "a title".to_owned(),
            "a body".to_owned(),
            now,
        )
    }

    fn needs_you(answers: bool) -> AppToastKind {
        AppToastKind::NeedsYou {
            has_answers: answers,
        }
    }

    #[test]
    fn drop_host_removes_only_that_hosts_toasts() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push_or_update(
            AppToast::new(
                boxed_key("a"),
                needs_you(true),
                "a".to_owned(),
                "body".to_owned(),
                now,
            ),
            now,
        );
        stack.push_or_update(toast("b", needs_you(true), now), now);
        stack.push_or_update(
            AppToast::new(
                boxed_key("c"),
                needs_you(true),
                "c".to_owned(),
                "body".to_owned(),
                now,
            ),
            now,
        );
        assert_eq!(
            stack.drop_host(&box_host(), now),
            vec![boxed_key("a"), boxed_key("c")]
        );
        assert_eq!(stack.items().len(), 1);
        assert_eq!(stack.items()[0].key, key("b"));
    }

    #[test]
    fn an_account_toast_lasts_eight_seconds() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        let account = AppToast::new(
            key_of(&HostKey::Local, ACCOUNTS_TOAST_KEY),
            AppToastKind::Account,
            "Accounts".to_owned(),
            "Switched to home".to_owned(),
            now,
        );
        stack.push_or_update(account, now);
        assert_eq!(stack.next_deadline(now), Some(now + ACCOUNT_FOR));
        assert_eq!(ACCOUNT_FOR, Duration::from_secs(8));
        assert_eq!(
            stack.expire(now + Duration::from_millis(7999)),
            Vec::<SessionKey>::new()
        );
        assert_eq!(
            stack.expire(now + ACCOUNT_FOR),
            vec![key_of(&HostKey::Local, ACCOUNTS_TOAST_KEY)]
        );
    }

    #[test]
    fn dismissing_the_hovered_toast_keeps_the_others_time_left() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push_or_update(toast("a", AppToastKind::Finished, now), now);
        stack.push_or_update(toast("b", AppToastKind::Finished, now), now);
        let hovered = now + Duration::from_secs(1);
        stack.hover(&key("a"), hovered);
        stack.dismiss(&key("a"), now + Duration::from_secs(2));
        let before = now + Duration::from_millis(6999);
        assert_eq!(
            stack.next_deadline(before),
            Some(now + Duration::from_secs(7))
        );
        assert!(stack.expire(before).is_empty(), "five seconds were held");
        assert_eq!(stack.expire(now + Duration::from_secs(7)), vec![key("b")]);
    }

    #[test]
    fn a_same_key_toast_replaces_in_place_and_restarts() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push_or_update(toast("a", AppToastKind::Finished, now), now);
        stack.push_or_update(toast("b", AppToastKind::Finished, now), now);
        stack.push_or_update(toast("c", AppToastKind::Finished, now), now);
        let later = now + Duration::from_secs(4);
        assert!(
            stack
                .push_or_update(toast("a", needs_you(false), later), later)
                .is_empty(),
            "the replacement drops nothing"
        );
        assert_eq!(stack.items().len(), 3, "the replacement adds no toast");
        assert_eq!(stack.items()[0].key, key("a"), "it keeps its position");
        assert_eq!(stack.items()[0].kind, needs_you(false), "it takes the new");
        assert_eq!(
            stack.items()[0].raised_at,
            later,
            "the stack stamps the push"
        );
        let gone = stack.expire(later + Duration::from_secs(10));
        assert!(
            !gone.contains(&key("a")),
            "the countdown restarted at the replacement"
        );
        assert_eq!(gone, vec![key("b"), key("c")], "the Finished pair is gone");
    }

    #[test]
    fn a_replacement_under_the_pointer_stays_paused() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push_or_update(toast("a", needs_you(false), now), now);
        stack.hover(&key("a"), now + Duration::from_secs(1));
        let later = now + Duration::from_secs(5);
        stack.push_or_update(toast("a", AppToastKind::Finished, later), later);
        assert_eq!(stack.items().len(), 1, "it replaces the toast in place");
        assert!(
            stack.expire(now + Duration::from_secs(600)).is_empty(),
            "the replacement is still under the pointer"
        );
        assert!(
            stack
                .next_deadline(now + Duration::from_secs(600))
                .is_none(),
            "the stack is still held"
        );
        stack.unhover(&key("a"), now + Duration::from_secs(601));
        assert!(
            stack.expire(now + Duration::from_secs(606)).is_empty(),
            "six seconds from the unhover"
        );
        assert_eq!(stack.expire(now + Duration::from_secs(607)), vec![key("a")]);
    }

    #[test]
    fn overflow_drops_the_oldest_finished_first() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push_or_update(toast("a", needs_you(false), now), now);
        stack.push_or_update(toast("b", needs_you(false), now), now);
        stack.push_or_update(toast("c", AppToastKind::Finished, now), now);
        stack.push_or_update(toast("d", needs_you(true), now), now);
        assert_eq!(
            stack.push_or_update(toast("e", needs_you(true), now), now),
            vec![key("c")],
            "the Finished one goes, not the oldest"
        );
        assert_eq!(stack.items().len(), MAX_SHOWN);
        assert!(
            stack.items().iter().any(|item| item.key == key("a")),
            "the oldest toast stays"
        );
    }

    #[test]
    fn overflow_drops_the_oldest_when_none_is_finished() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        for session in ["a", "b", "c", "d"] {
            stack.push_or_update(toast(session, needs_you(true), now), now);
        }
        assert_eq!(
            stack.push_or_update(toast("e", needs_you(false), now), now),
            vec![key("a")],
            "the oldest goes"
        );
        assert_eq!(
            stack
                .items()
                .iter()
                .map(|item| &item.key)
                .collect::<Vec<_>>(),
            vec![&key("b"), &key("c"), &key("d"), &key("e")]
        );
    }

    #[test]
    fn finished_expires_after_six_seconds() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push_or_update(toast("a", AppToastKind::Finished, now), now);
        assert!(
            stack.expire(now + Duration::from_millis(5999)).is_empty(),
            "still up just before six seconds"
        );
        assert_eq!(stack.expire(now + FINISHED_FOR), vec![key("a")]);
        assert!(stack.items().is_empty(), "the stack is empty");
    }

    #[test]
    fn needs_you_without_answers_expires_after_thirty_seconds() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push_or_update(toast("a", needs_you(false), now), now);
        assert!(
            stack.expire(now + Duration::from_millis(29_999)).is_empty(),
            "still up just before thirty seconds"
        );
        assert_eq!(stack.expire(now + NEEDS_YOU_FOR), vec![key("a")]);
    }

    #[test]
    fn needs_you_with_answers_stays_until_the_cap() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push_or_update(toast("a", needs_you(true), now), now);
        assert!(
            stack.expire(now + Duration::from_secs(599)).is_empty(),
            "answerable toasts outlast the plain ones"
        );
        assert_eq!(stack.expire(now + NEEDS_YOU_ANSWERABLE_CAP), vec![key("a")]);
    }

    #[test]
    fn hovering_any_toast_pauses_every_timer() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push_or_update(toast("a", AppToastKind::Finished, now), now);
        stack.push_or_update(toast("b", AppToastKind::Finished, now), now);
        stack.hover(&key("a"), now + Duration::from_secs(1));
        assert!(stack.expire(now + FINISHED_FOR).is_empty(), "b is held too");
        stack.hover(&key("b"), now + Duration::from_secs(2));
        assert!(
            stack.expire(now + Duration::from_secs(600)).is_empty(),
            "both countdowns are held"
        );
        stack.unhover(&key("a"), now + Duration::from_secs(601));
        assert!(
            stack.expire(now + Duration::from_secs(602)).is_empty(),
            "b is still under the pointer, so the stack stays held"
        );
        stack.unhover(&key("b"), now + Duration::from_secs(603));
        assert!(
            stack.expire(now + Duration::from_secs(604)).is_empty(),
            "both resume with their five seconds left"
        );
        assert_eq!(
            stack.expire(now + Duration::from_secs(608)),
            vec![key("a"), key("b")]
        );
    }

    #[test]
    fn unhover_resumes_the_remaining_time() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push_or_update(toast("a", AppToastKind::Finished, now), now);
        stack.hover(&key("a"), now + Duration::from_secs(1));
        stack.unhover(&key("a"), now + Duration::from_secs(11));
        assert!(
            stack.expire(now + Duration::from_secs(15)).is_empty(),
            "five seconds were left when the pointer came to rest"
        );
        assert_eq!(stack.expire(now + Duration::from_secs(16)), vec![key("a")]);
    }

    #[test]
    fn next_deadline_is_none_while_a_toast_is_hovered() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.push_or_update(toast("a", AppToastKind::Finished, now), now);
        assert_eq!(stack.next_deadline(now), Some(now + FINISHED_FOR));
        let one_later = now + Duration::from_secs(1);
        stack.push_or_update(toast("b", AppToastKind::Finished, one_later), one_later);
        assert_eq!(
            stack.next_deadline(one_later),
            Some(now + FINISHED_FOR),
            "a's countdown comes due first"
        );
        stack.hover(&key("a"), one_later);
        assert_eq!(
            stack.next_deadline(one_later),
            None,
            "the stack is held while the pointer rests on it"
        );
        stack.unhover(&key("a"), one_later);
        assert_eq!(
            stack.next_deadline(one_later),
            Some(now + FINISHED_FOR),
            "a's five seconds are back in front of b's six"
        );
    }

    fn area() -> WorkArea {
        WorkArea {
            left: 0.0,
            top: 0.0,
            right: 1920.0,
            bottom: 1080.0,
        }
    }

    fn left_edge() -> f32 {
        1920.0 - MARGIN - WIDTH
    }

    #[test]
    fn place_puts_the_newest_in_the_corner() {
        assert_eq!(
            place(area(), &[(AppToastKind::Finished, 100.0)]),
            vec![(0, (left_edge(), 1080.0 - MARGIN - 100.0))]
        );
    }

    #[test]
    fn place_stacks_older_toasts_upward_with_the_gap() {
        let newest_top = 1080.0 - MARGIN - 100.0;
        let older_top = newest_top - GAP - 80.0;
        assert_eq!(
            place(
                area(),
                &[(needs_you(false), 80.0), (AppToastKind::Finished, 100.0)]
            ),
            vec![
                (0, (left_edge(), older_top)),
                (1, (left_edge(), newest_top))
            ]
        );
    }

    #[test]
    fn place_leaves_out_toasts_above_the_work_area() {
        assert_eq!(
            place(
                area(),
                &[(needs_you(false), 100.0), (needs_you(false), 1000.0)]
            ),
            vec![(1, (left_edge(), 1080.0 - MARGIN - 1000.0))],
            "the older toast does not fit and is left out"
        );
    }

    #[test]
    fn place_leaves_out_finished_before_needs_you() {
        let newest_top = 1080.0 - MARGIN - 400.0;
        let older_top = newest_top - GAP - 400.0;
        assert_eq!(
            place(
                area(),
                &[
                    (needs_you(false), 400.0),
                    (AppToastKind::Finished, 400.0),
                    (needs_you(true), 400.0),
                ]
            ),
            vec![
                (0, (left_edge(), older_top)),
                (2, (left_edge(), newest_top))
            ],
            "the Finished toast goes, not the oldest Needs you"
        );
    }
}
