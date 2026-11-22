//! The prompt card: the buttons each prompt kind offers, which key answers
//! what, and the busy and "Answered elsewhere" marks a session's cards
//! carry, as plain rules; then the card drawn from them. The chat pane and
//! the summary view draw the same card, and the caller sends what a press
//! picks.

use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Instant;

use gpui::{AnyElement, App, ClickEvent, Div, Entity, Stateful, Window, div, prelude::*, px};
use protocol::{ChatAnswerKind, ChatOption, ChatPrompt, ChatPromptKind, ClientMessage};

use crate::UI_TEXT_SIZE;
use crate::buttons::{
    ButtonSize, Layer, field_frame, outlined_button, primary_button, read_only_outlined_button,
};
use crate::chat::{PromptStack, allow_with_text, deny_with_text, type_something_answer};
use crate::dashboard_rows::{AskOption, AskQuestion};
use crate::driver_id::DriverId as _;
use crate::fonts::ui_px_at;
use crate::text_input::TextInput;
use crate::ui_theme::UiTheme;
use crate::viewer_md_view::{LinkRoute, linked_text};

/// The line a card closed in the terminal or on another device leaves.
pub const ANSWERED_ELSEWHERE: &str = "Answered elsewhere";
/// What a card the chat cannot answer says.
pub const OPEN_THE_TERMINAL: &str = "Open the terminal to answer this";
/// What a submitted card says until the screen drops it.
pub const SENDING: &str = "Sending\u{2026}";

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
    /// Interrupts the running turn.
    Interrupt,
    /// Taken, doing nothing: never typed.
    Swallow,
    /// Left to the composer.
    Pass,
}

