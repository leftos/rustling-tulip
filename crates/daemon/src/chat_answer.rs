//! Answering a chat prompt: `ChatAnswer`'s runner, spawned from
//! `server/chat.rs`. It types the recipe the session's agent plugin builds
//! for the answer ([`crate::screen::dialogs::recipe_for`]) into the
//! terminal under the session's typing lane, one group of keys at a time,
//! each group only once the screen reads as the step before expects.
//!
//! An answer is not an owner key: it bumps no owner epoch. Once the screen
//! shows the answered prompt gone it clears the record's pending prompt the
//! way an owner key does, but only a pending prompt of the kind it answered.
//! "No, and tell the agent this" denies with the recipe, then sends the text
//! as a chat send does, under the same turn.

use std::time::Duration;

use agent_api::{AnswerRecipe, AnswerRefusal, RecipeStep, ScreenExpect};
use protocol::error_codes::{CHAT_REFUSED, INVALID_REQUEST};
use protocol::{
    ChatAnswerKind, ChatPrompt, ChatPromptKind, ChatScreen, ComposerState, PendingInput,
};
use tokio::time::{Instant, timeout};
use tracing::debug;

use crate::inject::{
    NO_RESPONSE, OWNER_TYPED, TypeRefusal, TypeTarget, check_typed_text, return_to_main,
    screen_reader, take_lane, type_message_under,
};
use crate::screen::dialogs::{expect_holds, prompt_identity, recipe_for};
use crate::sync::lock;
use crate::typing_lane::{Holder, TypingTurn};
use crate::view_drive::{KEY_WAIT, Reading, ScreenReader};

/// The longest an answer types for, from taking the lane to its last
/// reading.
const ANSWER_CEILING: Duration = Duration::from_secs(30);

const PROMPT_GONE: &str = "the prompt is no longer up";
const NOT_TAKEN: &str = "the terminal did not take the answer";
const DENIED_NOT_SENT: &str = "the denial was typed, but the message was not sent";
const DENY_IN_AGENT_VIEW: &str = "a denial with a message works only in the main conversation";

/// Answer the prompt `prompt_id` on `target`'s screen with `kind`.
///
/// # Errors
///
/// The lane is held by another writer ([`protocol::error_codes::BUSY`]);
/// the text is empty or holds a control character ([`INVALID_REQUEST`]); or
/// the prompt is no longer up, the plugin has no recipe for the answer, the
/// screen did not move as a step expects, the owner typed, or the terminal
/// did not answer within [`ANSWER_CEILING`] ([`CHAT_REFUSED`]).
pub async fn answer(
    target: &TypeTarget,
    prompt_id: &str,
    kind: &ChatAnswerKind,
) -> Result<(), TypeRefusal> {
    answer_within(target, prompt_id, kind, ANSWER_CEILING).await
}

/// [`answer`], with `ceiling` in place of [`ANSWER_CEILING`].
pub(crate) async fn answer_within(
    target: &TypeTarget,
    prompt_id: &str,
    kind: &ChatAnswerKind,
    ceiling: Duration,
) -> Result<(), TypeRefusal> {
    let mut progress = Progress::default();
    match timeout(ceiling, run(target, prompt_id, kind, &mut progress)).await {
        Ok(answered) => answered,
        Err(_) => Err(progress.expired()),
    }
}

/// How far an answer got: what its ceiling's refusal says.
#[derive(Default)]
struct Progress {
    /// The answer denies, then sends a message.
    deny: bool,
    /// A reading showed the answered prompt gone.
    gone: bool,
    /// The message's paste is typed.
    pasted: bool,
}

impl Progress {
    /// The refusal for an answer whose ceiling passed here.
    fn expired(&self) -> TypeRefusal {
        if self.pasted {
            TypeRefusal::left_in_box(NO_RESPONSE)
        } else if self.deny && self.gone {
            TypeRefusal::refused(DENIED_NOT_SENT)
        } else {
            TypeRefusal::refused(NO_RESPONSE)
        }
    }
}

