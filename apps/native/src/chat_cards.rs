//! The prompt card: the buttons each prompt kind offers, which key answers
//! what, and the busy and "Answered elsewhere" marks a session's cards
//! carry, as plain rules; then the card drawn from them. The chat pane and
//! the summary view draw the same card, and the caller sends what a press
//! picks.

use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Instant;

use gpui::{AnyElement, App, ClickEvent, Div, Window, div, prelude::*, px};
use protocol::{ChatAnswerKind, ChatPrompt, ChatPromptKind, ClientMessage};

use crate::UI_TEXT_SIZE;
use crate::buttons::{ButtonSize, Layer, outlined_button, primary_button};
use crate::chat::{PromptStack, allow_with_text, deny_with_text, type_something_answer};
use crate::fonts::ui_px_at;
use crate::ui_theme::UiTheme;

/// The line a card closed in the terminal or on another device leaves.
pub const ANSWERED_ELSEWHERE: &str = "Answered elsewhere";
/// What a card the chat cannot answer says.
pub const OPEN_THE_TERMINAL: &str = "Open the terminal to answer this";
/// What a submitted card says until the screen drops it.
pub const SENDING: &str = "Sending\u{2026}";

/// The answer composer text rides on that allows, naming the session's
/// agent as people read it (`agent`).
#[must_use]
pub fn ride_yes_label(agent: &str) -> String {
    format!("Yes, and tell {agent} this")
}

/// The answer composer text rides on that denies.
#[must_use]
pub fn ride_no_label(agent: &str) -> String {
    format!("No, and tell {agent} this")
}

/// How many of a session's answer requests are remembered, so a late
/// refusal of one is known as ours.
const SENT_CAP: usize = 16;

/// A permission prompt's option labelled exactly `label`, the last such.
fn labelled(prompt: &ChatPrompt, label: &str) -> Option<u32> {
    if prompt.kind != ChatPromptKind::Permission {
        return None;
    }
    prompt
        .options
        .iter()
        .rev()
        .find(|option| option.label.trim() == label)
        .map(|option| option.number)
}

/// The number of a permission prompt's "Yes" option, which allows once.
#[must_use]
pub fn allow_once_option(prompt: &ChatPrompt) -> Option<u32> {
    labelled(prompt, "Yes")
}

/// The number of a permission prompt's other "Yes, …" option, which allows
/// always, when the dialog offers one.
#[must_use]
pub fn always_allow_option(prompt: &ChatPrompt) -> Option<u32> {
    let once = allow_once_option(prompt);
    if prompt.kind != ChatPromptKind::Permission {
        return None;
    }
    prompt
        .options
        .iter()
        .find(|option| Some(option.number) != once && option.label.starts_with("Yes"))
        .map(|option| option.number)
}

/// The number of a permission prompt's "No" option, which Esc picks.
#[must_use]
pub fn deny_option(prompt: &ChatPrompt) -> Option<u32> {
    labelled(prompt, "No")
}

/// A key the chat reads before the composer does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKey {
    Digit(u32),
    Escape,
    CtrlEnter,
    CtrlShiftEnter,
}

/// What a key does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    /// Answers the current card.
    Answer(ChatAnswerKind),
    /// Sends the composer's text.
    Send,
    /// Taken, doing nothing: never typed.
    Swallow,
    /// Left to the composer.
    Pass,
}

/// What `key` does with `stack`'s current card (`busy` when it was
/// submitted and is not yet gone) and `composer` holding that text, at
/// `now`. Digits answer from an empty composer only after the grace, and
/// are dropped inside it; Esc denies a permission card; Ctrl+Enter sends
/// text when no card is up, allows a permission card once from an empty
/// composer, and leaves text with a card up to the ride-on buttons;
/// Ctrl+Shift+Enter always allows a permission card.
#[must_use]
pub fn key_action(
    stack: &PromptStack,
    busy: bool,
    key: CardKey,
    composer: &str,
    now: Instant,
) -> KeyAction {
    let empty = composer.trim().is_empty();
    let Some(prompt) = stack.current() else {
        return match key {
            CardKey::CtrlEnter if empty => KeyAction::Swallow,
            CardKey::CtrlEnter => KeyAction::Send,
            _ => KeyAction::Pass,
        };
    };
    let permission = prompt.kind == ChatPromptKind::Permission;
    let answer = match key {
        CardKey::Digit(_) if !empty => return KeyAction::Pass,
        CardKey::Digit(digit) => stack.digit_answer(digit, true, prompt.multi, now),
        CardKey::Escape | CardKey::CtrlShiftEnter if !permission => return KeyAction::Pass,
        CardKey::Escape => deny_option(prompt).map(option),
        CardKey::CtrlEnter if empty => allow_once_option(prompt).map(option),
        CardKey::CtrlEnter => None,
        CardKey::CtrlShiftEnter => always_allow_option(prompt).map(option),
    };
    match answer {
        Some(answer) if !busy => KeyAction::Answer(answer),
        _ => KeyAction::Swallow,
    }
}

