//! The phone's Needs you tray above Home's list: one compact card at a time
//! for each session the Needs you list holds, longest wait first; a swipe
//! moves between them, and a short prompt is answered from the card.
//!
//! Plain Rust, so the order, the buttons and the swipe are unit-testable
//! without GPUI; `home_view` feeds it the pan's scroll events and draws
//! the card it says, offset by the swipe's travel. The tray is drawn from
//! `&self`, so the card it last showed and the swipe in progress sit in a
//! [`RefCell`] the draw updates.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use gpui::ScrollHandle;
use protocol::PromptChoice;

use crate::dashboard_rows::{AskBlock, AskQuestion};
use crate::needs_you::{self, NeedsYouRow};
use crate::sidebar::SidebarModel;
use crate::target::HostKey;
use crate::toast_stack::MAX_TOAST_CHOICES;

/// How far back from the lift the release velocity looks, and how long a
/// finger may rest before the lift and still fling: gpui's stopped gap.
const VELOCITY_WINDOW: Duration = Duration::from_millis(40);
/// The share of the card's width a swipe passes to move a card.
const MOVE_SHARE: f32 = 1.0 / 3.0;
/// The release velocity, in pixels per second, at which a shorter swipe
/// still moves a card: a flick.
const FLING_PX_PER_S: f32 = 300.0;
/// The least travel a flick needs, so a tap's jitter never moves a card.
const FLICK_MIN_TRAVEL: f32 = 24.0;

/// Which session a card is: its host and id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CardKey {
    pub(crate) host: HostKey,
    pub(crate) session_id: String,
}

/// One answer button: its text, the option's description and what a press
/// sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrayButton {
    pub(crate) label: String,
    /// The option's description, drawn under its label; `None` when it has
    /// none, and for a permission's choices.
    pub(crate) description: Option<String>,
    pub(crate) choice: PromptChoice,
}

/// The card's row of buttons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Buttons {
    /// The prompt is answered from the card, against the wait `since`.
    Answer {
        since: DateTime<Utc>,
        buttons: Vec<TrayButton>,
    },
    /// Anything longer: one button that opens the session.
    Open,
}

/// One session's card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrayCard {
    pub(crate) key: CardKey,
    /// The label the sidebar leaf shows.
    pub(crate) label: String,
    /// The repo or workspace the session sits in.
    pub(crate) container_name: String,
    /// The prompt's text, else what the Needs you list says of it.
    pub(crate) text: String,
    /// A permission's command.
    pub(crate) command: Option<String>,
    /// When the wait began; `None` from a daemon that predates the stamp.
    pub(crate) since: Option<DateTime<Utc>>,
    /// Every question the prompt asks, with each option's description,
    /// which a card that cannot answer draws read-only; empty for a
    /// permission and for a prompt with no questions.
    pub(crate) questions: Vec<AskQuestion>,
    pub(crate) buttons: Buttons,
}

/// The cards for `model`'s Needs you list, in its order (the longest wait
/// first), each with the buttons of the ask block `ask` reads for it: only
/// the sessions of hosts whose connection `open` says is open, since a
/// closed host's list is stale and an answer could not reach it.
pub(crate) fn cards(
    model: &SidebarModel,
    open: impl Fn(&HostKey) -> bool,
    ask: impl Fn(&HostKey, &str) -> Option<AskBlock>,
) -> Vec<TrayCard> {
    needs_you::rows(model)
        .iter()
        .filter(|row| open(&row.host))
        .map(|row| card(row, ask(&row.host, &row.session_id).as_ref()))
        .collect()
}

/// `row`'s card, its text and buttons from `ask` when the session has one.
fn card(row: &NeedsYouRow, ask: Option<&AskBlock>) -> TrayCard {
    TrayCard {
        key: CardKey {
            host: row.host.clone(),
            session_id: row.session_id.clone(),
        },
        label: row.label.clone(),
        container_name: row.container_name.clone(),
        text: ask.map_or_else(|| row.detail.clone(), |ask| ask.text.clone()),
        command: ask.and_then(|ask| ask.command.clone()),
        since: row.since,
        questions: ask.map_or_else(Vec::new, |ask| ask.questions.clone()),
        buttons: ask.map_or(Buttons::Open, buttons),
    }
}