/// The answer's steps within its ceiling, recording in `progress` how far
/// it got.
async fn run(
    target: &TypeTarget,
    prompt_id: &str,
    kind: &ChatAnswerKind,
    progress: &mut Progress,
) -> Result<(), TypeRefusal> {
    let turn = take_lane(target, Holder::ChatAnswer).await?;
    let mut reader = screen_reader(target);
    let identity = mirror_identity(target, prompt_id)?;
    let reading = reader.current().await;
    let Some(shown) = shown_prompt(&reading)
        .filter(|prompt| prompt_identity(prompt) == identity)
        .cloned()
    else {
        return Err(TypeRefusal::refused(PROMPT_GONE));
    };
    let returns = in_agent_view_without_dialog(&reading);
    if !returns && in_agent_view(&reading) && matches!(kind, ChatAnswerKind::DenyWithText { .. }) {
        return Err(TypeRefusal::refused(DENY_IN_AGENT_VIEW));
    }
    let kind = checked(kind)?;
    let recipe = recipe_on(target, &shown, &kind)?;
    let (reading, shown) = if returns {
        let reading = return_to_main(target, &turn, &mut reader, reading).await?;
        let shown = still_shown(target, &shown, &reading, &kind, &recipe)?;
        (reading, shown)
    } else {
        (reading, shown)
    };
    let deny_text = match &kind {
        ChatAnswerKind::DenyWithText { text } => Some(text.as_str()),
        _ => None,
    };
    progress.deny = deny_text.is_some();
    let mut answerer = Answerer {
        target,
        turn: &turn,
        reader: &mut reader,
        answered: Answered {
            identity: prompt_identity(&shown),
            kind: shown.kind,
            pending: pending_input(target),
        },
        cleared: false,
        progress: &mut *progress,
    };
    answerer.type_recipe(&recipe, reading.chat).await?;
    if let Some(text) = deny_text {
        type_message_under(target, &turn, &mut reader, text, &mut progress.pasted).await?;
        // The next writer starts from the frame the Enter brought.
        reader.after_key().await;
    }
    Ok(())
}

/// The identity of the prompt the mirror shows as `prompt_id`.
fn mirror_identity(target: &TypeTarget, prompt_id: &str) -> Result<u64, TypeRefusal> {
    match target.mirror.current_prompt(&target.session_id) {
        Some((id, identity)) if id == prompt_id => Ok(identity),
        _ => Err(TypeRefusal::refused(PROMPT_GONE)),
    }
}

/// The prompt `reading` shows.
fn shown_prompt(reading: &Reading) -> Option<&ChatPrompt> {
    reading.chat.as_ref().and_then(|chat| chat.prompt.as_ref())
}

/// What the session's record says it waits for; `None` when nothing is
/// pending or the record is gone.
fn pending_input(target: &TypeTarget) -> Option<PendingInput> {
    let record = target.sessions.get(&target.session_id)?;
    let record = lock(&record);
    record.pending_input.clone()
}

/// Whether `reading` shows an agent's view.
fn in_agent_view(reading: &Reading) -> bool {
    reading
        .view
        .as_ref()
        .is_some_and(|view| view.agent_in_view.is_some())
}

/// Whether `reading` shows an agent's view with no dialog up, which in
/// practice is a Heads up notice for main: the keys must go to main first.
/// A dialog in an agent's view takes its keys where it is.
fn in_agent_view_without_dialog(reading: &Reading) -> bool {
    in_agent_view(reading) && reading.view.as_ref().is_some_and(|view| !view.dialog)
}

/// `kind` with its text as a chat send types it ([`check_typed_text`]).
fn checked(kind: &ChatAnswerKind) -> Result<ChatAnswerKind, TypeRefusal> {
    let mut kind = kind.clone();
    if let ChatAnswerKind::TypeSomething { text }
    | ChatAnswerKind::AllowWithText { text }
    | ChatAnswerKind::DenyWithText { text } = &mut kind
    {
        *text = check_typed_text(text).map_err(|why| TypeRefusal {
            code: INVALID_REQUEST,
            message: why.to_owned(),
        })?;
    }
    Ok(kind)
}

/// The plugin's recipe for `kind` on `prompt`.
fn recipe_on(
    target: &TypeTarget,
    prompt: &ChatPrompt,
    kind: &ChatAnswerKind,
) -> Result<AnswerRecipe, TypeRefusal> {
    recipe_for(&target.agent, prompt, kind).map_err(|refusal| {
        debug!(session_id = %target.session_id, ?refusal, ?kind, "chat answer: no recipe");
        recipe_refusal(refusal)
    })
}