fn option(number: u32) -> ChatAnswerKind {
    ChatAnswerKind::Option { number }
}

/// What a card button sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ButtonAnswer {
    /// This answer.
    Fixed(ChatAnswerKind),
    /// The composer's text as the user's own answer.
    TypedText,
}

impl ButtonAnswer {
    /// The answer a press sends with `composer` holding that text: none for
    /// a typed-text button while the composer is blank.
    #[must_use]
    pub fn resolve(&self, composer: &str) -> Option<ChatAnswerKind> {
        match self {
            Self::Fixed(answer) => Some(answer.clone()),
            Self::TypedText => type_something_answer(composer),
        }
    }
}

/// One button of a card: its selector suffix (`chat-card-<id>`), label,
/// tooltip and answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardButton {
    pub id: String,
    pub label: String,
    pub tip: Option<String>,
    pub answer: ButtonAnswer,
}

impl CardButton {
    fn numbered(number: u32, label: &str, answer: ButtonAnswer) -> Self {
        Self {
            id: number.to_string(),
            label: label.to_owned(),
            tip: None,
            answer,
        }
    }

    fn named(id: &str, label: &str, answer: ChatAnswerKind) -> Self {
        Self {
            id: id.to_owned(),
            label: label.to_owned(),
            tip: None,
            answer: ButtonAnswer::Fixed(answer),
        }
    }
}

/// The label of a question's own-text option.
const TYPE_SOMETHING: &str = "Type something";

/// The buttons `prompt`'s card offers, in order, naming the session's agent
/// as people read it (`agent`); none for a kind the chat cannot answer.
#[must_use]
pub fn card_buttons(prompt: &ChatPrompt, agent: &str) -> Vec<CardButton> {
    match prompt.kind {
        ChatPromptKind::Permission => permission_buttons(prompt),
        ChatPromptKind::Question => question_buttons(prompt),
        ChatPromptKind::Review => positional(prompt, &["Submit", "Cancel"], None),
        ChatPromptKind::Plan => positional(
            prompt,
            &[
                "Approve (auto)",
                "Approve (manual edits)",
                &format!("Tell {agent} what to change"),
            ],
            Some(2),
        ),
        ChatPromptKind::Trust => positional(prompt, &["Trust this folder", "Exit"], None),
        ChatPromptKind::HeadsUp => positional(prompt, &[], None),
        ChatPromptKind::Unknown => Vec::new(),
    }
}

/// Allow once, Always allow (when offered, its tooltip the rule) and Deny.
fn permission_buttons(prompt: &ChatPrompt) -> Vec<CardButton> {
    let mut buttons = Vec::with_capacity(3);
    if let Some(number) = allow_once_option(prompt) {
        buttons.push(CardButton::numbered(
            number,
            "Allow once",
            ButtonAnswer::Fixed(option(number)),
        ));
    }
    if let Some(number) = always_allow_option(prompt) {
        let rule = prompt
            .options
            .iter()
            .find(|option| option.number == number)
            .map(|option| option.label.clone());
        let mut button =
            CardButton::numbered(number, "Always allow", ButtonAnswer::Fixed(option(number)));
        button.tip = prompt.always_allow_tip.clone().or(rule);
        buttons.push(button);
    }
    if let Some(number) = deny_option(prompt) {
        buttons.push(CardButton::numbered(
            number,
            "Deny",
            ButtonAnswer::Fixed(option(number)),
        ));
    }
    buttons
}

/// One button per option, labelled by `labels` in order and by the
/// option's own label past them; the option at `typed` takes the composer's
/// text.
fn positional(prompt: &ChatPrompt, labels: &[&str], typed: Option<usize>) -> Vec<CardButton> {
    prompt
        .options
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            let label = labels.get(index).copied().unwrap_or(choice.label.as_str());
            let answer = if Some(index) == typed {
                ButtonAnswer::TypedText
            } else {
                ButtonAnswer::Fixed(option(choice.number))
            };
            CardButton::numbered(choice.number, label, answer)
        })
        .collect()
}

/// A question's options (picked, or toggled on a multi-select), its own
/// text option, and with several questions Back and Skip; a multi-select
/// ends with Next, or Submit on its last question.
fn question_buttons(prompt: &ChatPrompt) -> Vec<CardButton> {
    let mut buttons: Vec<CardButton> = prompt
        .options
        .iter()
        .map(|choice| {
            let answer = if choice.label == TYPE_SOMETHING {
                ButtonAnswer::TypedText
            } else if prompt.multi {
                ButtonAnswer::Fixed(ChatAnswerKind::Toggle {
                    number: choice.number,
                })
            } else {
                ButtonAnswer::Fixed(option(choice.number))
            };
            CardButton::numbered(choice.number, &choice.label, answer)
        })
        .collect();
    if prompt.questions.len() > 1 {
        buttons.push(CardButton::named("back", "Back", ChatAnswerKind::Back));
        buttons.push(CardButton::named("skip", "Skip", ChatAnswerKind::Skip));
    }
    if prompt.multi {
        let last = prompt
            .questions
            .last()
            .is_none_or(|question| question.current);
        let label = if last { "Submit" } else { "Next" };
        buttons.push(CardButton::named("submit", label, ChatAnswerKind::Submit));
    }
    buttons
}