/// `ask`'s buttons on a card: an option, Allow or Deny each, while there
/// are one to [`MAX_TOAST_CHOICES`] of them and the wait has its stamp;
/// else Open. A permission's Always allow and the typed answers are left
/// to the session's page.
fn buttons(ask: &AskBlock) -> Buttons {
    let Some(since) = ask.since else {
        return Buttons::Open;
    };
    let buttons: Vec<TrayButton> = ask
        .choices
        .iter()
        .filter(|choice| on_card(&choice.choice))
        .map(|choice| TrayButton {
            label: choice.label.clone(),
            description: choice.description.clone(),
            choice: choice.choice.clone(),
        })
        .collect();
    if buttons.is_empty() || buttons.len() > MAX_TOAST_CHOICES {
        Buttons::Open
    } else {
        Buttons::Answer { since, buttons }
    }
}

/// Whether a card offers `choice`.
fn on_card(choice: &PromptChoice) -> bool {
    matches!(
        choice,
        PromptChoice::Option { .. } | PromptChoice::Allow | PromptChoice::Deny
    )
}

/// The cards as drawn now and which of them shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Arranged {
    pub(crate) cards: Vec<TrayCard>,
    pub(crate) index: usize,
}

impl Arranged {
    /// The card that shows; `None` hides the tray.
    pub(crate) fn shown(&self) -> Option<&TrayCard> {
        self.cards.get(self.index)
    }

    /// `Needs you · {i} of {n}`, while a card shows.
    pub(crate) fn header(&self) -> Option<String> {
        self.shown()?;
        Some(format!(
            "Needs you · {} of {}",
            self.index + 1,
            self.cards.len()
        ))
    }
}

/// A touch down on the tray: how far it has travelled sideways and its
/// recent movements, for the release velocity.
#[derive(Debug, Default)]
struct Swipe {
    travel: f32,
    samples: Vec<(Instant, f32)>,
}

impl Swipe {
    fn moved(&mut self, dx: f32, at: Instant) {
        self.travel += dx;
        self.samples.push((at, dx));
        self.samples
            .retain(|(when, _)| at.saturating_duration_since(*when) <= VELOCITY_WINDOW);
    }

    /// The sideways velocity at a release at `at`, in pixels per second:
    /// the movements in the last [`VELOCITY_WINDOW`] before the release
    /// over the time they took; none when the finger had rested longer than
    /// that before the release, or the movements took no time.
    fn velocity(&self, at: Instant) -> f32 {
        let recent: Vec<&(Instant, f32)> = self
            .samples
            .iter()
            .filter(|(when, _)| at.saturating_duration_since(*when) <= VELOCITY_WINDOW)
            .collect();
        let (Some((first, _)), Some((last, _))) = (recent.first(), recent.last()) else {
            return 0.0;
        };
        let span = last.saturating_duration_since(*first).as_secs_f32();
        if span <= 0.0 {
            return 0.0;
        }
        let moved: f32 = recent.iter().skip(1).map(|(_, dx)| dx).sum();
        moved / span
    }
}

/// What lifting a touch off the tray did: whether the shown card changed,
/// and whether the touch travelled far enough sideways to be a swipe
/// rather than a tap.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Lift {
    /// The shown card changed: the lift moved to another card.
    pub(crate) changed: bool,
    /// The touch travelled at least [`FLICK_MIN_TRAVEL`] sideways, whether
    /// or not a card changed: at the last card a sideways swipe moves on
    /// past nothing and snaps back, but it is still a swipe.
    pub(crate) swiped: bool,
}

/// Which way a lift moves: `1` to the next card, `-1` to the one before,
/// `0` to snap back. A finger moving left (`travel` below zero) brings the
/// next card in. A swipe past a third of the card's `width` moves the way
/// it travelled; a shorter one moves only as a flick: at least
/// [`FLICK_MIN_TRAVEL`], released at [`FLING_PX_PER_S`] or faster the same
/// way it travelled. A release moving back the other way snaps back.
fn step(travel: f32, velocity: f32, width: f32) -> i8 {
    let towards = if travel < 0.0 { 1 } else { -1 };
    let past_third = width > 0.0 && travel.abs() > width * MOVE_SHARE;
    let flick = travel.abs() >= FLICK_MIN_TRAVEL
        && velocity.abs() >= FLING_PX_PER_S
        && (velocity < 0.0) == (travel < 0.0);
    if past_third || flick { towards } else { 0 }
}