/// What `key` does with `stack`'s current card (`busy` when it was
/// submitted and is not yet gone) and `composer` holding that text, at
/// `now`, `working` when the agent's last screen reading says a turn is
/// running. Digits answer from an empty composer only after the grace, and
/// are dropped inside it; Esc denies a permission card, and with no card up
/// interrupts a running turn; Ctrl+Enter sends text when no card is up,
/// allows a permission card once from an empty composer, and with text and
/// a card up does nothing; Ctrl+Shift+Enter always allows a permission
/// card.
#[must_use]
pub fn key_action(
    stack: &PromptStack,
    busy: bool,
    key: CardKey,
    composer: &str,
    working: bool,
    now: Instant,
) -> KeyAction {
    let empty = composer.trim().is_empty();
    let Some(prompt) = stack.current() else {
        return match key {
            CardKey::Escape if working => KeyAction::Interrupt,
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
    /// The card box's text as the user's own answer.
    TypedText,
    /// Allows the permission card, the card box's text alongside.
    AllowWithText,
    /// Denies the permission card, the card box's text alongside.
    DenyWithText,
}

impl ButtonAnswer {
    /// The answer a press on `prompt`'s card sends with its box holding
    /// `text`: none for a button that takes the box's text while the box
    /// is blank, or for one the prompt does not take.
    #[must_use]
    pub fn resolve(&self, prompt: &ChatPrompt, text: &str) -> Option<ChatAnswerKind> {
        match self {
            Self::Fixed(answer) => Some(answer.clone()),
            Self::TypedText => type_something_answer(text),
            Self::AllowWithText => allow_with_text(prompt, text),
            Self::DenyWithText => deny_with_text(prompt, text),
        }
    }

    /// Whether the press takes the card box's text.
    #[must_use]
    pub fn takes_text(&self) -> bool {
        !matches!(self, Self::Fixed(_))
    }
}

/// What `prompt`'s card box says while empty, naming the session's agent
/// as people read it (`agent`): on a permission card what to tell the agent
/// instead, else the label of the option that takes typed text; none for a
/// card without such an option, which draws no box.
#[must_use]
pub fn box_placeholder(prompt: &ChatPrompt, agent: &str) -> Option<String> {
    if prompt.kind == ChatPromptKind::Permission {
        return Some(format!("Tell {agent} what to do instead"));
    }
    card_buttons(prompt, agent)
        .into_iter()
        .find(|button| button.answer == ButtonAnswer::TypedText)
        .map(|button| button.label)
}

/// The buttons beside a permission card's box: Allow with text when the
/// prompt takes an amendment, then Deny with text; none for another kind.
#[must_use]
pub fn box_buttons(prompt: &ChatPrompt) -> Vec<CardButton> {
    if prompt.kind != ChatPromptKind::Permission {
        return Vec::new();
    }
    let mut buttons = Vec::with_capacity(2);
    if prompt.can_amend {
        buttons.push(CardButton::taking_text(
            "allow-text",
            "Allow with text",
            ButtonAnswer::AllowWithText,
        ));
    }
    buttons.push(CardButton::taking_text(
        "deny-text",
        "Deny with text",
        ButtonAnswer::DenyWithText,
    ));
    buttons
}

/// The number of `prompt`'s option that takes the card box's text, if it
/// has one.
#[must_use]
pub fn typed_option(prompt: &ChatPrompt) -> Option<u32> {
    card_buttons(prompt, "")
        .into_iter()
        .find(|button| button.answer == ButtonAnswer::TypedText)
        .and_then(|button| button.id.parse().ok())
}

/// Where a refused answer's text goes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GiveBack {
    /// To the box it came from, its card still up.
    CardBox,
    /// To the composer, its card gone.
    Composer,
}

/// A session's card box bookkeeping: the card the box belongs to, and the
/// recent answers that took the box's text, by request id with their
/// prompt ids, so a refusal gives the text back.
#[derive(Debug, Default)]
pub struct CardBoxes {
    owner: Option<String>,
    taken: VecDeque<(String, String, String)>,
}

impl CardBoxes {
    /// The screen now shows the card of `prompt_id`, or none: whether the
    /// box starts afresh, as it does for a different card. The same card
    /// shown again keeps its box.
    pub fn show(&mut self, prompt_id: Option<&str>) -> bool {
        if self.owner.as_deref() == prompt_id {
            return false;
        }
        self.owner = prompt_id.map(str::to_owned);
        true
    }

    /// An answer to `prompt_id` went out under `request_id`, taking `text`
    /// from the box.
    pub fn took(&mut self, request_id: String, prompt_id: &str, text: String) {
        self.taken
            .push_back((request_id, prompt_id.to_owned(), text));
        if self.taken.len() > SENT_CAP {
            self.taken.pop_front();
        }
    }

    /// The daemon refused `request_id`: the text that answer took and where
    /// it goes back, to the box while the box still belongs to its card,
    /// else to the composer; none for an answer that took no text.
    pub fn refused(&mut self, request_id: &str) -> Option<(String, GiveBack)> {
        let at = self
            .taken
            .iter()
            .position(|(sent, _, _)| sent == request_id)?;
        let (_, prompt_id, text) = self.taken.remove(at)?;
        let to = if self.owner.as_deref() == Some(prompt_id.as_str()) {
            GiveBack::CardBox
        } else {
            GiveBack::Composer
        };
        Some((text, to))
    }
}

/// `refused` given back to a box now holding `current`, before it with a
/// space between.
#[must_use]
pub fn restored_box_text(refused: &str, current: &str) -> String {
    if current.trim().is_empty() {
        refused.to_owned()
    } else {
        format!("{refused} {current}")
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
        Self::taking_text(id, label, ButtonAnswer::Fixed(answer))
    }

    fn taking_text(id: &str, label: &str, answer: ButtonAnswer) -> Self {
        Self {
            id: id.to_owned(),
            label: label.to_owned(),
            tip: None,
            answer,
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
        ChatPromptKind::McpServer if prompt.multi => server_buttons(prompt),
        ChatPromptKind::McpServer | ChatPromptKind::HeadsUp | ChatPromptKind::Confirm => {
            positional(prompt, &[], None)
        }
        ChatPromptKind::ModelPicker => model_picker_buttons(prompt),
        ChatPromptKind::Unknown => Vec::new(),
    }
}

/// The label of the model picker's last row: the daemon refuses its answer,
/// so the card offers no button for it.
const MORE_MODELS: &str = "More models";

/// The tick the picker draws on the current model's row.
const TICK: &str = "✔";

/// One button per model row, the picker's own label for each, its current
/// model's ticked as the picker ticks it; no button for More models.
fn model_picker_buttons(prompt: &ChatPrompt) -> Vec<CardButton> {
    prompt
        .options
        .iter()
        .filter(|choice| choice.label != MORE_MODELS)
        .map(|choice| {
            let current = choice
                .description
                .as_deref()
                .is_some_and(|text| text == "Current" || text.starts_with("Current · "));
            let label = if current {
                format!("{} {TICK}", choice.label)
            } else {
                choice.label.clone()
            };
            CardButton::numbered(
                choice.number,
                &label,
                ButtonAnswer::Fixed(option(choice.number)),
            )
        })
        .collect()
}

/// The stepper beside a model picker's adjustable row: Less effort, then
/// More effort; none for a picker without such a row, or another kind.
#[must_use]
pub fn adjust_buttons(prompt: &ChatPrompt) -> Vec<CardButton> {
    if prompt.kind != ChatPromptKind::ModelPicker || prompt.adjust.is_none() {
        return Vec::new();
    }
    vec![
        CardButton::named(
            "adjust-down",
            "Less effort",
            ChatAnswerKind::Adjust { raise: false },
        ),
        CardButton::named(
            "adjust-up",
            "More effort",
            ChatAnswerKind::Adjust { raise: true },
        ),
    ]
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

/// The servers of a multi-select new-MCP-servers prompt, each toggled, then
/// Enable selected, which submits the ticked ones; reject all gets no
/// button.
fn server_buttons(prompt: &ChatPrompt) -> Vec<CardButton> {
    let mut buttons: Vec<CardButton> = prompt
        .options
        .iter()
        .map(|choice| {
            CardButton::numbered(
                choice.number,
                &choice.label,
                ButtonAnswer::Fixed(ChatAnswerKind::Toggle {
                    number: choice.number,
                }),
            )
        })
        .collect();
    // A windowed list offers no server, and so no submit: it is answered
    // in the terminal.
    if !buttons.is_empty() {
        buttons.push(CardButton::named(
            "submit",
            "Enable selected",
            ChatAnswerKind::Submit,
        ));
    }
    buttons
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
    /// toggle or an adjust (both leave the dialog up) to the current card
    /// makes it busy; an answer to a card gone since is only remembered, so
    /// its refusal is still recognised and the current card's busy mark
    /// stays where it is.
    pub fn clicked(
        &mut self,
        prompt_id: &str,
        current: Option<&str>,
        request_id: String,
        answer: &ChatAnswerKind,
    ) {
        let stays_up = matches!(
            answer,
            ChatAnswerKind::Toggle { .. } | ChatAnswerKind::Adjust { .. }
        );
        if current != Some(prompt_id) || stays_up {
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
/// The space between a card's parts, its option rows and its tabs.
const CARD_GAP: f32 = 6.0;
const CHIP_PAD_X: f32 = 6.0;
const CHIP_PAD_Y: f32 = 1.0;
const CHIP_RADIUS: f32 = 4.0;
/// The tick before an answered question's tab.
const ANSWERED_MARK: &str = "✔";

/// What a card draws: its prompt, the agent its labels name, whether it was
/// submitted and is still up, the daemon's reason for refusing its last
/// answer, how many cards wait behind it, and its text box, for a card with
/// an option that takes typed text, and the session and route its question,
/// body and option descriptions draw their links for.
pub struct CardView<'a> {
    pub prompt: &'a ChatPrompt,
    pub agent: &'a str,
    pub busy: bool,
    pub refusal: Option<&'a str>,
    pub ghosts: usize,
    pub text_box: Option<&'a Entity<TextInput>>,
    pub(crate) links: CardLinks<'a>,
}

/// The session whose card a link is drawn in, and the route a click on it
/// follows: the desktop chat's or the phone's.
#[derive(Clone, Copy)]
pub(crate) struct CardLinks<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) route: LinkRoute,
}

impl CardLinks<'_> {
    /// `text` with its paths and web addresses drawn as links, named `id`.
    fn text(self, id: &str, text: &str, t: &UiTheme) -> AnyElement {
        linked_text(id, text, self.route, self.session_id, t)
    }
}

/// What a card calls with the button pressed; the caller resolves and sends
/// its answer.
pub type OnPress = Rc<dyn Fn(&ButtonAnswer, &mut Window, &mut App)>;

/// The card `view` describes, tagged `chat-card`, with up to two ghost edges
/// over it for the cards behind. Its buttons are tagged `chat-card-<id>` and
/// call `on_press`; a busy card's buttons take no click and it reads
/// "Sending…"; the option that takes typed text has the card's box beside
/// its button in its row (`chat-card-box`), and a permission card has its
/// box under its buttons with Allow with text (`chat-card-allow-text`,
/// when the prompt takes an amendment) and Deny with text
/// (`chat-card-deny-text`) beside it; a model picker with an adjustable row
/// shows its label (`chat-card-adjust`) under its rows with Less effort
/// (`chat-card-adjust-down`) and More effort (`chat-card-adjust-up`); a
/// refusal shows under the buttons (`chat-card-refusal`); a kind the chat
/// cannot answer says to open the terminal (`chat-card-terminal`).
pub fn prompt_card(
    offset: i8,
    shift: i8,
    view: &CardView<'_>,
    on_press: &OnPress,
    t: &UiTheme,
) -> AnyElement {
    let prompt = view.prompt;
    let buttons = card_buttons(prompt, view.agent);
    let box_row = box_buttons(prompt);
    let actions = if buttons.is_empty() {
        small(offset, OPEN_THE_TERMINAL, t.muted)
            .driver_id(|| "chat-card-terminal".to_owned())
            .into_any_element()
    } else {
        let (rows, named): (Vec<&CardButton>, Vec<&CardButton>) = buttons
            .iter()
            .partition(|button| option_of(prompt, button).is_some());
        div()
            .flex()
            .flex_col()
            .gap(px(CARD_GAP))
            .children(
                rows.into_iter()
                    .map(|button| card_button(offset, shift, button, view, on_press, t)),
            )
            .when(!named.is_empty(), |actions| {
                actions.child(
                    div().flex().flex_wrap().gap(px(CARD_GAP)).children(
                        named
                            .into_iter()
                            .map(|button| card_button(offset, shift, button, view, on_press, t)),
                    ),
                )
            })
            .when_some(
                view.text_box.filter(|_| !box_row.is_empty()),
                |actions, input| {
                    actions.child(
                        div()
                            .flex()
                            .w_full()
                            .items_center()
                            .gap(px(CARD_GAP))
                            .child(box_field(input, t))
                            .children(box_row.iter().map(|button| {
                                card_button(offset, shift, button, view, on_press, t)
                            })),
                    )
                },
            )
            .into_any_element()
    };
    let card = div()
        .driver_id(|| "chat-card".to_owned())
        .flex()
        .flex_col()
        .gap(px(CARD_GAP))
        .p(px(CARD_PAD))
        .rounded(px(CARD_RADIUS))
        .border_1()
        .border_color(gpui::rgb(t.line_strong))
        .text_size(ui_px_at(offset, UI_TEXT_SIZE))
        .text_color(gpui::rgb(t.text))
        .children(question_tab_row(offset, prompt, t))
        .when(!prompt.subject.is_empty(), |card| {
            card.child(
                div()
                    .driver_id(|| "chat-card-subject".to_owned())
                    .child(
                        view.links
                            .text("chat-card-subject-text", &prompt.subject, t),
                    ),
            )
        })
        .when(!prompt.body.is_empty(), |card| {
            card.child(
                small_div(offset, t.text_2)
                    .driver_id(|| "chat-card-body".to_owned())
                    .child(view.links.text("chat-card-body-text", &prompt.body, t)),
            )
        })
        .child(actions)
        .children(adjust_row(offset, shift, view, on_press, t))
        .when(view.busy, |card| {
            card.child(small(offset, SENDING, t.muted).driver_id(|| "chat-card-busy".to_owned()))
        })
        .children(view.refusal.map(|reason| {
            small(offset, reason, t.danger).driver_id(|| "chat-card-refusal".to_owned())
        }));
    let edges = view.ghosts.min(GHOST_EDGES);
    div()
        .flex()
        .flex_col()
        .children((1..=edges).rev().map(|depth| ghost_edge(depth, t)))
        .child(card)
        .into_any_element()
}

/// A model picker card's adjustable row: its label (`chat-card-adjust`),
/// then the [`adjust_buttons`]; none when the prompt has no such row.
fn adjust_row(
    offset: i8,
    shift: i8,
    view: &CardView<'_>,
    on_press: &OnPress,
    t: &UiTheme,
) -> Option<Div> {
    let label = view.prompt.adjust.as_deref()?;
    let buttons = adjust_buttons(view.prompt);
    if buttons.is_empty() {
        return None;
    }
    Some(
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(CARD_GAP))
            .child(small(offset, label, t.text_2).driver_id(|| "chat-card-adjust".to_owned()))
            .children(
                buttons
                    .iter()
                    .map(|button| card_button(offset, shift, button, view, on_press, t)),
            ),
    )
}

/// The option `button` answers, if it answers one.
fn option_of<'a>(prompt: &'a ChatPrompt, button: &CardButton) -> Option<&'a ChatOption> {
    prompt
        .options
        .iter()
        .find(|option| option.number.to_string() == button.id)
}

