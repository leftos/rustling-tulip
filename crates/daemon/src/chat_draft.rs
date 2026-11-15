//! Pulling the draft in a session's terminal prompt box into the chat:
//! `ChatPullDraft`'s runner, spawned from `server/chat.rs`. Under the
//! session's typing lane it reads the whole draft and clears the box with the
//! line clears the agent's plugin names
//! ([`crate::screen::dialogs::clear_recipe_for`]), one press at a time, each
//! checked under the record's and the screen's locks as a chat send's Enter
//! is ([`crate::inject`]).
//!
//! The draft is read under the same screen lock as the first press. Once a
//! press was typed the pull always answers with the text it read, whether
//! or not the box was left empty; before one it answers a refusal. A press
//! that brings no output ends the clear: the box keeps what is left. The
//! draft's text leaves the daemon only in that answer, never in a log line
//! or a loader's message. A pull may run while the session's first prompt is
//! held: a line clear never reaches the agent, and an emptied box lets the
//! prompt be typed.

use std::time::Duration;

use agent_api::{AnswerRecipe, RecipeStep, ScreenExpect};
use protocol::DaemonMessage;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};
use tracing::debug;

use crate::inject::{
    NO_RESPONSE, OWNER_TYPED, TypeRefusal, TypeTarget, box_refusal, draft_gate, ended_refusal,
    screen_reader, take_lane, waiting_refusal,
};
use crate::screen::Screen;
use crate::screen::dialogs::{self, expect_holds};
use crate::session::SessionRecord;
use crate::sync::lock;
use crate::typing_lane::{Holder, TypingTurn};
use crate::view_drive::{KEY_WAIT, Reading, ScreenReader};

/// The longest a pull runs, from taking the lane to its last reading.
const PULL_CEILING: Duration = Duration::from_secs(30);

pub(crate) const PASTED: &str =
    "the draft holds pasted text or an image; send it as it stands or clear it in the terminal";
pub(crate) const SCROLLED: &str =
    "the draft is taller than the box shows; send it as it stands or clear it in the terminal";
pub(crate) const UNREADABLE: &str = "the terminal's draft cannot be read";
pub(crate) const CANNOT_CLEAR: &str = "this agent's box cannot be cleared from the chat";

/// The draft a pull read, and whether the box last read empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pulled {
    /// The draft's text as the box held it.
    pub text: String,
    /// The box read empty after the last press.
    pub cleared: bool,
}

/// How far a pull got: the text it read with its first press, and the
/// presses left.
#[derive(Default)]
struct Progress {
    text: Option<String>,
    steps: std::vec::IntoIter<RecipeStep>,
}

impl Progress {
    /// The pull's answer once it stops: the text read, with `cleared`.
    ///
    /// # Errors
    ///
    /// No press was typed, so no text was read ([`NO_RESPONSE`]).
    fn pulled(&mut self, cleared: bool) -> Result<Pulled, TypeRefusal> {
        let text = self
            .text
            .take()
            .ok_or_else(|| TypeRefusal::refused(NO_RESPONSE))?;
        Ok(Pulled { text, cleared })
    }
}

/// What one pass of the clear did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// A press was typed.
    Pressed,
    /// The clear ends here.
    Stop,
}

/// Pull the draft in `target`'s main box for `requester`, the connection
/// that asked: read it, clear the box, and answer the text.
///
/// # Errors
///
/// Before any press: the lane is held by another writer
/// ([`protocol::error_codes::BUSY`]), or the agent ended, a prompt is
/// waiting, the box holds no draft a key may reach, the draft holds a paste
/// or scrolls, it cannot be read or cleared, the owner typed, or the terminal
/// did not answer within [`PULL_CEILING`]
/// ([`protocol::error_codes::CHAT_REFUSED`]).
pub async fn pull(
    target: &TypeTarget,
    requester: &mpsc::UnboundedSender<DaemonMessage>,
) -> Result<Pulled, TypeRefusal> {
    pull_within(target, requester, PULL_CEILING).await
}

/// [`pull`], with `ceiling` in place of [`PULL_CEILING`].
pub(crate) async fn pull_within(
    target: &TypeTarget,
    requester: &mpsc::UnboundedSender<DaemonMessage>,
    ceiling: Duration,
) -> Result<Pulled, TypeRefusal> {
    let turn = take_lane(target, Holder::ChatPull).await?;
    let mut reader = screen_reader(target);
    let mut progress = Progress::default();
    let cleared = timeout(
        ceiling,
        clear(target, &turn, &mut reader, requester, &mut progress),
    )
    .await;
    match cleared {
        Ok(pulled) => pulled,
        Err(_) => progress.pulled(false),
    }
}