/// The prompt main shows in `reading`, after the return, when it is still
/// the prompt `shown` checked in the agent's view: of the same kind, with
/// the same option labels in order, and giving `kind` the same `recipe`.
fn still_shown(
    target: &TypeTarget,
    shown: &ChatPrompt,
    reading: &Reading,
    kind: &ChatAnswerKind,
    recipe: &AnswerRecipe,
) -> Result<ChatPrompt, TypeRefusal> {
    let labels = |prompt: &ChatPrompt| {
        prompt
            .options
            .iter()
            .map(|option| option.label.clone())
            .collect::<Vec<_>>()
    };
    let same = shown_prompt(reading).filter(|main| {
        main.kind == shown.kind
            && labels(main) == labels(shown)
            && recipe_for(&target.agent, main, kind).as_ref() == Ok(recipe)
    });
    let Some(main) = same else {
        debug!(session_id = %target.session_id, "chat answer: main shows another prompt after the return");
        return Err(TypeRefusal::refused(PROMPT_GONE));
    };
    Ok(main.clone())
}

/// The `Error` that answers a plugin's refusal to build a recipe.
fn recipe_refusal(refusal: AnswerRefusal) -> TypeRefusal {
    let (code, message) = match refusal {
        AnswerRefusal::TextEmpty => (INVALID_REQUEST, "the answer is empty".to_owned()),
        AnswerRefusal::ControlCharacter => (
            INVALID_REQUEST,
            "the answer holds a control character".to_owned(),
        ),
        AnswerRefusal::Unsupported => (
            CHAT_REFUSED,
            "this agent's prompts cannot be answered from the chat".to_owned(),
        ),
        AnswerRefusal::UnknownOption(number) => {
            (CHAT_REFUSED, format!("the prompt has no option {number}"))
        }
        AnswerRefusal::NotThisDialog => (
            CHAT_REFUSED,
            "that answer does not fit the prompt on screen".to_owned(),
        ),
        AnswerRefusal::TooManyOptions => (
            CHAT_REFUSED,
            "the option takes more than one key".to_owned(),
        ),
    };
    TypeRefusal { code, message }
}

/// The chat screen `reading` shows, when the plugin could read one: `None`
/// with no chat reading, or with one that recognised nothing on the screen
/// (no box and no prompt).
fn readable(reading: Reading) -> Option<ChatScreen> {
    reading
        .chat
        .filter(|chat| chat.prompt.is_some() || chat.composer != ComposerState::Unknown)
}

/// Whether answering a prompt of `kind` answers the record's pending prompt
/// `pending`: a permission dialog a permission prompt, a question or its
/// review step a question, a plan its approval. A Heads up or a trust
/// dialog answers none.
fn answers(kind: &ChatPromptKind, pending: &PendingInput) -> bool {
    matches!(
        (kind, pending),
        (ChatPromptKind::Permission, PendingInput::Permission { .. })
            | (
                ChatPromptKind::Question | ChatPromptKind::Review,
                PendingInput::Question { .. }
            )
            | (ChatPromptKind::Plan, PendingInput::PlanApproval { .. })
    )
}

/// Whether `now`, typed on `before`, holds `step`'s expectation; `gone` is
/// whether it shows the answered prompt gone.
fn step_holds(
    step: &RecipeStep,
    before: Option<&ChatScreen>,
    now: &ChatScreen,
    gone: bool,
) -> bool {
    match &step.expect {
        ScreenExpect::DialogGone => gone,
        // A repaint of the dialog as it was is not the step taken.
        ScreenExpect::SameDialog => {
            before != Some(now) && expect_holds(&step.expect, before, Some(now))
        }
        expect => expect_holds(expect, before, Some(now)),
    }
}

/// What a step's reading must show beyond the step's own expectation.
#[derive(Clone, Copy)]
enum Also<'a> {
    Nothing,
    /// A change from this reading, the one before the answer's first key:
    /// the last step of a recipe whose last expectation is no check.
    Moved(Option<&'a ChatScreen>),
    /// The box back and taking text: the denial before its message.
    BoxReady,
}