/// A question card's tabs, each tagged `chat-card-tab-<n>` from 1, the
/// answered ones ticked; the current tab is drawn as the header chip
/// (`chat-card-header`). None for another kind or a question without tabs.
fn question_tab_row(offset: i8, prompt: &ChatPrompt, t: &UiTheme) -> Option<Div> {
    if prompt.kind != ChatPromptKind::Question || prompt.questions.is_empty() {
        return None;
    }
    let tabs = prompt.questions.iter().enumerate().map(|(index, tab)| {
        let label = if tab.answered {
            format!("{ANSWERED_MARK} {}", tab.label)
        } else {
            tab.label.clone()
        };
        let chip = div()
            .px(px(CHIP_PAD_X))
            .py(px(CHIP_PAD_Y))
            .rounded(px(CHIP_RADIUS))
            .text_size(ui_px_at(offset, SMALL_TEXT));
        let chip = if tab.current {
            chip.driver_id(|| "chat-card-header".to_owned())
                .bg(gpui::rgb(t.chip))
                .text_color(gpui::rgb(t.text_2))
        } else {
            chip.text_color(gpui::rgb(t.muted))
        };
        let number = index + 1;
        div()
            .driver_id(move || format!("chat-card-tab-{number}"))
            .child(chip.child(label))
    });
    Some(div().flex().flex_wrap().gap(px(CARD_GAP)).children(tabs))
}