/// A button beside the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerAction {
    /// Sends the composer's text.
    Send,
    /// Allows the card, the text riding on the answer.
    RideYes,
    /// Denies the card, the text riding on the answer.
    RideNo,
}

impl ComposerAction {
    /// The button's selector.
    #[must_use]
    pub fn selector(self) -> &'static str {
        match self {
            Self::Send => "chat-send",
            Self::RideYes => "chat-ride-yes",
            Self::RideNo => "chat-ride-no",
        }
    }
}

/// The buttons beside a composer holding `text` while `prompt`'s card is
/// up: Send while no card is; with text and a card, the ride-on answers
/// the card takes (Yes when it takes an amendment, No on a permission
/// card); with no text and a card, none.
#[must_use]
pub fn composer_actions(prompt: Option<&ChatPrompt>, text: &str) -> Vec<ComposerAction> {
    let Some(prompt) = prompt else {
        return vec![ComposerAction::Send];
    };
    let mut actions = Vec::with_capacity(2);
    if allow_with_text(prompt, text).is_some() {
        actions.push(ComposerAction::RideYes);
    }
    if deny_with_text(prompt, text).is_some() {
        actions.push(ComposerAction::RideNo);
    }
    actions
}

/// What a refusal of one of our answers did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerRefusal {
    /// Its card had already left: nothing changed.
    Late,
    /// Its card is free again; `text` is the composer text the answer
    /// took, to give back.
    Freed,
}

/// The message that answers the card drawn for `drawn_prompt_id` with
/// `answer` for `session_id`: it names that prompt even when another one has
/// replaced it on the screen since, so the daemon refuses a stale click.
#[must_use]
pub(crate) fn card_answer_message(
    session_id: &str,
    drawn_prompt_id: &str,
    answer: ChatAnswerKind,
    request_id: Option<String>,
) -> ClientMessage {
    ClientMessage::ChatAnswer {
        session_id: session_id.to_owned(),
        prompt_id: drawn_prompt_id.to_owned(),
        answer,
        request_id,
    }
}

/// A submitted answer the screen has not yet dropped the card of.
#[derive(Debug, Clone)]
struct Busy {
    prompt_id: String,
    request_id: String,
}

/// A session's card marks: the card submitted and waiting for the screen to
/// drop it, the reason the daemon refused the last answer (by prompt id),
/// whether a card closed without our answer, and the recent answers' request
/// ids with their prompt ids.
#[derive(Debug, Default)]
pub struct CardState {
    busy: Option<Busy>,
    refusal: Option<(String, String)>,
    elsewhere: bool,
    sent: VecDeque<(String, String)>,
    /// The prompt the screen shows now.
    shown: Option<String>,
}

impl CardState {
    /// An answer to `prompt_id` went out under `request_id`, carrying the
    /// composer's `text` when it took it: the card is busy until the screen
    /// drops it.
    pub fn answered(&mut self, prompt_id: &str, request_id: String) {
        self.remember(prompt_id, &request_id);
        self.busy = Some(Busy {
            prompt_id: prompt_id.to_owned(),
            request_id,
        });
    }

    /// A toggle of `prompt_id`'s option went out under `request_id`: the
    /// dialog stays up with the box flipped, so the card does not go busy.
    pub fn toggled(&mut self, prompt_id: &str, request_id: &str) {
        self.remember(prompt_id, request_id);
    }

    /// `answer` to the card drawn for `prompt_id` went out under
    /// `request_id` while the stack shows `current`. An answer other than a
    /// toggle to the current card makes it busy; an answer to a card gone
    /// since is only remembered, so its refusal is still recognised and the
    /// current card's busy mark stays where it is.
    pub fn clicked(
        &mut self,
        prompt_id: &str,
        current: Option<&str>,
        request_id: String,
        answer: &ChatAnswerKind,
    ) {
        if current != Some(prompt_id) || matches!(answer, ChatAnswerKind::Toggle { .. }) {
            self.toggled(prompt_id, &request_id);
        } else {
            self.answered(prompt_id, request_id);
        }
    }

    fn remember(&mut self, prompt_id: &str, request_id: &str) {
        if self
            .refusal
            .as_ref()
            .is_some_and(|(refused, _)| refused == prompt_id)
        {
            self.refusal = None;
        }
        self.sent
            .push_back((request_id.to_owned(), prompt_id.to_owned()));
        if self.sent.len() > SENT_CAP {
            self.sent.pop_front();
        }
    }

    /// Whether the card of `prompt_id` was submitted and is still up.
    #[must_use]
    pub fn is_busy(&self, prompt_id: &str) -> bool {
        self.busy
            .as_ref()
            .is_some_and(|busy| busy.prompt_id == prompt_id)
    }