impl Also<'_> {
    /// Whether `now`, typed on `before`, shows what this asks for.
    fn holds(self, before: Option<&ChatScreen>, now: &ChatScreen) -> bool {
        match self {
            Self::Nothing => true,
            Self::Moved(first) => first != Some(now),
            Self::BoxReady => expect_holds(&ScreenExpect::ComposerReady, before, Some(now)),
        }
    }
}

/// The prompt an answer's keys are typed on.
struct Answered {
    identity: u64,
    kind: ChatPromptKind,
    /// The record's pending input just before the first key.
    pending: Option<PendingInput>,
}

/// An answer typing its recipe under its turn of the lane.
struct Answerer<'a> {
    target: &'a TypeTarget,
    turn: &'a TypingTurn,
    reader: &'a mut ScreenReader,
    answered: Answered,
    /// Whether the prompt has been seen gone and the clear run.
    cleared: bool,
    progress: &'a mut Progress,
}

impl Answerer<'_> {
    /// Type `recipe`'s steps on the screen read as `first`; for a denial
    /// with a message, its last step must also bring the box back.
    async fn type_recipe(
        &mut self,
        recipe: &AnswerRecipe,
        first: Option<ChatScreen>,
    ) -> Result<(), TypeRefusal> {
        let mut before = first.clone();
        for (at, step) in recipe.steps.iter().enumerate() {
            let last = at + 1 == recipe.steps.len();
            let also = match (last, self.progress.deny, &step.expect) {
                (true, true, _) => Also::BoxReady,
                (true, false, ScreenExpect::Any) => Also::Moved(first.as_ref()),
                _ => Also::Nothing,
            };
            if self.turn.owner_typed_since() {
                return Err(TypeRefusal::refused(OWNER_TYPED));
            }
            self.reader.arm();
            for key in &step.keys {
                self.turn.press(&self.target.pty, key.to_bytes());
            }
            before = Some(self.settle_on(step, before.as_ref(), also).await?);
        }
        Ok(())
    }

    /// Wait up to [`KEY_WAIT`] for a readable settled reading that holds
    /// `step`'s expectation, typed on `before`, and `also`; the screen it
    /// shows.
    async fn settle_on(
        &mut self,
        step: &RecipeStep,
        before: Option<&ChatScreen>,
        also: Also<'_>,
    ) -> Result<ChatScreen, TypeRefusal> {
        let deadline = Instant::now() + KEY_WAIT;
        let box_ready = matches!(also, Also::BoxReady);
        while let Some(reading) = self.reader.next_before(deadline).await {
            let Some(now) = readable(reading) else {
                continue;
            };
            let gone = self.gone(&now);
            if gone {
                self.progress.gone = true;
                self.clear_once();
            }
            if box_ready && gone && now.prompt.is_some() {
                debug!(session_id = %self.target.session_id, "chat answer: another prompt came up after the denial");
                return Err(TypeRefusal::refused(DENIED_NOT_SENT));
            }
            if step_holds(step, before, &now, gone) && also.holds(before, &now) {
                return Ok(now);
            }
        }
        debug!(session_id = %self.target.session_id, ?step, "chat answer: the screen did not move as expected");
        let why = if box_ready && self.progress.gone {
            DENIED_NOT_SENT
        } else {
            NOT_TAKEN
        };
        Err(TypeRefusal::refused(why))
    }

    /// Whether `now` shows the answered prompt gone: no prompt and no
    /// dialog, or a prompt that is another one.
    fn gone(&self, now: &ChatScreen) -> bool {
        match &now.prompt {
            Some(prompt) => prompt_identity(prompt) != self.answered.identity,
            None => now.composer != ComposerState::Dialog,
        }
    }

    /// The first time the prompt is seen gone, clear the record's pending
    /// prompt as an owner key would, when it is of the answered prompt's
    /// kind and is still the one pending before the first key, or came while
    /// none was.
    fn clear_once(&mut self) {
        if self.cleared {
            return;
        }
        self.cleared = true;
        let kind = self.answered.kind.clone();
        let captured = self.answered.pending.take();
        self.target
            .sessions
            .clear_answered_prompt_if(&self.target.session_id, |record| {
                record.pending_input.as_ref().is_some_and(|pending| {
                    answers(&kind, pending)
                        && (captured.is_none() || record.pending_input == captured)
                })
            });
    }
}