/// One button of a card; a selected multi-select option, and a model
/// picker's highlighted row (whose model the effort belongs to), is filled.
/// A button answering an option is a full-width row, the option's
/// description (`chat-card-option-desc-<n>`) under its label; the option
/// that takes typed text shares its row with the card's box.
fn card_button(
    offset: i8,
    shift: i8,
    button: &CardButton,
    view: &CardView<'_>,
    on_press: &OnPress,
    t: &UiTheme,
) -> AnyElement {
    let (prompt, busy) = (view.prompt, view.busy);
    let selector = format!("chat-card-{}", button.id);
    let choice = option_of(prompt, button);
    let fills = prompt.multi || prompt.kind == ChatPromptKind::ModelPicker;
    let selected = fills && choice.is_some_and(|option| option.selected);
    let answer = button.answer.clone();
    let on_press = Rc::clone(on_press);
    let text_box = view
        .text_box
        .filter(|_| button.answer == ButtonAnswer::TypedText);
    let base = if let Some(option) = choice {
        option_row_linked(
            OptionRow {
                offset,
                shift,
                selector: &selector,
                label: &button.label,
                description: option.description.as_deref(),
                description_selector: Some(format!("chat-card-option-desc-{}", option.number)),
                selected,
                enabled: !busy,
                read_only: false,
                min_height: None,
            },
            Some(view.links),
            t,
        )
        .when(text_box.is_none(), Styled::w_full)
    } else {
        let base = if selected {
            primary_button(
                offset,
                shift,
                &selector,
                ButtonSize::Compact,
                !busy,
                Layer::Base,
                t,
            )
        } else {
            outlined_button(
                offset,
                shift,
                &selector,
                ButtonSize::Compact,
                !busy,
                Layer::Base,
                t,
            )
        };
        base.child(button.label.clone())
    };
    let base = base
        .when_some(button.tip.clone(), |base, tip| {
            base.tooltip(crate::tooltip(tip))
        })
        .when(!busy, move |base| {
            base.on_click(move |_: &ClickEvent, window, cx| on_press(&answer, window, cx))
        });
    match text_box {
        Some(input) => div()
            .flex()
            .w_full()
            .items_center()
            .gap(px(CARD_GAP))
            .child(base.flex_none())
            .child(box_field(input, t))
            .into_any_element(),
        None => base.into_any_element(),
    }
}

/// The colour of an option's description under its label: the filled
/// button's text colour while a multi-select option is selected, else the
/// secondary text colour.
fn option_description_color(selected: bool, t: &UiTheme) -> u32 {
    if selected { t.on_accent } else { t.text_2 }
}

/// One option's row: its label with its description under it, drawn as a
/// button the caller tags and, where it takes a tap, handles.
pub(crate) struct OptionRow<'a> {
    /// The UI text size offset and weight shift.
    pub(crate) offset: i8,
    pub(crate) shift: i8,
    /// The button's selector.
    pub(crate) selector: &'a str,
    /// The option's label.
    pub(crate) label: &'a str,
    /// The description drawn under the label; `None` when it has none.
    pub(crate) description: Option<&'a str>,
    /// The selector the description line takes, where it needs one.
    pub(crate) description_selector: Option<String>,
    /// Whether the option is a selected multi-select choice, drawn filled.
    pub(crate) selected: bool,
    /// Whether the row takes a press.
    pub(crate) enabled: bool,
    /// Whether the row is shown read-only: it takes no press, drawn at full
    /// strength rather than a disabled control's dim.
    pub(crate) read_only: bool,
    /// The least height the row takes, for the phone's 44 pt target.
    pub(crate) min_height: Option<f32>,
}