/// The pull's steps within its ceiling: the gate on a settled reading, then
/// one press per pass until the box reads empty, a check stops it, the
/// presses run out, or a press brings no output.
async fn clear(
    target: &TypeTarget,
    turn: &TypingTurn,
    reader: &mut ScreenReader,
    requester: &mpsc::UnboundedSender<DaemonMessage>,
    progress: &mut Progress,
) -> Result<Pulled, TypeRefusal> {
    draft_gate(target, &reader.current().await)?;
    loop {
        if press_clear(target, turn, reader, requester, progress)? == Step::Stop {
            break;
        }
        match reader.next_before(Instant::now() + KEY_WAIT).await {
            Some(reading) if box_empty(&reading) => return progress.pulled(true),
            Some(_) => {}
            None => {
                debug!(session_id = %target.session_id, "chat pull: a press brought no output");
                break;
            }
        }
    }
    progress.pulled(box_empty(&reader.read()))
}

/// One press of the clear under the record's and the screen's locks, once
/// the screen still shows a draft in main's box with no dialog and no owner
/// key, waiting prompt or ended agent came, and the requester still listens.
/// The first pass reads the draft and its recipe under the same screen lock;
/// a failed check there refuses, and on a later pass ends the clear.
fn press_clear(
    target: &TypeTarget,
    turn: &TypingTurn,
    reader: &mut ScreenReader,
    requester: &mpsc::UnboundedSender<DaemonMessage>,
    progress: &mut Progress,
) -> Result<Step, TypeRefusal> {
    let output = reader.subscribe();
    // The lock order is record, screen. Both are held from the check to the
    // key, so no prompt or frame comes between them.
    let Some(record) = target.sessions.get(&target.session_id) else {
        return Ok(Step::Stop);
    };
    let record = lock(&record);
    let screen = lock(&target.screen);
    let reading = Reading::of(&target.agent, &screen);
    let first = progress.text.is_none();
    if let Some(refusal) = press_refusal(target, turn, &record, &reading, requester) {
        if first {
            return Err(refusal);
        }
        debug!(session_id = %target.session_id, reason = %refusal.message, "chat pull: stopped");
        return Ok(Step::Stop);
    }
    if first {
        let (text, recipe) = first_draft(target, &screen)?;
        progress.text = Some(text);
        progress.steps = recipe.steps.into_iter();
    }
    let Some(step) = progress.steps.next() else {
        return Ok(Step::Stop);
    };
    reader.arm_at(output, screen.fed());
    for key in &step.keys {
        turn.press(&target.pty, key.to_bytes());
    }
    Ok(Step::Pressed)
}

/// Why no press may be typed now: the owner typed since the lane was taken,
/// the agent ended, a prompt is waiting, the box holds no draft a key may
/// reach, or the requester's connection is gone.
fn press_refusal(
    target: &TypeTarget,
    turn: &TypingTurn,
    record: &SessionRecord,
    reading: &Reading,
    requester: &mpsc::UnboundedSender<DaemonMessage>,
) -> Option<TypeRefusal> {
    if turn.owner_typed_since() {
        return Some(TypeRefusal::refused(OWNER_TYPED));
    }
    ended_refusal(record)
        .or_else(|| waiting_refusal(target, record))
        .or_else(|| box_refusal(target, reading))
        .or_else(|| {
            requester
                .is_closed()
                .then(|| TypeRefusal::refused(NO_RESPONSE))
        })
}

/// The draft `screen`'s box holds and the recipe that clears it.
///
/// # Errors
///
/// The draft cannot be read, holds a paste or an image, scrolls, or the
/// agent's plugin has no recipe that clears it
/// ([`protocol::error_codes::CHAT_REFUSED`]).
fn first_draft(
    target: &TypeTarget,
    screen: &Screen,
) -> Result<(String, AnswerRecipe), TypeRefusal> {
    let Some(draft) = dialogs::read_draft(&target.agent, screen) else {
        return Err(TypeRefusal::refused(UNREADABLE));
    };
    if draft.pasted {
        return Err(TypeRefusal::refused(PASTED));
    }
    if !draft.whole {
        return Err(TypeRefusal::refused(SCROLLED));
    }
    let recipe = dialogs::clear_recipe_for(&target.agent, &draft)
        .map_err(|_| TypeRefusal::refused(CANNOT_CLEAR))?;
    Ok((draft.text, recipe))
}

/// Whether `reading`'s box reads empty: ready, working or showing only the
/// agent's suggestion.
fn box_empty(reading: &Reading) -> bool {
    expect_holds(&ScreenExpect::ComposerReady, None, reading.chat.as_ref())
}