    /// The screen's prompt went from `before` to `after`: a card it dropped
    /// that we submitted collapses, one we did not reads "Answered
    /// elsewhere".
    pub fn on_screen(&mut self, before: Option<&str>, after: Option<&str>) {
        self.shown = after.map(str::to_owned);
        let Some(before) = before.filter(|before| Some(*before) != after) else {
            return;
        };
        if self.is_busy(before) {
            self.busy = None;
        } else {
            self.elsewhere = true;
        }
        if self
            .refusal
            .as_ref()
            .is_some_and(|(refused, _)| refused == before)
        {
            self.refusal = None;
        }
    }

    /// A new chat entry arrived: the "Answered elsewhere" line goes.
    pub fn on_new_entry(&mut self) {
        self.elsewhere = false;
    }

    /// Whether the "Answered elsewhere" line shows.
    #[must_use]
    pub fn answered_elsewhere(&self) -> bool {
        self.elsewhere
    }

    /// The daemon refused `request_id` with `message`: `None` when it is
    /// not one of our answers. A refusal for a card still up frees it and
    /// keeps the reason on it; one for a card that already left changes
    /// nothing.
    pub fn refused(&mut self, request_id: &str, message: &str) -> Option<AnswerRefusal> {
        let prompt_id = self
            .sent
            .iter()
            .find(|(sent, _)| sent == request_id)
            .map(|(_, prompt_id)| prompt_id.clone())?;
        let current = match self.busy.as_ref() {
            Some(busy) => busy.request_id == request_id,
            None => self.shown.as_deref() == Some(prompt_id.as_str()),
        };
        if !current {
            return Some(AnswerRefusal::Late);
        }
        self.busy = None;
        self.refusal = Some((prompt_id, message.to_owned()));
        Some(AnswerRefusal::Freed)
    }

    /// The daemon's reason for refusing the last answer to `prompt_id`.
    #[must_use]
    pub fn refusal(&self, prompt_id: &str) -> Option<&str> {
        self.refusal
            .as_ref()
            .filter(|(refused, _)| refused == prompt_id)
            .map(|(_, message)| message.as_str())
    }
}

/// The room around a card's content, its corners, and how far each ghost
/// edge behind it is inset.
const CARD_PAD: f32 = 10.0;
const CARD_RADIUS: f32 = 8.0;
const GHOST_INSET: f32 = 8.0;
const GHOST_HEIGHT: f32 = 4.0;
/// How many ghost edges a card shows at most.
const GHOST_EDGES: usize = 2;
const SMALL_TEXT: f32 = 12.0;

/// What a card draws: its prompt, the agent its labels name, whether it was
/// submitted and is still up, the daemon's reason for refusing its last
/// answer, and how many cards wait behind it.
pub struct CardView<'a> {
    pub prompt: &'a ChatPrompt,
    pub agent: &'a str,
    pub busy: bool,
    pub refusal: Option<&'a str>,
    pub ghosts: usize,
}

/// What a card calls with the button pressed; the caller resolves and sends
/// its answer.
pub type OnPress = Rc<dyn Fn(&ButtonAnswer, &mut Window, &mut App)>;

/// The card `view` describes, tagged `chat-card`, with up to two ghost edges
/// over it for the cards behind. Its buttons are tagged `chat-card-<id>` and
/// call `on_press`; a busy card's buttons take no click and it reads
/// "Sending…"; a refusal shows under the buttons (`chat-card-refusal`); a
/// kind the chat cannot answer says to open the terminal
/// (`chat-card-terminal`).
pub fn prompt_card(offset: i8, view: &CardView<'_>, on_press: &OnPress, t: &UiTheme) -> AnyElement {
    let prompt = view.prompt;
    let buttons = card_buttons(prompt, view.agent);
    let actions = if buttons.is_empty() {
        small(offset, OPEN_THE_TERMINAL, t.muted)
            .debug_selector(|| "chat-card-terminal".to_owned())
            .into_any_element()
    } else {
        div()
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .children(
                buttons
                    .iter()
                    .map(|button| card_button(button, prompt, view.busy, on_press, t)),
            )
            .into_any_element()
    };
    let card = div()
        .debug_selector(|| "chat-card".to_owned())
        .flex()
        .flex_col()
        .gap(px(6.0))
        .p(px(CARD_PAD))
        .rounded(px(CARD_RADIUS))
        .border_1()
        .border_color(gpui::rgb(t.line_strong))
        .text_size(ui_px_at(offset, UI_TEXT_SIZE))
        .text_color(gpui::rgb(t.text))
        .when(!prompt.subject.is_empty(), |card| {
            card.child(div().child(prompt.subject.clone()))
        })
        .when(!prompt.body.is_empty(), |card| {
            card.child(small(offset, &prompt.body, t.text_2))
        })
        .child(actions)
        .when(view.busy, |card| {
            card.child(
                small(offset, SENDING, t.muted).debug_selector(|| "chat-card-busy".to_owned()),
            )
        })
        .children(view.refusal.map(|reason| {
            small(offset, reason, t.danger).debug_selector(|| "chat-card-refusal".to_owned())
        }));
    let edges = view.ghosts.min(GHOST_EDGES);
    div()
        .flex()
        .flex_col()
        .children((1..=edges).rev().map(|depth| ghost_edge(depth, t)))
        .child(card)
        .into_any_element()
}