/// [`OptionRow`] drawn: its label, and under it its description in the
/// colour a filled row's text takes, as plain text. Both wrap inside the
/// row's width, which never grows past what its parent gives it, and the row
/// never shrinks below their height.
pub(crate) fn option_row(row: OptionRow<'_>, t: &UiTheme) -> Stateful<Div> {
    option_row_linked(row, None, t)
}

/// [`option_row`] whose description draws its paths and web addresses as
/// links for `links`, when it is set; a click on one opens the link and
/// does not press the row.
fn option_row_linked(
    row: OptionRow<'_>,
    links: Option<CardLinks<'_>>,
    t: &UiTheme,
) -> Stateful<Div> {
    let (offset, selector) = (row.offset, row.selector);
    let base = row_button(&row, t);
    let base = base
        .flex_col()
        .items_start()
        .min_w(px(0.0))
        .flex_shrink_0()
        .child(div().w_full().min_w(px(0.0)).child(row.label.to_owned()));
    let base = match row.description {
        Some(description) => {
            let text = match links {
                Some(links) => links.text(&format!("{selector}-desc-text"), description, t),
                None => description.to_owned().into_any_element(),
            };
            base.child(
                small_div(offset, option_description_color(row.selected, t))
                    .w_full()
                    .min_w(px(0.0))
                    .child(text)
                    .when_some(row.description_selector, |line, selector| {
                        line.driver_id(move || selector)
                    }),
            )
        }
        None => base,
    };
    match row.min_height {
        Some(height) => base.min_h(px(height)),
        None => base,
    }
}

/// [`OptionRow`]'s button: a read-only row takes no press at full strength,
/// a selected one is filled, and any other is outlined.
fn row_button(row: &OptionRow<'_>, t: &UiTheme) -> Stateful<Div> {
    let (offset, shift, selector) = (row.offset, row.shift, row.selector);
    if row.read_only {
        read_only_outlined_button(offset, shift, selector, ButtonSize::Compact, Layer::Base, t)
    } else if row.selected {
        primary_button(
            offset,
            shift,
            selector,
            ButtonSize::Compact,
            row.enabled,
            Layer::Base,
            t,
        )
    } else {
        outlined_button(
            offset,
            shift,
            selector,
            ButtonSize::Compact,
            row.enabled,
            Layer::Base,
            t,
        )
    }
}

/// The tray card's questions, drawn read-only at the chat card's sizes: each
/// question's heading and text, then each option's label with its
/// description under it, tagged `phone-tray-question-<n>`,
/// `phone-tray-option-<q>-<n>` and `phone-tray-option-desc-<q>-<n>`; nothing
/// takes a tap.
pub(crate) fn tray_questions(
    offset: i8,
    shift: i8,
    questions: &[AskQuestion],
    min_height: f32,
    t: &UiTheme,
) -> Vec<Div> {
    questions
        .iter()
        .zip(1..)
        .map(|(question, number)| tray_question(offset, shift, question, number, min_height, t))
        .collect()
}

/// The `number`th question of [`tray_questions`]: its heading, its text and
/// each option's row, at least `min_height` tall.
fn tray_question(
    offset: i8,
    shift: i8,
    question: &AskQuestion,
    number: usize,
    min_height: f32,
    t: &UiTheme,
) -> Div {
    let heading = (!question.header.is_empty()).then(|| small(offset, &question.header, t.muted));
    let subject = div()
        .driver_id(move || format!("phone-tray-question-{number}"))
        .text_size(ui_px_at(offset, UI_TEXT_SIZE))
        .text_color(gpui::rgb(t.text))
        .child(question.question.clone());
    div()
        .flex()
        .flex_col()
        .min_w(px(0.0))
        .gap(px(CARD_GAP))
        .children(heading)
        .child(subject)
        .children(question.options.iter().zip(1..).map(|(option, at)| {
            let selector = format!("phone-tray-option-{number}-{at}");
            let description_selector = format!("phone-tray-option-desc-{number}-{at}");
            option_row(
                tray_option_row(
                    offset,
                    shift,
                    &selector,
                    option,
                    description_selector,
                    min_height,
                ),
                t,
            )
        }))
}

/// One option row of [`tray_question`] `number`, its label tagged `selector`
/// and its description `description_selector`, at least `min_height` tall;
/// read-only, so a tap leaves it without a pressed fill or pointer cursor.
fn tray_option_row<'a>(
    offset: i8,
    shift: i8,
    selector: &'a str,
    option: &'a AskOption,
    description_selector: String,
    min_height: f32,
) -> OptionRow<'a> {
    OptionRow {
        offset,
        shift,
        selector,
        label: &option.label,
        description: option.description.as_deref(),
        description_selector: Some(description_selector),
        selected: false,
        enabled: false,
        read_only: true,
        min_height: Some(min_height),
    }
}

/// The card's text box (`chat-card-box`), filling the rest of its row.
fn box_field(input: &Entity<TextInput>, t: &UiTheme) -> Div {
    field_frame(false, Layer::Base, t)
        .driver_id(|| "chat-card-box".to_owned())
        .flex_1()
        .min_w(px(0.0))
        .child(input.clone())
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
    small_div(offset, color).child(text.to_owned())
}