#[derive(Debug, Default)]
struct State {
    /// The card last shown, kept by its key.
    shown: Option<CardKey>,
    /// The place it had.
    index: usize,
    /// The cards' order as last drawn.
    order: Vec<CardKey>,
    /// The touch down on the tray, while there is one; the order holds
    /// still meanwhile.
    swipe: Option<Swipe>,
    /// The card whose body's scroll the tray tracks, and that body's
    /// handle.
    body: Option<(CardKey, ScrollHandle)>,
}

/// What the tray remembers between frames.
#[derive(Debug, Default)]
pub(crate) struct Tray {
    state: RefCell<State>,
}

impl Tray {
    /// `latest`, the cards in the Needs you list's order, as drawn now: in
    /// that order, or while a touch is down in the order they were drawn
    /// in, new cards after. The shown card stays shown while it is listed,
    /// else the card that took its place does, clamped to the last.
    pub(crate) fn arrange(&self, latest: Vec<TrayCard>) -> Arranged {
        let mut state = self.state.borrow_mut();
        if latest.is_empty() {
            // The tray hides: a swipe on it is over, and the order unfreezes.
            state.swipe = None;
        }
        let cards = if state.swipe.is_some() {
            frozen(&state.order, latest)
        } else {
            latest
        };
        let index = place(&cards, state.shown.as_ref(), state.index);
        state.index = index;
        state.shown = cards.get(index).map(|card| card.key.clone());
        state.order = cards.iter().map(|card| card.key.clone()).collect();
        Arranged { cards, index }
    }

    /// `key`'s card body's scroll: a handle of its own, at the top, from the
    /// first time the tray shows that card, so the next card never opens
    /// part-way down the last one's body.
    pub(crate) fn body_scroll(&self, key: &CardKey) -> ScrollHandle {
        let mut state = self.state.borrow_mut();
        if let Some((held, handle)) = state.body.as_ref()
            && held == key
        {
            return handle.clone();
        }
        let handle = ScrollHandle::new();
        state.body = Some((key.clone(), handle.clone()));
        handle
    }

    /// How far the shown card is drawn off its place: the swipe's travel.
    pub(crate) fn offset(&self) -> f32 {
        self.state
            .borrow()
            .swipe
            .as_ref()
            .map_or(0.0, |swipe| swipe.travel)
    }

    /// A touch went down on the tray, already `dx` sideways, at `at`.
    pub(crate) fn touch_started(&self, dx: f32, at: Instant) {
        let mut swipe = Swipe::default();
        swipe.moved(dx, at);
        self.state.borrow_mut().swipe = Some(swipe);
    }

    /// The touch moved `dx` sideways at `at`; nothing while no touch is
    /// down, as for the momentum after a lift.
    pub(crate) fn touch_moved(&self, dx: f32, at: Instant) {
        if let Some(swipe) = self.state.borrow_mut().swipe.as_mut() {
            swipe.moved(dx, at);
        }
    }

    /// The touch lifted after a last `dx` at `at` over a card `width`
    /// wide: it moves one card when it travelled past a third of the
    /// width or was released at a fling's velocity, else snaps back. The
    /// ends do not wrap.
    pub(crate) fn touch_ended(&self, dx: f32, at: Instant, width: f32) -> Lift {
        let mut state = self.state.borrow_mut();
        let Some(mut swipe) = state.swipe.take() else {
            return Lift::default();
        };
        swipe.travel += dx;
        let swiped = swipe.travel.abs() >= FLICK_MIN_TRAVEL;
        let towards = step(swipe.travel, swipe.velocity(at), width);
        let from = state.index;
        let to = match towards {
            1 if from + 1 < state.order.len() => from + 1,
            -1 if from > 0 => from - 1,
            _ => from,
        };
        state.index = to;
        state.shown = state.order.get(to).cloned();
        Lift {
            changed: to != from,
            swiped,
        }
    }

    /// The system took the touch: the card snaps back.
    pub(crate) fn touch_cancelled(&self) {
        self.state.borrow_mut().swipe = None;
    }
}