/// One button of a card; a selected multi-select option is filled.
fn card_button(
    button: &CardButton,
    prompt: &ChatPrompt,
    busy: bool,
    on_press: &OnPress,
    t: &UiTheme,
) -> AnyElement {
    let selector = format!("chat-card-{}", button.id);
    let selected = prompt.multi
        && prompt
            .options
            .iter()
            .any(|option| option.selected && option.number.to_string() == button.id);
    let base = if selected {
        primary_button(&selector, ButtonSize::Compact, !busy, Layer::Base, t)
    } else {
        outlined_button(&selector, ButtonSize::Compact, !busy, Layer::Base, t)
    };
    let answer = button.answer.clone();
    let on_press = Rc::clone(on_press);
    base.child(button.label.clone())
        .when_some(button.tip.clone(), |base, tip| {
            base.tooltip(crate::tooltip(tip))
        })
        .when(!busy, move |base| {
            base.on_click(move |_: &ClickEvent, window, cx| on_press(&answer, window, cx))
        })
        .into_any_element()
}

/// The top edge of the `depth`th card behind the current one.
fn ghost_edge(depth: usize, t: &UiTheme) -> Div {
    let inset = GHOST_INSET * f32::from(u8::try_from(depth).unwrap_or(u8::MAX));
    div()
        .mx(px(inset))
        .h(px(GHOST_HEIGHT))
        .rounded_t(px(CARD_RADIUS))
        .border_1()
        .border_b_0()
        .border_color(gpui::rgb(t.line))
}