/// The box of a small line in `color`, its text still to add.
fn small_div(offset: i8, color: u32) -> Div {
    div()
        .text_size(ui_px_at(offset, SMALL_TEXT))
        .text_color(gpui::rgb(color))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use protocol::{
        ChatAnswerKind, ChatOption, ChatPrompt, ChatPromptKind, ChatQuestionTab, ChatScreen,
        ClientMessage, ComposerState,
    };

    use super::{
        AnswerRefusal, ButtonAnswer, CardKey, CardState, KeyAction, adjust_buttons,
        allow_once_option, always_allow_option, card_answer_message, card_buttons, deny_option,
        key_action, option_description_color, tray_option_row,
    };
    use crate::chat::{DIGIT_GRACE, PromptStack};
    use crate::dashboard_rows::AskOption;

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
            adjust: None,
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
            key_action(&stack, false, CardKey::Digit(1), "", false, inside),
            KeyAction::Swallow,
            "a digit inside the grace is dropped, never typed"
        );
        assert_eq!(
            key_action(&stack, false, CardKey::Digit(1), "", false, after_grace(at)),
            KeyAction::Answer(ChatAnswerKind::Option { number: 1 })
        );
        assert_eq!(
            key_action(&stack, false, CardKey::Digit(7), "", false, after_grace(at)),
            KeyAction::Swallow,
            "an unlisted digit answers nothing and types nothing"
        );
    }

    #[test]
    fn digits_type_into_a_composer_with_text_or_with_no_card() {
        let at = Instant::now();
        let stack = stack(permission("p1"), at);
        assert_eq!(
            key_action(
                &stack,
                false,
                CardKey::Digit(1),
                "abc",
                false,
                after_grace(at)
            ),
            KeyAction::Pass
        );
        let empty = PromptStack::default();
        assert_eq!(
            key_action(&empty, false, CardKey::Digit(1), "", false, after_grace(at)),
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
                false,
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
                false,
                after_grace(at)
            ),
            KeyAction::Answer(ChatAnswerKind::Option { number: 0 })
        );
    }

    #[test]
    fn esc_denies_a_permission_card_only() {
        let at = Instant::now();
        assert_eq!(
            key_action(
                &stack(permission("p1"), at),
                false,
                CardKey::Escape,
                "",
                false,
                at
            ),
            KeyAction::Answer(ChatAnswerKind::Option { number: 3 })
        );
        let trust = prompt_with(ChatPromptKind::Trust, &[(1, "Yes"), (2, "No")]);
        assert_eq!(
            key_action(&stack(trust, at), false, CardKey::Escape, "", false, at),
            KeyAction::Pass
        );
        assert_eq!(
            key_action(
                &PromptStack::default(),
                false,
                CardKey::Escape,
                "",
                false,
                at
            ),
            KeyAction::Pass
        );
    }

    #[test]
    fn key_action_esc_interrupts_a_working_turn_with_no_card_up() {
        let at = Instant::now();
        let none = PromptStack::default();
        assert_eq!(
            key_action(&none, false, CardKey::Escape, "", true, at),
            KeyAction::Interrupt
        );
        assert_eq!(
            key_action(&none, false, CardKey::Escape, "", false, at),
            KeyAction::Pass,
            "a turn that is not running has nothing to interrupt"
        );
    }

    #[test]
    fn key_action_esc_leaves_a_card_alone_while_working() {
        let at = Instant::now();
        assert_eq!(
            key_action(
                &stack(permission("p1"), at),
                false,
                CardKey::Escape,
                "",
                true,
                at
            ),
            KeyAction::Answer(ChatAnswerKind::Option { number: 3 }),
            "a permission card keeps Esc as Deny"
        );
        let trust = prompt_with(ChatPromptKind::Trust, &[(1, "Yes"), (2, "No")]);
        assert_eq!(
            key_action(&stack(trust, at), false, CardKey::Escape, "", true, at),
            KeyAction::Pass
        );
    }

    #[test]
    fn ctrl_enter_sends_text_allows_once_from_empty_and_waits_on_a_card_with_text() {
        let at = Instant::now();
        let none = PromptStack::default();
        assert_eq!(
            key_action(&none, false, CardKey::CtrlEnter, "hi", false, at),
            KeyAction::Send
        );
        assert_eq!(
            key_action(&none, false, CardKey::CtrlEnter, " ", false, at),
            KeyAction::Swallow
        );
        let card = stack(permission("p1"), at);
        assert_eq!(
            key_action(&card, false, CardKey::CtrlEnter, "", false, at),
            KeyAction::Answer(ChatAnswerKind::Option { number: 1 })
        );
        assert_eq!(
            key_action(&card, false, CardKey::CtrlEnter, "hi", false, at),
            KeyAction::Swallow,
            "with text and a card, Ctrl+Enter does nothing"
        );
    }

    #[test]
    fn ctrl_shift_enter_always_allows_whenever_a_permission_card_is_up() {
        let at = Instant::now();
        let card = stack(permission("p1"), at);
        for text in ["", "some text"] {
            assert_eq!(
                key_action(&card, false, CardKey::CtrlShiftEnter, text, false, at),
                KeyAction::Answer(ChatAnswerKind::Option { number: 2 })
            );
        }
        assert_eq!(
            key_action(
                &PromptStack::default(),
                false,
                CardKey::CtrlShiftEnter,
                "",
                false,
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
                key_action(&card, true, key, "", false, after_grace(at)),
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
                    ButtonAnswer::TypedText
                    | ButtonAnswer::AllowWithText
                    | ButtonAnswer::DenyWithText => None,
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
    fn an_mcp_server_card_takes_the_screens_labels() {
        let mcp_server = prompt_with(
            ChatPromptKind::McpServer,
            &[
                (1, "Use this MCP server"),
                (2, "Use this and all future MCP servers in this project"),
                (3, "Continue without using this MCP server"),
            ],
        );
        assert_eq!(
            fixed(&card_buttons(&mcp_server, "Claude")),
            [
                row("1", "Use this MCP server", opt(1)),
                row(
                    "2",
                    "Use this and all future MCP servers in this project",
                    opt(2)
                ),
                row("3", "Continue without using this MCP server", opt(3)),
            ]
        );
        let at = Instant::now();
        assert_eq!(
            key_action(
                &stack(mcp_server, at),
                false,
                CardKey::Escape,
                "",
                false,
                at
            ),
            KeyAction::Pass,
            "Esc has no button on the card"
        );
    }

    /// The 2.1.329 `/model` picker's rows as the daemon reads them.
    fn model_picker() -> ChatPrompt {
        let described = |number: u32, label: &str, description: Option<&str>| ChatOption {
            number,
            label: label.to_owned(),
            description: description.map(str::to_owned),
            selected: false,
        };
        prompt(
            "p1",
            ChatPromptKind::ModelPicker,
            vec![
                described(
                    1,
                    "Default (recommended)",
                    Some("Current · Opus 5.5 · Best for everyday, complex tasks"),
                ),
                described(2, "Opus 5.5", Some("For complex work and everyday tasks")),
                described(3, "Fable 5.2", Some("For your toughest challenges")),
                described(4, "Sonnet 5.5", Some("Most efficient for simpler tasks")),
                described(5, "Haiku 5.5", Some("Fastest for quick answers")),
                described(6, "More models", None),
            ],
        )
    }

    #[test]
    fn a_model_picker_card_offers_each_model_and_ticks_the_current_one() {
        let mut prompt = model_picker();
        assert_eq!(
            fixed(&card_buttons(&prompt, "Claude")),
            [
                row("1", "Default (recommended) ✔", opt(1)),
                row("2", "Opus 5.5", opt(2)),
                row("3", "Fable 5.2", opt(3)),
                row("4", "Sonnet 5.5", opt(4)),
                row("5", "Haiku 5.5", opt(5)),
            ]
        );
        prompt.options[1].description = Some("Current".to_owned());
        assert_eq!(
            fixed(&card_buttons(&prompt, "Claude"))[1],
            row("2", "Opus 5.5 ✔", opt(2)),
            "a current model with a bare Current description is ticked"
        );
    }

    #[test]
    fn a_model_picker_card_offers_the_effort_stepper() {
        let mut prompt = model_picker();
        prompt.adjust = Some("High effort".to_owned());
        assert_eq!(
            fixed(&adjust_buttons(&prompt)),
            [
                row(
                    "adjust-down",
                    "Less effort",
                    ChatAnswerKind::Adjust { raise: false }
                ),
                row(
                    "adjust-up",
                    "More effort",
                    ChatAnswerKind::Adjust { raise: true }
                ),
            ]
        );
    }

    #[test]
    fn a_more_models_picker_offers_no_stepper() {
        let picker = model_picker();
        assert_eq!(picker.adjust, None);
        assert_eq!(adjust_buttons(&picker), []);
        let mut confirm = prompt_with(ChatPromptKind::Confirm, &[(1, "Yes"), (2, "No")]);
        confirm.adjust = Some("High effort".to_owned());
        assert_eq!(
            adjust_buttons(&confirm),
            [],
            "only a model picker steps its row"
        );
    }

    #[test]
    fn a_model_picker_card_offers_no_more_models_button() {
        let buttons = fixed(&card_buttons(&model_picker(), "Claude"));
        assert!(
            buttons.iter().all(|(id, _, _)| id != "6"),
            "the daemon refuses More models: {buttons:?}"
        );
    }

    #[test]
    fn a_confirm_card_takes_the_screens_labels() {
        let confirm = prompt_with(
            ChatPromptKind::Confirm,
            &[(1, "Yes, switch to Opus 5.5"), (2, "No, go back")],
        );
        assert_eq!(
            fixed(&card_buttons(&confirm, "Claude")),
            [
                row("1", "Yes, switch to Opus 5.5", opt(1)),
                row("2", "No, go back", opt(2)),
            ]
        );
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
    fn a_typed_text_button_answers_only_with_text() {
        let card = prompt_with(ChatPromptKind::Question, &[(1, "Type something")]);
        assert_eq!(ButtonAnswer::TypedText.resolve(&card, " "), None);
        assert_eq!(
            ButtonAnswer::TypedText.resolve(&card, "use div"),
            Some(ChatAnswerKind::TypeSomething {
                text: "use div".to_owned()
            })
        );
        assert_eq!(
            ButtonAnswer::Fixed(ChatAnswerKind::Back).resolve(&card, ""),
            Some(ChatAnswerKind::Back)
        );
    }

    #[test]
    fn a_permission_box_allows_when_amendable_and_always_denies_with_text() {
        use super::{box_buttons, box_placeholder};
        let card = permission("p1");
        assert_eq!(
            box_placeholder(&card, "Claude").as_deref(),
            Some("Tell Claude what to do instead")
        );
        let ids = |prompt: &ChatPrompt| -> Vec<String> {
            box_buttons(prompt)
                .into_iter()
                .map(|button| format!("{}:{}", button.id, button.label))
                .collect()
        };
        assert_eq!(
            ids(&card),
            ["allow-text:Allow with text", "deny-text:Deny with text"]
        );
        assert_eq!(ButtonAnswer::DenyWithText.resolve(&card, " "), None);
        assert_eq!(
            ButtonAnswer::AllowWithText.resolve(&card, "careful"),
            Some(ChatAnswerKind::AllowWithText {
                text: "careful".to_owned()
            })
        );
        let mut fixed = card.clone();
        fixed.can_amend = false;
        assert_eq!(ids(&fixed), ["deny-text:Deny with text"]);
        assert_eq!(
            ButtonAnswer::DenyWithText.resolve(&fixed, "no"),
            Some(ChatAnswerKind::DenyWithText {
                text: "no".to_owned()
            })
        );
        let question = prompt_with(ChatPromptKind::Question, &[(1, "a")]);
        assert_eq!(box_buttons(&question), []);
    }

    #[test]
    fn a_card_with_an_option_that_takes_text_has_a_box_named_after_it() {
        use super::{box_placeholder, typed_option};
        let question = prompt(
            "q1",
            ChatPromptKind::Question,
            vec![option(1, "sub"), option(2, "Type something")],
        );
        assert_eq!(
            box_placeholder(&question, "Claude").as_deref(),
            Some("Type something")
        );
        assert_eq!(typed_option(&question), Some(2));
        let plan = prompt_with(ChatPromptKind::Plan, &[(1, "a"), (2, "b"), (3, "c")]);
        assert_eq!(
            box_placeholder(&plan, "Claude").as_deref(),
            Some("Tell Claude what to change")
        );
        assert_eq!(typed_option(&plan), Some(3));
        let review = prompt_with(ChatPromptKind::Review, &[(1, "a"), (2, "b")]);
        assert_eq!(box_placeholder(&review, "Claude"), None);
        assert_eq!(typed_option(&review), None);
    }

    #[test]
    fn a_box_belongs_to_its_card_and_gets_refused_text_back() {
        use super::{CardBoxes, GiveBack};
        let mut boxes = CardBoxes::default();
        assert!(boxes.show(Some("q1")), "a first card starts a box");
        assert!(!boxes.show(Some("q1")), "the same card keeps it");
        boxes.took("r1".to_owned(), "q1", "mul".to_owned());
        boxes.took("r2".to_owned(), "q1", "div".to_owned());
        assert_eq!(boxes.refused("r9"), None, "not an answer that took text");
        assert_eq!(
            boxes.refused("r1"),
            Some(("mul".to_owned(), GiveBack::CardBox))
        );
        assert_eq!(boxes.refused("r1"), None, "a refusal is taken once");
        assert!(boxes.show(Some("q2")), "a new card starts afresh");
        assert_eq!(
            boxes.refused("r2"),
            Some(("div".to_owned(), GiveBack::Composer)),
            "its card gone, the text goes to the composer"
        );
        assert!(boxes.show(None));
        assert!(!boxes.show(None));
    }

    #[test]
    fn refused_box_text_goes_back_before_what_was_typed_since() {
        assert_eq!(super::restored_box_text("mul", ""), "mul");
        assert_eq!(super::restored_box_text("mul", " "), "mul");
        assert_eq!(super::restored_box_text("mul", "more"), "mul more");
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

    #[test]
    fn a_selected_options_description_takes_the_filled_buttons_text_colour() {
        for (id, _, theme) in crate::builtin_themes::BUILT_INS {
            assert_eq!(
                option_description_color(true, theme),
                theme.on_accent,
                "{id}: a selected option's description takes the filled button's text colour"
            );
            let ratio =
                crate::contrast::ratio(option_description_color(true, theme), theme.primary);
            assert!(
                ratio >= 4.5,
                "{id}: a selected option's description is {ratio:.2}:1 on the primary fill"
            );
        }
    }

    #[test]
    fn an_unselected_options_description_stays_secondary_text() {
        for (id, _, theme) in crate::builtin_themes::BUILT_INS {
            assert_eq!(
                option_description_color(false, theme),
                theme.text_2,
                "{id}: an unselected option's description stays the secondary text colour"
            );
        }
    }

    #[test]
    fn a_read_only_tray_option_draws_no_pressed_state() {
        let option = AskOption {
            label: "main".to_owned(),
            description: Some("the default branch".to_owned()),
        };
        let row = tray_option_row(
            0,
            0,
            "phone-tray-option-1-1",
            &option,
            "phone-tray-option-desc-1-1".to_owned(),
            44.0,
        );
        assert!(
            !row.enabled,
            "a tray option takes no press: no pressed fill, no pointer cursor"
        );
        assert!(row.read_only, "and draws at full strength, not dimmed");
        assert!(!row.selected, "no tray option is a chosen one");
        assert_eq!(row.label, "main");
        assert_eq!(row.description, Some("the default branch"));
        assert_eq!(
            row.description_selector.as_deref(),
            Some("phone-tray-option-desc-1-1")
        );
        assert_eq!(row.min_height, Some(44.0), "the phone's 44 pt target");
    }

    #[test]
    fn a_read_only_outlined_look_takes_no_press_at_full_strength() {
        use crate::buttons::{
            ButtonKind, ButtonLook, ButtonSize, Layer, button_look, read_only_outlined_look,
        };
        use crate::ui_theme::UiTheme;
        let t = &UiTheme::CONTRAST;
        let look = read_only_outlined_look(0, 0, ButtonSize::Compact, Layer::Base, t);
        assert!(!look.enabled, "takes no press");
        assert_eq!(look.hover_fill, None, "no hover fill");
        assert_eq!(look.pressed_fill, None, "no pressed fill");
        assert!(
            (look.opacity - 1.0).abs() < f32::EPSILON,
            "drawn at full strength, not the disabled dim"
        );
        let disabled = button_look(
            0,
            0,
            ButtonKind::Outlined,
            ButtonSize::Compact,
            false,
            Layer::Base,
            t,
        );
        assert_eq!(
            ButtonLook {
                opacity: 0.0,
                ..look
            },
            ButtonLook {
                opacity: 0.0,
                ..disabled
            },
            "a disabled outlined button in every other field"
        );
    }
}