/// `latest` in `order`, the cards `order` does not hold after them.
fn frozen(order: &[CardKey], latest: Vec<TrayCard>) -> Vec<TrayCard> {
    let (mut held, fresh): (Vec<TrayCard>, Vec<TrayCard>) = latest
        .into_iter()
        .partition(|card| order.contains(&card.key));
    held.sort_by_key(|card| order.iter().position(|key| *key == card.key));
    held.extend(fresh);
    held
}

/// The place of the shown card among `cards`: its own while it is listed,
/// else `last`, the place it had, clamped to the last card.
fn place(cards: &[TrayCard], shown: Option<&CardKey>, last: usize) -> usize {
    if cards.is_empty() {
        return 0;
    }
    shown
        .and_then(|key| cards.iter().position(|card| card.key == *key))
        .unwrap_or_else(|| last.min(cards.len() - 1))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test fixtures")]
mod tests {
    use super::*;
    use crate::connection_picker::SavedHost;
    use crate::dashboard_rows::{RowInput, ask_block, row_of};
    use crate::target::Target;
    use protocol::{DaemonMessage, RepoEntry, SessionSnapshot};
    use serde_json::{Value, json};

    const BOX: &str = "box.tail1234.ts.net";

    fn key() -> HostKey {
        HostKey::of(&Target::tailnet(BOX, Some(8787)))
    }

    fn repo() -> RepoEntry {
        serde_json::from_value(json!({ "id": "r1", "name": "r1", "path": "C:/r1" }))
            .expect("repo fixture")
    }

    /// `id` waiting since `minutes` ago on `pending`.
    fn waiting(id: &str, minutes: i64, pending: &Value) -> SessionSnapshot {
        let since = Utc::now() - chrono::TimeDelta::minutes(minutes);
        serde_json::from_value(json!({
            "id": id,
            "label": id,
            "kind": "single",
            "members": [{
                "repo_id": "r1",
                "repo_name": "r1",
                "branch": "main",
                "worktree_path": "",
            }],
            "status": "awaiting_input",
            "status_since": since,
            "pending_input": pending,
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
        .expect("session fixture")
    }

    fn question(options: &[&str]) -> Value {
        json!({
            "kind": "question",
            "questions": [{
                "question": "Which branch?",
                "header": "",
                "options": options,
                "multi_select": false,
            }],
        })
    }

    fn permission() -> Value {
        json!({ "kind": "permission", "tool_name": "Bash", "summary": "gh pr merge 118" })
    }

    /// "box" saved and listing r1 with `sessions`.
    fn model(sessions: Vec<SessionSnapshot>) -> SidebarModel {
        let mut model = SidebarModel::default();
        model.set_saved_hosts(vec![
            SavedHost::from_target(&Target::tailnet(BOX, Some(8787))).expect("a tailnet host"),
        ]);
        model.hide_this_pc();
        model.apply(
            &key(),
            &DaemonMessage::Repos {
                repos: vec![repo()],
            },
        );
        model.apply(&key(), &DaemonMessage::Sessions { sessions });
        model
    }

    /// The tray's cards for `model`, their ask blocks read as the toast
    /// reads them with no chat loaded.
    fn cards_of(model: &SidebarModel) -> Vec<TrayCard> {
        cards(
            model,
            |_| true,
            |host, id| {
                let session = model.session_on(host, id)?;
                let row = row_of(model, session);
                ask_block(&RowInput {
                    row: &row,
                    session,
                    facts: None,
                    agents: &[],
                    unseen: false,
                    prompt: None,
                })
            },
        )
    }

    fn only_card(pending: &Value) -> TrayCard {
        cards_of(&model(vec![waiting("s", 1, pending)]))
            .pop()
            .expect("one card")
    }

    fn labels(card: &TrayCard) -> Vec<(&str, &PromptChoice)> {
        match &card.buttons {
            Buttons::Answer { buttons, .. } => buttons
                .iter()
                .map(|button| (button.label.as_str(), &button.choice))
                .collect(),
            Buttons::Open => Vec::new(),
        }
    }

    /// The descriptions of `card`'s answer buttons.
    fn descriptions(card: &TrayCard) -> Vec<Option<&str>> {
        match &card.buttons {
            Buttons::Answer { buttons, .. } => buttons
                .iter()
                .map(|button| button.description.as_deref())
                .collect(),
            Buttons::Open => Vec::new(),
        }
    }

    fn ids(arranged: &Arranged) -> Vec<&str> {
        arranged
            .cards
            .iter()
            .map(|card| card.key.session_id.as_str())
            .collect()
    }

    fn shown(arranged: &Arranged) -> &str {
        arranged
            .shown()
            .map(|card| card.key.session_id.as_str())
            .expect("a card shows")
    }

    /// Cards `ids` in that order, each a two-option question.
    fn listed(ids: &[&str]) -> Vec<TrayCard> {
        let card = only_card(&question(&["main", "dev"]));
        ids.iter()
            .map(|id| {
                let mut card = card.clone();
                card.key.session_id = (*id).to_owned();
                card
            })
            .collect()
    }

    const WIDTH: f32 = 300.0;

    /// A tray showing the first of `n` cards.
    fn tray_of(n: usize) -> (Tray, Vec<TrayCard>) {
        let names = ["a", "b", "c", "d"];
        let cards = listed(&names[..n]);
        let tray = Tray::default();
        tray.arrange(cards.clone());
        (tray, cards)
    }

    /// A slow drag: `steps` movements of `dx` 200 ms apart, then a lift
    /// 200 ms later, so no fling.
    fn slow_swipe(tray: &Tray, dx: f32, steps: u32) -> Lift {
        let start = Instant::now();
        let gap = Duration::from_millis(200);
        tray.touch_started(0.0, start);
        for n in 1..=steps {
            tray.touch_moved(dx, start + gap * n);
        }
        tray.touch_ended(0.0, start + gap * (steps + 1), WIDTH)
    }

    #[test]
    fn cards_are_longest_wait_first() {
        let pending = question(&["main", "dev"]);
        let model = model(vec![
            waiting("a", 1, &pending),
            waiting("b", 5, &pending),
            waiting("c", 3, &pending),
        ]);
        let tray = Tray::default();
        let arranged = tray.arrange(cards_of(&model));
        assert_eq!(ids(&arranged), ["b", "c", "a"]);
        assert_eq!(arranged.header().as_deref(), Some("Needs you · 1 of 3"));
        assert!(
            tray.arrange(Vec::new()).header().is_none(),
            "no card, no tray"
        );
    }

    #[test]
    fn the_shown_card_is_kept_when_another_card_arrives() {
        let (tray, _) = tray_of(2);
        assert!(slow_swipe(&tray, -60.0, 2).changed, "moved to b");
        let arranged = tray.arrange(listed(&["x", "a", "b"]));
        assert_eq!(shown(&arranged), "b", "b stays shown");
        assert_eq!(arranged.header().as_deref(), Some("Needs you · 3 of 3"));
    }

    #[test]
    fn the_shown_card_falls_back_to_its_place_when_it_leaves() {
        let (tray, _) = tray_of(3);
        assert!(slow_swipe(&tray, -60.0, 2).changed, "moved to b");
        assert_eq!(
            shown(&tray.arrange(listed(&["a", "c"]))),
            "c",
            "c took b's place"
        );
        let (tray, _) = tray_of(3);
        slow_swipe(&tray, -60.0, 2);
        slow_swipe(&tray, -60.0, 2);
        assert_eq!(
            shown(&tray.arrange(listed(&["a", "b"]))),
            "b",
            "clamped to the last"
        );
    }

    #[test]
    fn the_order_is_frozen_while_a_touch_is_down() {
        let (tray, _) = tray_of(2);
        tray.touch_started(-5.0, Instant::now());
        let held = tray.arrange(listed(&["c", "b", "a"]));
        assert_eq!(ids(&held), ["a", "b", "c"], "held, the new card after");
        assert_eq!(shown(&held), "a");
        tray.touch_ended(0.0, Instant::now() + Duration::from_secs(1), WIDTH);
        let lifted = tray.arrange(listed(&["c", "b", "a"]));
        assert_eq!(ids(&lifted), ["c", "b", "a"], "re-sorted on the lift");
        assert_eq!(shown(&lifted), "a", "the shown card kept");
    }

    #[test]
    fn a_single_question_with_three_options_answers_from_the_card() {
        let card = only_card(&question(&["Yes", "All edits", "No"]));
        assert_eq!(
            labels(&card),
            [
                ("Yes", &PromptChoice::Option { index: 0 }),
                ("All edits", &PromptChoice::Option { index: 1 }),
                ("No", &PromptChoice::Option { index: 2 }),
            ]
        );
        assert_eq!(card.text, "Which branch?");
    }

    #[test]
    fn four_options_show_open() {
        let card = only_card(&question(&["a", "b", "c", "d"]));
        assert_eq!(card.buttons, Buttons::Open);
        let several = json!({
            "kind": "question",
            "questions": [
                { "question": "One?", "header": "", "options": ["a"], "multi_select": false },
                { "question": "Two?", "header": "", "options": ["b"], "multi_select": false },
            ],
        });
        assert_eq!(
            only_card(&several).buttons,
            Buttons::Open,
            "several questions"
        );
        let multi = json!({
            "kind": "question",
            "questions": [{ "question": "Which?", "header": "", "options": ["a", "b"], "multi_select": true }],
        });
        assert_eq!(only_card(&multi).buttons, Buttons::Open, "multi-select");
    }

    #[test]
    fn a_card_that_cannot_answer_lists_its_questions_for_reading() {
        let several = json!({
            "kind": "question",
            "questions": [
                {
                    "question": "Keep the newest?",
                    "header": "Migration",
                    "options": ["Keep", "Merge"],
                    "option_descriptions": ["the newest copy", "both copies"],
                    "multi_select": false,
                },
                {
                    "question": "Which checks?",
                    "header": "Checks",
                    "options": ["fmt", "test"],
                    "multi_select": true,
                },
            ],
        });
        let card = only_card(&several);
        assert_eq!(card.buttons, Buttons::Open, "several questions, Open");
        assert_eq!(card.questions.len(), 2, "every question is listed");
        assert_eq!(card.questions[0].header, "Migration");
        assert_eq!(card.questions[0].options[1].label, "Merge");
        assert_eq!(
            card.questions[0].options[1].description.as_deref(),
            Some("both copies")
        );
        assert_eq!(card.questions[1].options[1].label, "test");
        assert_eq!(card.questions[1].options[1].description, None);

        let mut unstamped = waiting("s", 1, &question(&["a", "b"]));
        unstamped.status_since = None;
        let card = cards_of(&model(vec![unstamped])).pop().expect("one card");
        assert_eq!(card.buttons, Buttons::Open, "no stamp, no answer");
        assert_eq!(card.questions.len(), 1, "its question is still listed");

        assert_eq!(only_card(&permission()).questions, [], "no question");
        let plan = json!({ "kind": "plan_approval", "plan": "Ship it" });
        assert_eq!(only_card(&plan).questions, [], "nor a plan approval");
    }

    #[test]
    fn an_answers_description_rides_on_its_button() {
        let mut described = question(&["main", "dev"]);
        described["questions"][0]["option_descriptions"] = json!(["the trunk", "the feature line"]);
        let card = only_card(&described);
        assert_eq!(
            descriptions(&card),
            [Some("the trunk"), Some("the feature line")]
        );
    }

    #[test]
    fn a_permission_shows_allow_and_deny() {
        let card = only_card(&permission());
        assert_eq!(
            labels(&card),
            [
                ("Allow", &PromptChoice::Allow),
                ("Deny", &PromptChoice::Deny)
            ]
        );
        assert_eq!(card.command.as_deref(), Some("gh pr merge 118"));
    }

    #[test]
    fn a_plan_approval_shows_open() {
        let plan = json!({ "kind": "plan_approval", "plan": "Ship it" });
        assert_eq!(only_card(&plan).buttons, Buttons::Open);
        let mut unstamped = waiting("s", 1, &question(&["a", "b"]));
        unstamped.status_since = None;
        let card = cards_of(&model(vec![unstamped])).pop().expect("one card");
        assert_eq!(card.buttons, Buttons::Open, "no status stamp");
    }

    #[test]
    fn a_swipe_past_a_third_moves_a_card() {
        let (tray, cards) = tray_of(2);
        assert!(slow_swipe(&tray, -60.0, 2).changed, "120 of 300 moves");
        assert_eq!(shown(&tray.arrange(cards.clone())), "b");
        assert!(slow_swipe(&tray, 60.0, 2).changed, "and back the other way");
        assert_eq!(shown(&tray.arrange(cards)), "a");
    }

    #[test]
    fn a_short_swipe_snaps_back() {
        let (tray, cards) = tray_of(2);
        let start = Instant::now();
        tray.touch_started(0.0, start);
        tray.touch_moved(-40.0, start + Duration::from_millis(200));
        assert!(
            (tray.offset() + 40.0).abs() < f32::EPSILON,
            "drawn off by the travel"
        );
        assert!(
            !tray
                .touch_ended(0.0, start + Duration::from_millis(400), WIDTH)
                .changed
        );
        assert!(tray.offset().abs() < f32::EPSILON, "snapped back");
        assert_eq!(shown(&tray.arrange(cards)), "a");
    }

    /// A touch down at 0 ms, each of `moves` (ms, dx) in turn, then the
    /// lift at `lift` ms; the clock is the test's own.
    fn drag(tray: &Tray, moves: &[(u64, f32)], lift: u64) -> Lift {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        tray.touch_started(0.0, start);
        for &(ms, dx) in moves {
            tray.touch_moved(dx, at(ms));
        }
        tray.touch_ended(0.0, at(lift), WIDTH)
    }

    #[test]
    fn a_slow_short_drag_snaps_back() {
        let (tray, cards) = tray_of(2);
        let moves: Vec<(u64, f32)> = (1..=6).map(|n| (n * 50, -10.0)).collect();
        assert!(
            !drag(&tray, &moves, 360).changed,
            "60 px over 300 ms, then a 60 ms pause"
        );
        assert_eq!(shown(&tray.arrange(cards)), "a");
    }

    #[test]
    fn an_eased_back_drag_does_not_move_backwards() {
        let (tray, cards) = tray_of(2);
        assert!(slow_swipe(&tray, -60.0, 2).changed, "on b");
        let moves = [
            (100, -40.0),
            (200, -40.0),
            (240, 2.0),
            (245, 2.0),
            (250, 2.0),
        ];
        assert!(
            !drag(&tray, &moves, 250).changed,
            "-80 px, then +6 px in the last 30 ms: no move either way"
        );
        assert_eq!(shown(&tray.arrange(cards)), "b");
    }

    #[test]
    fn a_fast_flick_moves() {
        let (tray, cards) = tray_of(2);
        assert!(
            drag(&tray, &[(10, -10.0), (30, -10.0), (50, -10.0)], 50).changed,
            "30 px in 50 ms"
        );
        assert_eq!(shown(&tray.arrange(cards)), "b");
    }

    #[test]
    fn a_swipe_ends_when_the_cards_go() {
        let (tray, _) = tray_of(2);
        tray.touch_started(-5.0, Instant::now());
        assert!(tray.arrange(Vec::new()).shown().is_none(), "the tray hides");
        let back = tray.arrange(listed(&["b", "a"]));
        assert!(tray.offset().abs() < f32::EPSILON, "drawn in place");
        assert_eq!(ids(&back), ["b", "a"], "in the list's order");
    }

    #[test]
    fn the_ends_do_not_wrap() {
        let (tray, cards) = tray_of(2);
        assert!(
            !slow_swipe(&tray, 60.0, 2).changed,
            "nothing before the first"
        );
        assert_eq!(shown(&tray.arrange(cards.clone())), "a");
        assert!(slow_swipe(&tray, -60.0, 2).changed);
        assert!(
            !slow_swipe(&tray, -60.0, 2).changed,
            "nothing after the last"
        );
        assert_eq!(shown(&tray.arrange(cards)), "b");
    }

    #[test]
    fn a_lift_after_a_sideways_swipe_says_it_swiped() {
        let (tray, cards) = tray_of(2);
        assert!(slow_swipe(&tray, -60.0, 2).changed, "moved to b");
        let lift = slow_swipe(&tray, -60.0, 2);
        assert!(!lift.changed, "the last card has nowhere to go");
        assert!(lift.swiped, "120 px sideways is a swipe, not a tap");
        assert_eq!(shown(&tray.arrange(cards)), "b");
    }

    #[test]
    fn a_lift_with_little_sideways_travel_did_not_swipe() {
        let (tray, _) = tray_of(2);
        let start = Instant::now();
        tray.touch_started(0.0, start);
        let lift = tray.touch_ended(-10.0, start + Duration::from_millis(400), WIDTH);
        assert!(!lift.changed, "10 px moves nothing");
        assert!(!lift.swiped, "10 px is a tap's jitter");
    }
}