/// One small line of `text` in `color`.
fn small(offset: i8, text: &str, color: u32) -> Div {
    div()
        .text_size(ui_px_at(offset, SMALL_TEXT))
        .text_color(gpui::rgb(color))
        .child(text.to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use protocol::{
        ChatAnswerKind, ChatOption, ChatPrompt, ChatPromptKind, ChatQuestionTab, ChatScreen,
        ClientMessage, ComposerState,
    };

    use super::{
        AnswerRefusal, ButtonAnswer, CardKey, CardState, KeyAction, allow_once_option,
        always_allow_option, card_answer_message, card_buttons, deny_option, key_action,
    };
    use crate::chat::{DIGIT_GRACE, PromptStack};

    fn option(number: u32, label: &str) -> ChatOption {
        ChatOption {
            number,
            label: label.to_owned(),
            description: None,
            selected: false,
        }
    }

    fn prompt(id: &str, kind: ChatPromptKind, options: Vec<ChatOption>) -> ChatPrompt {
        ChatPrompt {
            id: id.to_owned(),
            kind,
            subject: "calc.py".to_owned(),
            body: String::new(),
            options,
            questions: Vec::new(),
            can_amend: true,
            always_allow_tip: None,
            multi: false,
            cursor: None,
        }
    }

    fn permission(id: &str) -> ChatPrompt {
        prompt(
            id,
            ChatPromptKind::Permission,
            vec![
                option(1, "Yes"),
                option(2, "Yes, and don't ask again for: python *"),
                option(3, "No"),
            ],
        )
    }

    /// A stack showing `prompt` since `at`.
    fn stack(prompt: ChatPrompt, at: Instant) -> PromptStack {
        let mut stack = PromptStack::default();
        stack.on_screen(
            &ChatScreen {
                composer: ComposerState::Dialog,
                prompt: Some(prompt),
            },
            at,
        );
        stack
    }

    fn after_grace(at: Instant) -> Instant {
        at + DIGIT_GRACE
    }

    #[test]
    fn the_permission_options_are_read_from_their_labels() {
        let prompt = permission("p1");
        assert_eq!(allow_once_option(&prompt), Some(1));
        assert_eq!(always_allow_option(&prompt), Some(2));
        assert_eq!(deny_option(&prompt), Some(3));
        let two = prompt_with(ChatPromptKind::Permission, &[(1, "Yes"), (2, "No")]);
        assert_eq!(always_allow_option(&two), None);
        assert_eq!(deny_option(&two), Some(2));
        let plan = prompt_with(ChatPromptKind::Plan, &[(1, "Yes"), (2, "No")]);
        assert_eq!(deny_option(&plan), None, "only a permission card denies");
        assert_eq!(allow_once_option(&plan), None);
    }

    fn prompt_with(kind: ChatPromptKind, options: &[(u32, &str)]) -> ChatPrompt {
        prompt(
            "p1",
            kind,
            options.iter().map(|(n, label)| option(*n, label)).collect(),
        )
    }

    #[test]
    fn a_digit_from_an_empty_composer_waits_out_the_grace_then_answers() {
        let at = Instant::now();
        let stack = stack(permission("p1"), at);
        let inside = at + DIGIT_GRACE.saturating_sub(Duration::from_millis(1));
        assert_eq!(
            key_action(&stack, false, CardKey::Digit(1), "", inside),
            KeyAction::Swallow,
            "a digit inside the grace is dropped, never typed"
        );
        assert_eq!(
            key_action(&stack, false, CardKey::Digit(1), "", after_grace(at)),
            KeyAction::Answer(ChatAnswerKind::Option { number: 1 })
        );
        assert_eq!(
            key_action(&stack, false, CardKey::Digit(7), "", after_grace(at)),
            KeyAction::Swallow,
            "an unlisted digit answers nothing and types nothing"
        );
    }

    #[test]
    fn digits_type_into_a_composer_with_text_or_with_no_card() {
        let at = Instant::now();
        let stack = stack(permission("p1"), at);
        assert_eq!(
            key_action(&stack, false, CardKey::Digit(1), "abc", after_grace(at)),
            KeyAction::Pass
        );
        let empty = PromptStack::default();
        assert_eq!(
            key_action(&empty, false, CardKey::Digit(1), "", after_grace(at)),
            KeyAction::Pass
        );
    }

    #[test]
    fn a_digit_on_a_multi_select_toggles_and_zero_answers_a_heads_up() {
        let at = Instant::now();
        let mut multi = prompt_with(ChatPromptKind::Question, &[(1, "a"), (2, "b")]);
        multi.multi = true;
        assert_eq!(
            key_action(
                &stack(multi, at),
                false,
                CardKey::Digit(2),
                "",
                after_grace(at)
            ),
            KeyAction::Answer(ChatAnswerKind::Toggle { number: 2 })
        );
        let heads_up = prompt_with(
            ChatPromptKind::HeadsUp,
            &[(1, "Learn more"), (2, "Knew this already"), (0, "Dismiss")],
        );
        assert_eq!(
            key_action(
                &stack(heads_up, at),
                false,
                CardKey::Digit(0),
                "",
                after_grace(at)
            ),
            KeyAction::Answer(ChatAnswerKind::Option { number: 0 })
        );
    }

    #[test]
    fn esc_denies_a_permission_card_only() {
        let at = Instant::now();
        assert_eq!(
            key_action(&stack(permission("p1"), at), false, CardKey::Escape, "", at),
            KeyAction::Answer(ChatAnswerKind::Option { number: 3 })
        );
        let trust = prompt_with(ChatPromptKind::Trust, &[(1, "Yes"), (2, "No")]);
        assert_eq!(
            key_action(&stack(trust, at), false, CardKey::Escape, "", at),
            KeyAction::Pass
        );
        assert_eq!(
            key_action(&PromptStack::default(), false, CardKey::Escape, "", at),
            KeyAction::Pass
        );
    }

    #[test]
    fn ctrl_enter_sends_text_allows_once_from_empty_and_waits_on_a_card_with_text() {
        let at = Instant::now();
        let none = PromptStack::default();
        assert_eq!(
            key_action(&none, false, CardKey::CtrlEnter, "hi", at),
            KeyAction::Send
        );
        assert_eq!(
            key_action(&none, false, CardKey::CtrlEnter, " ", at),
            KeyAction::Swallow
        );
        let card = stack(permission("p1"), at);
        assert_eq!(
            key_action(&card, false, CardKey::CtrlEnter, "", at),
            KeyAction::Answer(ChatAnswerKind::Option { number: 1 })
        );
        assert_eq!(
            key_action(&card, false, CardKey::CtrlEnter, "hi", at),
            KeyAction::Swallow,
            "with text and a card, the ride-on buttons answer"
        );
    }

    #[test]
    fn ctrl_shift_enter_always_allows_whenever_a_permission_card_is_up() {
        let at = Instant::now();
        let card = stack(permission("p1"), at);
        for text in ["", "some text"] {
            assert_eq!(
                key_action(&card, false, CardKey::CtrlShiftEnter, text, at),
                KeyAction::Answer(ChatAnswerKind::Option { number: 2 })
            );
        }
        assert_eq!(
            key_action(
                &PromptStack::default(),
                false,
                CardKey::CtrlShiftEnter,
                "",
                at
            ),
            KeyAction::Pass
        );
    }

    #[test]
    fn a_busy_card_takes_no_keys() {
        let at = Instant::now();
        let card = stack(permission("p1"), at);
        for key in [
            CardKey::Digit(1),
            CardKey::Escape,
            CardKey::CtrlEnter,
            CardKey::CtrlShiftEnter,
        ] {
            assert_eq!(
                key_action(&card, true, key, "", after_grace(at)),
                KeyAction::Swallow
            );
        }
    }

    fn fixed(buttons: &[super::CardButton]) -> Vec<(String, String, Option<ChatAnswerKind>)> {
        buttons
            .iter()
            .map(|button| {
                let answer = match &button.answer {
                    ButtonAnswer::Fixed(answer) => Some(answer.clone()),
                    ButtonAnswer::TypedText => None,
                };
                (button.id.clone(), button.label.clone(), answer)
            })
            .collect()
    }

    fn opt(number: u32) -> ChatAnswerKind {
        ChatAnswerKind::Option { number }
    }

    fn row(
        id: &str,
        label: &str,
        answer: impl Into<Option<ChatAnswerKind>>,
    ) -> (String, String, Option<ChatAnswerKind>) {
        (id.to_owned(), label.to_owned(), answer.into())
    }

    #[test]
    fn a_permission_card_offers_allow_once_always_allow_and_deny() {
        let mut prompt = permission("p1");
        prompt.always_allow_tip = Some("python * in .claude/settings.local.json".to_owned());
        let buttons = card_buttons(&prompt, "Claude");
        assert_eq!(
            fixed(&buttons),
            [
                row("1", "Allow once", opt(1)),
                row("2", "Always allow", opt(2)),
                row("3", "Deny", opt(3)),
            ]
        );
        assert_eq!(
            buttons[1].tip.as_deref(),
            Some("python * in .claude/settings.local.json")
        );
        prompt.always_allow_tip = None;
        assert_eq!(
            card_buttons(&prompt, "Claude")[1].tip.as_deref(),
            Some("Yes, and don't ask again for: python *"),
            "with no tip the option's own text names the rule"
        );
    }

    #[test]
    fn review_plan_trust_and_heads_up_cards_take_their_labels() {
        let review = prompt_with(
            ChatPromptKind::Review,
            &[(1, "Submit answers"), (2, "Cancel")],
        );
        assert_eq!(
            fixed(&card_buttons(&review, "Claude")),
            [row("1", "Submit", opt(1)), row("2", "Cancel", opt(2))]
        );
        let plan = prompt_with(
            ChatPromptKind::Plan,
            &[
                (1, "Yes, auto"),
                (2, "Yes, manual"),
                (3, "No, keep planning"),
            ],
        );
        assert_eq!(
            fixed(&card_buttons(&plan, "Claude")),
            [
                row("1", "Approve (auto)", opt(1)),
                row("2", "Approve (manual edits)", opt(2)),
                row("3", "Tell Claude what to change", None),
            ]
        );
        let trust = prompt_with(
            ChatPromptKind::Trust,
            &[(1, "Yes, proceed"), (2, "No, exit")],
        );
        assert_eq!(
            fixed(&card_buttons(&trust, "Claude")),
            [
                row("1", "Trust this folder", opt(1)),
                row("2", "Exit", opt(2))
            ]
        );
        let heads_up = prompt_with(
            ChatPromptKind::HeadsUp,
            &[(1, "Learn more"), (2, "Knew this already"), (0, "Dismiss")],
        );
        assert_eq!(
            fixed(&card_buttons(&heads_up, "Claude")),
            [
                row("1", "Learn more", opt(1)),
                row("2", "Knew this already", opt(2)),
                row("0", "Dismiss", opt(0)),
            ]
        );
        let unknown = prompt_with(ChatPromptKind::Unknown, &[(1, "a")]);
        assert_eq!(card_buttons(&unknown, "Claude"), []);
    }

    #[test]
    fn a_question_card_picks_or_toggles_and_offers_back_skip_and_submit() {
        let single = prompt_with(
            ChatPromptKind::Question,
            &[(1, "sub"), (2, "div"), (3, "Type something")],
        );
        assert_eq!(
            fixed(&card_buttons(&single, "Claude")),
            [
                row("1", "sub", opt(1)),
                row("2", "div", opt(2)),
                row("3", "Type something", None),
            ]
        );
        let mut multi = single.clone();
        multi.multi = true;
        multi.questions = vec![tab("Op", true), tab("Base", false)];
        assert_eq!(
            fixed(&card_buttons(&multi, "Claude")),
            [
                row("1", "sub", Some(ChatAnswerKind::Toggle { number: 1 })),
                row("2", "div", Some(ChatAnswerKind::Toggle { number: 2 })),
                row("3", "Type something", None),
                row("back", "Back", Some(ChatAnswerKind::Back)),
                row("skip", "Skip", Some(ChatAnswerKind::Skip)),
                row("submit", "Next", Some(ChatAnswerKind::Submit)),
            ]
        );
        multi.questions = vec![tab("Op", false), tab("Base", true)];
        assert_eq!(
            card_buttons(&multi, "Claude")
                .last()
                .map(|button| button.label.clone()),
            Some("Submit".to_owned()),
            "the last question submits"
        );
    }

    fn tab(label: &str, current: bool) -> ChatQuestionTab {
        ChatQuestionTab {
            label: label.to_owned(),
            answered: false,
            current,
        }
    }

    #[test]
    fn the_composer_offers_send_or_the_ride_on_answers_the_card_takes() {
        use super::{ComposerAction, composer_actions};
        assert_eq!(composer_actions(None, ""), [ComposerAction::Send]);
        let card = permission("p1");
        assert_eq!(composer_actions(Some(&card), " "), []);
        assert_eq!(
            composer_actions(Some(&card), "careful"),
            [ComposerAction::RideYes, ComposerAction::RideNo]
        );
        let mut fixed = card.clone();
        fixed.can_amend = false;
        assert_eq!(
            composer_actions(Some(&fixed), "no"),
            [ComposerAction::RideNo]
        );
        let mut plan = prompt_with(ChatPromptKind::Plan, &[(1, "a")]);
        plan.can_amend = false;
        assert_eq!(composer_actions(Some(&plan), "x"), []);
    }

    #[test]
    fn a_typed_text_button_answers_only_with_text() {
        assert_eq!(ButtonAnswer::TypedText.resolve(" "), None);
        assert_eq!(
            ButtonAnswer::TypedText.resolve("use div"),
            Some(ChatAnswerKind::TypeSomething {
                text: "use div".to_owned()
            })
        );
        assert_eq!(
            ButtonAnswer::Fixed(ChatAnswerKind::Back).resolve(""),
            Some(ChatAnswerKind::Back)
        );
    }

    #[test]
    fn a_card_stays_busy_until_the_screen_drops_its_prompt() {
        let mut cards = CardState::default();
        cards.answered("p1", "r1".to_owned());
        assert!(cards.is_busy("p1"));
        cards.on_screen(Some("p1"), Some("p1"));
        assert!(cards.is_busy("p1"), "the same prompt keeps it busy");
        cards.on_screen(Some("p1"), None);
        assert!(!cards.is_busy("p1"));
        assert!(!cards.answered_elsewhere(), "our own answer closed it");
    }

    #[test]
    fn a_card_gone_without_our_answer_reads_answered_elsewhere_until_the_next_entry() {
        let mut cards = CardState::default();
        cards.on_screen(None, Some("p1"));
        assert!(!cards.answered_elsewhere());
        cards.on_screen(Some("p1"), Some("p2"));
        assert!(cards.answered_elsewhere());
        cards.on_new_entry();
        assert!(!cards.answered_elsewhere());
    }

    #[test]
    fn a_refused_answer_frees_the_card_and_keeps_the_reason_on_it() {
        let mut cards = CardState::default();
        cards.answered("p1", "r1".to_owned());
        assert_eq!(cards.refused("r9", "nope"), None, "not ours");
        assert_eq!(
            cards.refused("r1", "the terminal did not respond"),
            Some(AnswerRefusal::Freed)
        );
        assert!(!cards.is_busy("p1"));
        assert_eq!(cards.refusal("p1"), Some("the terminal did not respond"));
        assert_eq!(cards.refusal("p2"), None);
        cards.answered("p1", "r2".to_owned());
        assert_eq!(cards.refusal("p1"), None, "a new answer clears the reason");
    }

    #[test]
    fn a_late_refusal_for_a_prompt_that_left_changes_nothing() {
        let mut cards = CardState::default();
        cards.answered("p1", "r1".to_owned());
        cards.on_screen(Some("p1"), Some("p2"));
        cards.answered("p2", "r2".to_owned());
        assert_eq!(
            cards.refused("r1", "late"),
            Some(AnswerRefusal::Late),
            "ours, but late"
        );
        assert!(cards.is_busy("p2"));
        assert_eq!(cards.refusal("p1"), None);
        assert_eq!(cards.refusal("p2"), None);
    }

    #[test]
    fn a_toggle_never_holds_the_card_busy() {
        let mut cards = CardState::default();
        cards.on_screen(None, Some("p1"));
        cards.toggled("p1", "r1");
        assert!(!cards.is_busy("p1"));
        assert_eq!(cards.refused("r1", "no"), Some(AnswerRefusal::Freed));
        assert_eq!(cards.refusal("p1"), Some("no"));
    }

    #[test]
    fn a_card_answer_uses_the_prompt_it_was_drawn_for_not_the_current_one() {
        let drawn = permission("p1");
        let stack = stack(permission("p2"), Instant::now());
        let current = stack.current().map(|prompt| prompt.id.clone());
        assert_eq!(current.as_deref(), Some("p2"));
        let message = card_answer_message(
            "session-1",
            &drawn.id,
            ChatAnswerKind::Option { number: 1 },
            Some("r1".to_owned()),
        );
        assert!(
            matches!(
                message,
                ClientMessage::ChatAnswer { ref prompt_id, .. } if prompt_id == "p1"
            ),
            "the message names the drawn prompt, not the current one"
        );
    }

    #[test]
    fn a_click_on_a_gone_card_leaves_the_current_cards_busy_mark() {
        let mut cards = CardState::default();
        cards.on_screen(None, Some("p2"));
        cards.answered("p2", "r1".to_owned());
        cards.clicked(
            "p1",
            Some("p2"),
            "r2".to_owned(),
            &ChatAnswerKind::Option { number: 1 },
        );
        assert!(cards.is_busy("p2"), "the current card stays busy");
        assert!(!cards.is_busy("p1"));
        assert_eq!(
            cards.refused("r2", "gone"),
            Some(AnswerRefusal::Late),
            "the gone card's refusal is still ours"
        );
        assert!(cards.is_busy("p2"));
    }
}
