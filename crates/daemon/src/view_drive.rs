//! The status watcher's hand on a session's terminal view: it returns the
//! terminal to the main conversation when a prompt for the main agent comes
//! up while an agent's view is open, keeps a draft typed for that agent in
//! the prompt stash meanwhile, and puts the draft back once the prompt is
//! answered.
//!
//! [`ViewDrive`] bounds one run of keys towards a [`ViewGoal`]: the agent's
//! plugin names each key ([`crate::screen::dialogs::next_view_key`]), and the
//! drive gives up after too many keys, too many Downs off the panel in a
//! row, or too long. [`ViewReturn`] decides when to drive and what to do with
//! the draft, from each settled reading the watcher hands it. Neither types
//! anything: they answer with the keys the watcher writes.
//!
//! [`ScreenReader`] is the reading a writer outside the watcher takes after
//! each of its own keys (a chat send): the screen once the output the key
//! brought has been drawn and has paused.

use std::sync::Arc;
use std::time::Duration;

use agent_api::{Key, KeyHolder, ViewGoal, ViewReading, ViewRefusal, ViewStep};
use protocol::{AgentId, ChatPromptKind, ChatScreen, ComposerState, SessionNoticeKind};
use tokio::sync::broadcast;
use tokio::time::Instant;

use crate::pty::PtyHandle;
use crate::screen::{Screen, SharedScreen, dialogs};
use crate::sync::lock;

/// The most Downs in a row a drive presses while the panel does not hold
/// the keys.
pub const MAX_DOWNS: u32 = 3;
/// The most keys one drive presses.
pub const MAX_KEYS: u32 = 12;
/// The longest one drive runs.
pub const DRIVE_LIMIT: Duration = Duration::from_secs(10);
/// How long after a key the watcher reads the screen again when no output
/// came.
pub const KEY_WAIT: Duration = Duration::from_secs(3);

/// How the draft Heads up's "Chat in main session" puts in the box begins.
const NOTE_PREFIX: &str = "Here is a note offered by a side agent:";

/// Why a drive stopped short of its goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveFailure {
    /// The plugin has no key towards the goal from the reading.
    Refused(ViewRefusal),
    /// More than [`MAX_DOWNS`] Downs in a row off the panel.
    TooManyDowns,
    /// More than [`MAX_KEYS`] keys.
    TooManyKeys,
    /// Longer than [`DRIVE_LIMIT`].
    TimedOut,
}

/// What a drive does with one reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveAction {
    /// Press this key, then read the screen again.
    Press(Key),
    /// The goal's view is up.
    Reached,
    /// The drive stops here.
    Failed(DriveFailure),
}

/// One run of keys towards a view, and the bounds it keeps.
#[derive(Debug, Clone)]
pub struct ViewDrive {
    goal: ViewGoal,
    keys: u32,
    downs_in_a_row: u32,
    started: Instant,
    last_key_at: Option<Instant>,
}

impl ViewDrive {
    /// A drive towards `goal` starting at `now`.
    #[must_use]
    pub fn new(goal: ViewGoal, now: Instant) -> Self {
        Self {
            goal,
            keys: 0,
            downs_in_a_row: 0,
            started: now,
            last_key_at: None,
        }
    }

    #[must_use]
    pub fn goal(&self) -> ViewGoal {
        self.goal
    }

    /// When the drive last pressed a key.
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the watcher times its re-read from its own key time"
        )
    )]
    pub fn last_key_at(&self) -> Option<Instant> {
        self.last_key_at
    }

    /// What to do with `reading`, for which the plugin's next key towards
    /// the goal is `step`. A goal reached or refused ends the drive whatever
    /// the time; a key is pressed only within the drive's bounds.
    pub fn step(&mut self, reading: &ViewReading, step: ViewStep, now: Instant) -> DriveAction {
        let key = match step {
            ViewStep::Done => return DriveAction::Reached,
            ViewStep::Refuse(refusal) => {
                return DriveAction::Failed(DriveFailure::Refused(refusal));
            }
            ViewStep::Press(key) => key,
        };
        if now.duration_since(self.started) > DRIVE_LIMIT {
            return DriveAction::Failed(DriveFailure::TimedOut);
        }
        if key == Key::Down && reading.key_holder != KeyHolder::Panel {
            self.downs_in_a_row += 1;
            if self.downs_in_a_row > MAX_DOWNS {
                return DriveAction::Failed(DriveFailure::TooManyDowns);
            }
        } else {
            self.downs_in_a_row = 0;
        }
        self.keys += 1;
        if self.keys > MAX_KEYS {
            return DriveAction::Failed(DriveFailure::TooManyKeys);
        }
        self.last_key_at = Some(now);
        DriveAction::Press(key)
    }
}

/// How long a [`ScreenReader`] waits for the output to pause before it reads
/// the screen: the status watcher's own debounce.
pub const READ_QUIET: Duration = Duration::from_millis(40);

/// How often a [`ScreenReader`] looks again while the grid has not yet been
/// fed the output the reader saw.
const FED_POLL: Duration = Duration::from_millis(5);

/// One look at a session's screen: which conversation it shows and where its
/// keys go, and what its input box holds; `None` where the agent's plugin
/// gives no such reading.
#[derive(Debug, Clone)]
pub struct Reading {
    pub view: Option<ViewReading>,
    pub chat: Option<ChatScreen>,
}

impl Reading {
    /// `screen` as `agent`'s plugin reads it.
    #[must_use]
    pub fn of(agent: &AgentId, screen: &Screen) -> Self {
        Self {
            view: dialogs::read_view(agent, screen),
            chat: dialogs::read(agent, screen),
        }
    }
}

/// Reads a session's screen for a writer that types into it: after a key,
/// once the session's grid has been fed every byte of output the reader
/// received since it was armed and no output came for [`READ_QUIET`], or
/// once [`KEY_WAIT`] has passed. The status watcher feeds the grid from the
/// same output, so a reading never comes from a frame the watcher has not
/// drawn yet.
pub struct ScreenReader {
    pty: Arc<PtyHandle>,
    screen: SharedScreen,
    agent: AgentId,
    output: broadcast::Receiver<Vec<u8>>,
    /// The grid's fed count when the reader was last armed.
    base: u64,
    /// The bytes of output received since the reader was last armed.
    seen: u64,
}

impl ScreenReader {
    /// A reader of `screen`, the grid of `pty`'s output, as `agent`'s plugin
    /// reads it.
    #[must_use]
    pub fn new(pty: Arc<PtyHandle>, screen: SharedScreen, agent: AgentId) -> Self {
        let output = pty.output.subscribe();
        let base = lock(&screen).fed();
        Self {
            pty,
            screen,
            agent,
            output,
            base,
            seen: 0,
        }
    }

    /// Start counting the output anew; called before each write, so the
    /// output that write brings is counted.
    pub fn arm(&mut self) {
        let output = self.subscribe();
        let fed = lock(&self.screen).fed();
        self.arm_at(output, fed);
    }

    /// A new subscription to the session's output, for [`Self::arm_at`].
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.pty.output.subscribe()
    }

    /// [`Self::arm`] for a caller holding the screen's lock: `output` was
    /// subscribed before the lock was taken, and `fed` is the grid's fed
    /// count read under it. Subscribing first means no output the grid has
    /// not drawn goes unseen; output drawn between the two is counted on both
    /// sides, which only makes the reading wait.
    pub fn arm_at(&mut self, output: broadcast::Receiver<Vec<u8>>, fed: u64) {
        self.output = output;
        self.base = fed;
        self.seen = 0;
    }

    /// The screen as it stands now.
    #[must_use]
    pub fn read(&self) -> Reading {
        Reading::of(&self.agent, &lock(&self.screen))
    }

    /// The screen with no key pressed: once the output has paused and the
    /// grid holds it, at the latest after [`KEY_WAIT`].
    pub async fn current(&mut self) -> Reading {
        self.arm();
        let now = Instant::now();
        match self.settle(now + KEY_WAIT, Some(now)).await {
            Some(reading) => reading,
            None => self.read(),
        }
    }

    /// The screen after a key: once the output it brought has been drawn and
    /// has paused, or after [`KEY_WAIT`] whatever came.
    pub async fn after_key(&mut self) -> Reading {
        match self.settle(Instant::now() + KEY_WAIT, None).await {
            Some(reading) => reading,
            None => self.read(),
        }
    }

    /// The next reading once more output has been drawn and has paused;
    /// `None` when no output came before `deadline`.
    pub async fn next_before(&mut self, deadline: Instant) -> Option<Reading> {
        self.settle(deadline, None).await
    }

    /// Wait for the output to settle, `last_output` the time output last
    /// came; a reading at `deadline` once any came, else `None` then. The
    /// reading at the deadline waits, at most a further [`KEY_WAIT`], for the
    /// grid to draw what the reader received, but not for a pause.
    async fn settle(
        &mut self,
        deadline: Instant,
        mut last_output: Option<Instant>,
    ) -> Option<Reading> {
        loop {
            let now = Instant::now();
            match last_output {
                Some(_) if now >= deadline => return Some(self.read_once_drawn().await),
                Some(at) if now >= at + READ_QUIET && self.drawn() => return Some(self.read()),
                None if now >= deadline => return None,
                _ => {}
            }
            let wake = match last_output {
                Some(at) if now >= at + READ_QUIET => now + FED_POLL,
                Some(at) => at + READ_QUIET,
                None => deadline,
            };
            if let Ok(received) =
                tokio::time::timeout_at(wake.min(deadline), self.output.recv()).await
            {
                if !self.count(received) {
                    return last_output.map(|_| self.read());
                }
                last_output = Some(Instant::now());
            }
        }
    }

    /// Count one receipt of output; `false` once the output has ended.
    fn count(&mut self, received: Result<Vec<u8>, broadcast::error::RecvError>) -> bool {
        match received {
            Ok(bytes) => {
                let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                self.seen = self.seen.saturating_add(len);
                true
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                // The bytes lost are unknown: what the grid holds now counts
                // as drawn, and the pause after it decides the reading.
                tracing::debug!(skipped, "screen reader: output lagged");
                let fed = lock(&self.screen).fed();
                self.base = fed.wrapping_sub(self.seen);
                true
            }
            Err(broadcast::error::RecvError::Closed) => false,
        }
    }

    /// The screen once the grid has drawn every byte the reader received, or
    /// as it stands after [`KEY_WAIT`] more.
    async fn read_once_drawn(&self) -> Reading {
        let limit = Instant::now() + KEY_WAIT;
        while !self.drawn() && Instant::now() < limit {
            tokio::time::sleep(FED_POLL).await;
        }
        if !self.drawn() {
            tracing::debug!("screen reader: the grid never drew the output it received");
        }
        self.read()
    }

    /// Whether the grid has been fed every byte the reader received.
    fn drawn(&self) -> bool {
        lock(&self.screen).fed().wrapping_sub(self.base) >= self.seen
    }
}

/// When a saved draft goes back to its agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreAfter {
    /// Once no Heads up notice is up.
    HeadsUpGone,
    /// Once the main agent is working again.
    MainWorking,
}

/// A draft the watcher stashed before leaving an agent's view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedDraft {
    /// The agent's description, as the rule over its view showed it.
    pub agent: String,
    /// The agent's panel row.
    pub row: usize,
    pub after: RestoreAfter,
}

/// What the watcher does for a reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnAction {
    /// Write this key to the session's terminal.
    Press(Key),
    /// Tell the session's clients.
    Notice(SessionNoticeKind),
    /// Log this at info.
    Log(String),
    /// Log this at warn: something did not go as read.
    Warn(String),
}

/// What a drive is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// Back to main for a prompt.
    Return,
    /// Back to the saved draft's agent.
    Reopen,
    /// Back to main to put the saved draft there.
    RestoreInMain,
}

/// The work under way between readings.
#[derive(Debug, Clone)]
enum Task {
    /// The stash key was pressed; the next reading must show it took.
    Stashing(SavedDraft),
    Drive(ViewDrive, Purpose),
}

/// When to take the terminal back to main and what to do with the draft:
/// a state machine fed every settled reading of one session's screen.
#[derive(Debug, Default)]
pub struct ViewReturn {
    /// The identity of the last Heads up a return was made for.
    handled_heads_up: Option<u64>,
    /// An `AskUserQuestion` was seen with an agent in view.
    question_armed: bool,
    /// A return was made for the note draft in an agent's box, and the box
    /// has held it since.
    note_handled: bool,
    /// The last reading's agent in view and whether its box held text: a
    /// box emptied by something other than a send, while a draft is saved,
    /// means the user stashed or cleared it.
    last_box: Option<(Option<String>, bool)>,
    task: Option<Task>,
    saved: Option<SavedDraft>,
    /// The last reading pressed a key, and no reading has come since.
    key_pending: bool,
}

impl ViewReturn {
    /// Whether a drive is under way or a pressed key awaits its reading: the
    /// terminal is mid-switch and its keys are not free.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.task.is_some() || self.key_pending
    }

    /// Stop the work under way, keeping any saved draft: no key is owed a
    /// reading any more.
    pub fn abandon(&mut self) {
        self.task = None;
        self.key_pending = false;
    }

    /// The draft waiting to go back to its agent.
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the tests read the saved draft")
    )]
    pub fn saved(&self) -> Option<&SavedDraft> {
        self.saved.as_ref()
    }

    /// Act on one settled reading of `agent`'s screen: `view` is its view
    /// reading and `chat` its chat reading, from the same frame.
    pub fn on_reading(
        &mut self,
        agent: &AgentId,
        view: &ViewReading,
        chat: Option<&ChatScreen>,
        now: Instant,
    ) -> Vec<ReturnAction> {
        let mut out = Vec::new();
        let note = view
            .box_first_line
            .as_deref()
            .is_some_and(|line| line.starts_with(NOTE_PREFIX));
        let heads_up_shown = chat
            .and_then(|chat| chat.prompt.as_ref())
            .is_some_and(|prompt| prompt.kind == ChatPromptKind::HeadsUp);
        if !heads_up_shown {
            self.handled_heads_up = None;
        }
        if view.agent_in_view.is_none() {
            self.question_armed = false;
        }
        if !note {
            self.note_handled = false;
        }
        self.drop_if_user_emptied_box(view, chat, &mut out);
        match self.task.take() {
            Some(Task::Stashing(draft)) => self.after_stash(agent, view, draft, now, &mut out),
            Some(Task::Drive(drive, purpose)) => {
                self.drive_on(agent, view, drive, purpose, now, &mut out);
            }
            None => {
                if let Some(after) = self.trigger(view, chat, note) {
                    self.on_trigger(agent, view, after, now, &mut out);
                } else {
                    self.try_restore(agent, view, chat, now, &mut out);
                }
            }
        }
        self.last_box = Some((view.agent_in_view.clone(), view.box_first_line.is_some()));
        self.key_pending = out
            .iter()
            .any(|action| matches!(action, ReturnAction::Press(_)));
        out
    }

    /// While a draft is saved, a box that held text in the last reading of
    /// the same view and is empty now, with a stash held and the composer
    /// not working, was stashed or cleared by the user: the stash's one slot
    /// may hold their text now, so the saved draft is dropped.
    fn drop_if_user_emptied_box(
        &mut self,
        view: &ViewReading,
        chat: Option<&ChatScreen>,
        out: &mut Vec<ReturnAction>,
    ) {
        let Some((last_view, had_text)) = self.last_box.as_ref() else {
            return;
        };
        let working = chat.is_some_and(|chat| chat.composer == ComposerState::Working);
        let emptied =
            *had_text && view.box_first_line.is_none() && *last_view == view.agent_in_view;
        if !emptied || !view.stashed || working {
            return;
        }
        if let Some(saved) = self.saved.take() {
            out.push(ReturnAction::Warn(format!(
                "the box was emptied with no send while {}'s draft was in the stash; \
                 the stash may hold other text now, so the draft is not restored",
                saved.agent
            )));
        }
    }

    /// Which trigger the reading fires, if any, as the restore condition its
    /// saved draft would wait for. Nothing fires, and nothing is marked
    /// handled, while a dialog is up.
    fn trigger(
        &mut self,
        view: &ViewReading,
        chat: Option<&ChatScreen>,
        note: bool,
    ) -> Option<RestoreAfter> {
        view.agent_in_view.as_ref()?;
        let prompt = chat.and_then(|chat| chat.prompt.as_ref());
        if prompt.is_some_and(|prompt| prompt.kind == ChatPromptKind::Question) {
            self.question_armed = true;
        }
        if view.dialog {
            return None;
        }
        if let Some(heads_up) = prompt.filter(|prompt| prompt.kind == ChatPromptKind::HeadsUp) {
            let identity = crate::screen::dialogs::prompt_identity(heads_up);
            if self.handled_heads_up != Some(identity) {
                self.handled_heads_up = Some(identity);
                self.note_handled |= note;
                return Some(RestoreAfter::HeadsUpGone);
            }
        }
        let keys_free = chat.is_some_and(|chat| {
            !matches!(
                chat.composer,
                ComposerState::Dialog | ComposerState::Overlay { .. }
            )
        });
        if self.question_armed && prompt.is_none() && keys_free {
            self.question_armed = false;
            return Some(RestoreAfter::MainWorking);
        }
        if note && !self.note_handled {
            self.note_handled = true;
            return Some(RestoreAfter::HeadsUpGone);
        }
        None
    }

    /// A prompt for main came up with an agent in view: stash the agent's
    /// draft and drive to main, or leave a held stash alone.
    fn on_trigger(
        &mut self,
        agent: &AgentId,
        view: &ViewReading,
        after: RestoreAfter,
        now: Instant,
        out: &mut Vec<ReturnAction>,
    ) {
        let Some(in_view) = view.agent_in_view.clone() else {
            return;
        };
        match view.box_first_line.as_deref() {
            Some(line) if line.starts_with(NOTE_PREFIX) => {
                out.push(ReturnAction::Log(format!(
                    "returning to main from {in_view}'s view with the note's own draft"
                )));
                self.start_drive(agent, view, ViewGoal::Main, Purpose::Return, now, out);
            }
            Some(_) if view.stashed => {
                out.push(ReturnAction::Notice(SessionNoticeKind::WaitingInMain));
                out.push(ReturnAction::Log(format!(
                    "not leaving {in_view}'s view: a draft is in the box and a stash is held"
                )));
            }
            Some(_) => {
                let Some(row) = view.panel_in_view else {
                    out.push(ReturnAction::Warn(format!(
                        "not leaving {in_view}'s view: its draft has no panel row to come back to"
                    )));
                    return;
                };
                out.push(ReturnAction::Log(format!("stashing {in_view}'s draft")));
                out.push(ReturnAction::Press(Key::Stash));
                self.task = Some(Task::Stashing(SavedDraft {
                    agent: in_view,
                    row,
                    after,
                }));
            }
            None => {
                out.push(ReturnAction::Log(format!(
                    "returning to main from {in_view}'s view"
                )));
                self.start_drive(agent, view, ViewGoal::Main, Purpose::Return, now, out);
            }
        }
    }

    /// The reading after the stash key: the box empty and the stash held,
    /// or the return stops.
    fn after_stash(
        &mut self,
        agent: &AgentId,
        view: &ViewReading,
        draft: SavedDraft,
        now: Instant,
        out: &mut Vec<ReturnAction>,
    ) {
        if view.box_first_line.is_some() || !view.stashed {
            out.push(ReturnAction::Warn(format!(
                "the stash key did not empty {}'s box into the stash; staying in its view",
                draft.agent
            )));
            return;
        }
        self.saved = Some(draft);
        self.start_drive(agent, view, ViewGoal::Main, Purpose::Return, now, out);
    }

    /// With a draft saved and main in view once its condition is met, take
    /// the draft back to its agent.
    fn try_restore(
        &mut self,
        agent: &AgentId,
        view: &ViewReading,
        chat: Option<&ChatScreen>,
        now: Instant,
        out: &mut Vec<ReturnAction>,
    ) {
        let Some(saved) = self.saved.clone() else {
            return;
        };
        let Some(chat) = chat else {
            return;
        };
        if view.agent_in_view.is_some() || view.dialog || view.box_first_line.is_some() {
            return;
        }
        let due = match saved.after {
            RestoreAfter::HeadsUpGone => chat
                .prompt
                .as_ref()
                .is_none_or(|prompt| prompt.kind != ChatPromptKind::HeadsUp),
            RestoreAfter::MainWorking => chat.composer == ComposerState::Working,
        };
        if !due || !self.stash_still_held(view, out) {
            return;
        }
        if view.panel.is_empty() {
            out.push(ReturnAction::Log(format!(
                "no agent is listed any more; {}'s draft goes back in main",
                saved.agent
            )));
            self.restore_in_main(agent, view, now, out);
            return;
        }
        let row = saved.row;
        out.push(ReturnAction::Log(format!(
            "reopening {}'s view to restore its draft",
            saved.agent
        )));
        self.start_drive(
            agent,
            view,
            ViewGoal::Agent { row },
            Purpose::Reopen,
            now,
            out,
        );
    }

    /// Whether the stash still holds the saved draft; drops the draft with a
    /// log line when it does not.
    fn stash_still_held(&mut self, view: &ViewReading, out: &mut Vec<ReturnAction>) -> bool {
        if view.stashed {
            return true;
        }
        if let Some(saved) = self.saved.take() {
            out.push(ReturnAction::Log(format!(
                "the saved draft is no longer in the stash; {}'s draft is not restored",
                saved.agent
            )));
        }
        false
    }

    /// Start a drive towards `goal` and take its first step on `view`.
    fn start_drive(
        &mut self,
        agent: &AgentId,
        view: &ViewReading,
        goal: ViewGoal,
        purpose: Purpose,
        now: Instant,
        out: &mut Vec<ReturnAction>,
    ) {
        self.drive_on(agent, view, ViewDrive::new(goal, now), purpose, now, out);
    }

    /// One step of a running drive on `view`.
    fn drive_on(
        &mut self,
        agent: &AgentId,
        view: &ViewReading,
        mut drive: ViewDrive,
        purpose: Purpose,
        now: Instant,
        out: &mut Vec<ReturnAction>,
    ) {
        let step = crate::screen::dialogs::next_view_key(agent, drive.goal(), view);
        match drive.step(view, step, now) {
            DriveAction::Press(key) => {
                out.push(ReturnAction::Press(key));
                self.task = Some(Task::Drive(drive, purpose));
            }
            DriveAction::Reached => self.reached(agent, view, purpose, now, out),
            DriveAction::Failed(failure) => {
                if self.drive_failed(drive.goal(), purpose, failure, out) {
                    self.restore_in_main(agent, view, now, out);
                }
            }
        }
    }

    /// A drive stopped short; answers whether the saved draft now goes back
    /// in main. A failed reopen does that instead; a failed return to main
    /// for the draft drops it, leaving it in the stash. Each saved draft
    /// thus gets one drive of each.
    fn drive_failed(
        &mut self,
        goal: ViewGoal,
        purpose: Purpose,
        failure: DriveFailure,
        out: &mut Vec<ReturnAction>,
    ) -> bool {
        let stopped = format!("the view switch to {goal:?} stopped: {failure:?}");
        match purpose {
            Purpose::Return => out.push(ReturnAction::Warn(stopped)),
            Purpose::Reopen => {
                let line = format!("{stopped}; the draft goes back in main");
                out.push(
                    if failure == DriveFailure::Refused(ViewRefusal::RowMissing) {
                        ReturnAction::Log(line)
                    } else {
                        ReturnAction::Warn(line)
                    },
                );
                return true;
            }
            Purpose::RestoreInMain => {
                if let Some(saved) = self.saved.take() {
                    out.push(ReturnAction::Warn(format!(
                        "{stopped}; {}'s draft stays in the stash",
                        saved.agent
                    )));
                }
            }
        }
        false
    }

    /// A drive reached its goal's view.
    fn reached(
        &mut self,
        agent: &AgentId,
        view: &ViewReading,
        purpose: Purpose,
        now: Instant,
        out: &mut Vec<ReturnAction>,
    ) {
        match purpose {
            Purpose::Return => {}
            Purpose::Reopen => {
                if !self.stash_still_held(view, out) {
                    return;
                }
                let Some(saved) = self.saved.as_ref() else {
                    return;
                };
                if view.agent_in_view.as_deref() == Some(saved.agent.as_str())
                    && view.box_first_line.is_none()
                {
                    out.push(ReturnAction::Log(format!(
                        "restoring {}'s draft",
                        saved.agent
                    )));
                    out.push(ReturnAction::Press(Key::Stash));
                    self.saved = None;
                } else {
                    self.restore_in_main(agent, view, now, out);
                }
            }
            Purpose::RestoreInMain => {
                if !self.stash_still_held(view, out) {
                    return;
                }
                let Some(saved) = self.saved.take() else {
                    return;
                };
                if view.box_first_line.is_some() {
                    out.push(ReturnAction::Warn(format!(
                        "main's box holds text; {}'s draft stays in the stash",
                        saved.agent
                    )));
                    return;
                }
                out.push(ReturnAction::Log(format!(
                    "{}'s view is gone; its draft goes back in main",
                    saved.agent
                )));
                out.push(ReturnAction::Press(Key::Stash));
                out.push(ReturnAction::Notice(
                    SessionNoticeKind::DraftReturnedToMain { agent: saved.agent },
                ));
            }
        }
    }

    /// The saved draft's agent is gone or not in view: put the draft in
    /// main's box instead.
    fn restore_in_main(
        &mut self,
        agent: &AgentId,
        view: &ViewReading,
        now: Instant,
        out: &mut Vec<ReturnAction>,
    ) {
        self.start_drive(
            agent,
            view,
            ViewGoal::Main,
            Purpose::RestoreInMain,
            now,
            out,
        );
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use protocol::ChatPrompt;

    const SLEEPER: &str = "Sleeper A";
    const PANEL: [&str; 2] = ["main", "general-purpose  Sleeper A"];

    fn claude() -> AgentId {
        agent_registry::default_agent()
    }

    /// The main conversation in view, the box holding the keys and empty.
    fn main_view() -> ViewReading {
        ViewReading {
            agent_in_view: None,
            key_holder: KeyHolder::Box,
            stashed: false,
            box_first_line: None,
            dialog: false,
            panel: PANEL.iter().map(|&row| row.to_owned()).collect(),
            panel_cursor: None,
            panel_in_view: Some(0),
        }
    }

    /// The agent described as `agent` in view on panel row 1.
    fn agent_view(agent: &str) -> ViewReading {
        ViewReading {
            agent_in_view: Some(agent.to_owned()),
            panel_in_view: Some(1),
            ..main_view()
        }
    }

    fn with_box(view: ViewReading, line: &str, stashed: bool) -> ViewReading {
        ViewReading {
            box_first_line: Some(line.to_owned()),
            stashed,
            ..view
        }
    }

    fn stashed(view: ViewReading) -> ViewReading {
        ViewReading {
            stashed: true,
            ..view
        }
    }

    fn prompt(kind: ChatPromptKind, body: &str) -> ChatPrompt {
        ChatPrompt {
            id: "p1".to_owned(),
            kind,
            subject: String::new(),
            body: body.to_owned(),
            options: Vec::new(),
            questions: Vec::new(),
            can_amend: false,
            always_allow_tip: None,
            multi: false,
            cursor: None,
        }
    }

    fn chat(composer: ComposerState, prompt: Option<ChatPrompt>) -> ChatScreen {
        ChatScreen { composer, prompt }
    }

    fn heads_up(body: &str) -> ChatScreen {
        chat(
            ComposerState::Ready,
            Some(prompt(ChatPromptKind::HeadsUp, body)),
        )
    }

    fn idle() -> ChatScreen {
        chat(ComposerState::Ready, None)
    }

    fn keys(actions: &[ReturnAction]) -> Vec<Key> {
        actions
            .iter()
            .filter_map(|action| match action {
                ReturnAction::Press(key) => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    fn notices(actions: &[ReturnAction]) -> Vec<SessionNoticeKind> {
        actions
            .iter()
            .filter_map(|action| match action {
                ReturnAction::Notice(notice) => Some(notice.clone()),
                _ => None,
            })
            .collect()
    }

    fn feed(
        machine: &mut ViewReturn,
        view: &ViewReading,
        chat: Option<&ChatScreen>,
    ) -> Vec<ReturnAction> {
        machine.on_reading(&claude(), view, chat, Instant::now())
    }

    /// The reader of a session's screen as its output arrives.
    mod reader {
        use super::*;
        use crate::history::test_support::recording_pty;
        use crate::screen::fixture_screen::screen_from;
        use std::sync::Mutex;

        /// Main with its box empty and holding the keys.
        const V176: &str = include_str!("screen/fixtures/v176-main-heads-up-box-keys.txt");
        const HANG: Duration = Duration::from_secs(30);

        /// The bytes that draw v176 with `text` in its box.
        fn frame(text: &str) -> Vec<u8> {
            let rows: Vec<String> = V176
                .lines()
                .map(|row| {
                    if row.trim_end() == "❯" {
                        format!("❯ {text}")
                    } else {
                        row.to_owned()
                    }
                })
                .collect();
            format!("\x1b[H\x1b[2J{}", rows.join("\r\n")).into_bytes()
        }

        fn chunk(n: usize) -> Vec<u8> {
            frame(&format!("chunk {n:04}"))
        }

        /// The text in the reading's box.
        fn shown(reading: &Reading) -> Option<String> {
            reading
                .view
                .as_ref()
                .and_then(|view| view.box_first_line.clone())
        }

        fn grid() -> SharedScreen {
            Arc::new(Mutex::new(screen_from(V176)))
        }

        /// The output never pauses for [`READ_QUIET`] and the grid stays one
        /// chunk behind it while it flows, so the reader decides at its
        /// deadline; the frame it reads holds every chunk it received.
        #[tokio::test]
        async fn a_reading_at_the_deadline_holds_every_chunk_it_saw() {
            let (pty, _input, exit) = recording_pty();
            let screen = grid();
            let mut reader = ScreenReader::new(Arc::clone(&pty), Arc::clone(&screen), claude());
            reader.arm();
            let task = tokio::spawn(async move {
                let reading = reader.after_key().await;
                (reading, reader)
            });
            let mut sent = 0;
            while !task.is_finished() && sent < 240 {
                exit.chunk(chunk(sent)).await;
                if sent > 0 {
                    lock(&screen).feed(&chunk(sent - 1));
                }
                sent += 1;
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            lock(&screen).feed(&chunk(sent - 1));
            let (reading, reader) = tokio::time::timeout(HANG, task)
                .await
                .expect("hung: the reader never read")
                .expect("the reader ran");
            let size = u64::try_from(chunk(0).len()).expect("a chunk's size fits");
            let received = reader.seen / size;
            assert!(received > 0, "the reader received output");
            let shown = shown(&reading).expect("a chunk is in the box");
            let shown: u64 = shown
                .strip_prefix("chunk ")
                .and_then(|n| n.parse().ok())
                .expect("the box shows a chunk");
            assert!(
                shown + 1 >= received,
                "the reading shows chunk {shown}, but the reader received {received} chunks"
            );
        }

        /// A chunk sent while the reader waits for the screen's lock to read
        /// its base is one it receives, and the reading after it waits for
        /// the grid to draw it.
        #[tokio::test]
        #[expect(
            clippy::await_holding_lock,
            reason = "the test holds the screen while the reader arms on another thread"
        )]
        async fn the_reader_subscribes_before_it_reads_its_base() {
            let (pty, _input, exit) = recording_pty();
            let screen = grid();
            let mut reader = ScreenReader::new(Arc::clone(&pty), Arc::clone(&screen), claude());
            let before = pty.output.receiver_count();
            let held = lock(&screen);
            let arming = std::thread::spawn(move || {
                reader.arm();
                reader
            });
            tokio::time::timeout(HANG, async {
                while pty.output.receiver_count() == before {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("hung: the reader did not subscribe before reading its base");
            let queued = frame("queued before the base");
            exit.chunk(queued.clone()).await;
            drop(held);
            let mut reader = arming.join().expect("the reader armed");
            lock(&screen).feed(&queued);
            let reading = tokio::time::timeout(HANG, reader.after_key())
                .await
                .expect("hung: the reader never read");
            assert_eq!(shown(&reading).as_deref(), Some("queued before the base"));
        }
    }

    #[test]
    fn a_drive_fails_on_the_fourth_down_in_a_row_off_the_panel() {
        let now = Instant::now();
        let mut drive = ViewDrive::new(ViewGoal::Main, now);
        let off_panel = agent_view(SLEEPER);
        for _ in 0..MAX_DOWNS {
            assert_eq!(
                drive.step(&off_panel, ViewStep::Press(Key::Down), now),
                DriveAction::Press(Key::Down)
            );
        }
        assert_eq!(
            drive.step(&off_panel, ViewStep::Press(Key::Down), now),
            DriveAction::Failed(DriveFailure::TooManyDowns)
        );
    }

    #[test]
    fn another_key_resets_the_downs_in_a_row() {
        let now = Instant::now();
        let mut drive = ViewDrive::new(ViewGoal::Main, now);
        let off_panel = agent_view(SLEEPER);
        for _ in 0..MAX_DOWNS {
            drive.step(&off_panel, ViewStep::Press(Key::Down), now);
        }
        assert_eq!(
            drive.step(&off_panel, ViewStep::Press(Key::Up), now),
            DriveAction::Press(Key::Up)
        );
        assert_eq!(
            drive.step(&off_panel, ViewStep::Press(Key::Down), now),
            DriveAction::Press(Key::Down),
            "the Up broke the run"
        );
    }

    #[test]
    fn downs_on_the_panel_count_only_as_keys_and_the_thirteenth_key_fails() {
        let now = Instant::now();
        let mut drive = ViewDrive::new(ViewGoal::Agent { row: 1 }, now);
        let on_panel = ViewReading {
            key_holder: KeyHolder::Panel,
            ..main_view()
        };
        for n in 0..MAX_KEYS {
            assert_eq!(
                drive.step(&on_panel, ViewStep::Press(Key::Down), now),
                DriveAction::Press(Key::Down),
                "key {n}"
            );
        }
        assert_eq!(drive.last_key_at(), Some(now));
        assert_eq!(
            drive.step(&on_panel, ViewStep::Press(Key::Down), now),
            DriveAction::Failed(DriveFailure::TooManyKeys)
        );
    }

    #[test]
    fn a_drive_times_out_after_its_limit() {
        let start = Instant::now();
        let mut drive = ViewDrive::new(ViewGoal::Main, start);
        let view = agent_view(SLEEPER);
        assert_eq!(
            drive.step(&view, ViewStep::Press(Key::Down), start + DRIVE_LIMIT),
            DriveAction::Press(Key::Down),
            "the limit itself is still in time"
        );
        assert_eq!(
            drive.step(
                &view,
                ViewStep::Press(Key::Down),
                start + DRIVE_LIMIT + Duration::from_millis(1)
            ),
            DriveAction::Failed(DriveFailure::TimedOut)
        );
    }

    #[test]
    fn a_drive_reaches_on_done_and_fails_on_a_refusal() {
        let now = Instant::now();
        let mut drive = ViewDrive::new(ViewGoal::Main, now);
        assert_eq!(
            drive.step(&main_view(), ViewStep::Done, now),
            DriveAction::Reached
        );
        assert_eq!(
            drive.step(&main_view(), ViewStep::Refuse(ViewRefusal::DialogUp), now),
            DriveAction::Failed(DriveFailure::Refused(ViewRefusal::DialogUp))
        );
    }

    #[test]
    fn a_heads_up_returns_once_per_notice_and_never_from_main() {
        let mut from_main = ViewReturn::default();
        assert!(
            keys(&feed(&mut from_main, &main_view(), Some(&heads_up("one")))).is_empty(),
            "main is already in view"
        );

        let mut machine = ViewReturn::default();
        let first = heads_up("one");
        assert_eq!(
            keys(&feed(&mut machine, &agent_view(SLEEPER), Some(&first))),
            [Key::Down]
        );
        assert!(
            keys(&feed(&mut machine, &main_view(), Some(&first))).is_empty(),
            "main reached"
        );
        assert!(
            keys(&feed(&mut machine, &agent_view(SLEEPER), Some(&first))).is_empty(),
            "the same notice in the agent's view again"
        );
        assert_eq!(
            keys(&feed(
                &mut machine,
                &agent_view(SLEEPER),
                Some(&heads_up("two"))
            )),
            [Key::Down],
            "a new notice returns again"
        );
    }

    #[test]
    fn a_question_that_closes_in_an_agents_view_returns_to_main() {
        let question = chat(
            ComposerState::Dialog,
            Some(prompt(ChatPromptKind::Question, "Pick a colour")),
        );
        let dialog = ViewReading {
            dialog: true,
            ..agent_view(SLEEPER)
        };
        let mut machine = ViewReturn::default();
        assert_eq!(
            keys(&feed(&mut machine, &dialog, Some(&question))),
            [],
            "the dialog is up"
        );
        assert_eq!(
            keys(&feed(&mut machine, &agent_view(SLEEPER), Some(&idle()))),
            [Key::Down],
            "the question is gone with the agent still in view"
        );

        let mut went_to_main = ViewReturn::default();
        feed(&mut went_to_main, &dialog, Some(&question));
        assert_eq!(
            keys(&feed(&mut went_to_main, &main_view(), Some(&idle()))),
            [],
            "main is in view"
        );
        assert!(
            keys(&feed(
                &mut went_to_main,
                &agent_view(SLEEPER),
                Some(&idle())
            ))
            .is_empty(),
            "main in view disarmed it"
        );
    }

    #[test]
    fn the_notes_own_draft_drives_to_main_without_a_stash() {
        let mut machine = ViewReturn::default();
        let view = with_box(
            agent_view(SLEEPER),
            "Here is a note offered by a side agent:",
            false,
        );
        let actions = feed(&mut machine, &view, Some(&heads_up("one")));
        assert_eq!(keys(&actions), [Key::Down], "no stash key: {actions:?}");
        assert_eq!(machine.saved(), None);
    }

    #[test]
    fn the_notes_draft_in_an_agents_view_returns_once_while_it_stays() {
        let note = "Here is a note offered by a side agent:";
        let in_box = with_box(agent_view(SLEEPER), note, false);
        let mut machine = ViewReturn::default();
        assert_eq!(
            keys(&feed(&mut machine, &in_box, Some(&idle()))),
            [Key::Down]
        );
        let dialog = ViewReading {
            dialog: true,
            ..in_box.clone()
        };
        let stopped = feed(&mut machine, &dialog, Some(&idle()));
        assert!(
            keys(&stopped).is_empty() && matches!(stopped.as_slice(), [ReturnAction::Warn(_)]),
            "the drive stops on the dialog: {stopped:?}"
        );
        assert!(
            keys(&feed(&mut machine, &in_box, Some(&idle()))).is_empty(),
            "the same draft in the box fires no second return"
        );
        assert_eq!(
            keys(&feed(&mut machine, &agent_view(SLEEPER), Some(&idle()))),
            [],
            "an empty box clears the note's mark"
        );
        assert_eq!(
            keys(&feed(&mut machine, &in_box, Some(&idle()))),
            [Key::Down],
            "the box emptied in between, so the note is new"
        );
    }

    #[test]
    fn a_draft_with_a_stash_held_stays_and_says_main_waits() {
        let mut machine = ViewReturn::default();
        let view = with_box(agent_view(SLEEPER), "my draft", true);
        let actions = feed(&mut machine, &view, Some(&heads_up("one")));
        assert!(keys(&actions).is_empty(), "{actions:?}");
        assert_eq!(notices(&actions), [SessionNoticeKind::WaitingInMain]);
        assert_eq!(machine.saved(), None);
    }

    /// A machine that stashed Sleeper A's draft for a Heads up and is back
    /// in main with the notice still up.
    fn stashed_and_back_in_main() -> ViewReturn {
        let mut machine = ViewReturn::default();
        let notice = heads_up("one");
        let draft = with_box(agent_view(SLEEPER), "my draft", false);
        assert_eq!(
            keys(&feed(&mut machine, &draft, Some(&notice))),
            [Key::Stash],
            "the stash goes first"
        );
        assert_eq!(
            keys(&feed(
                &mut machine,
                &stashed(agent_view(SLEEPER)),
                Some(&notice)
            )),
            [Key::Down]
        );
        assert_eq!(
            machine.saved(),
            Some(&SavedDraft {
                agent: SLEEPER.to_owned(),
                row: 1,
                after: RestoreAfter::HeadsUpGone,
            })
        );
        assert_eq!(
            keys(&feed(&mut machine, &stashed(main_view()), Some(&notice))),
            [],
            "main reached"
        );
        machine
    }

    #[test]
    fn a_draft_is_stashed_first_and_saved() {
        let machine = stashed_and_back_in_main();
        assert!(machine.saved().is_some());
    }

    #[test]
    fn a_stash_that_did_not_take_stops_with_no_key() {
        let mut machine = ViewReturn::default();
        let draft = with_box(agent_view(SLEEPER), "my draft", false);
        feed(&mut machine, &draft, Some(&heads_up("one")));
        let actions = feed(&mut machine, &draft, Some(&heads_up("one")));
        assert!(keys(&actions).is_empty(), "{actions:?}");
        assert_eq!(machine.saved(), None);
    }

    #[test]
    fn the_restore_reopens_the_row_and_stashes_back_on_an_empty_box() {
        let mut machine = stashed_and_back_in_main();
        assert!(
            keys(&feed(
                &mut machine,
                &stashed(main_view()),
                Some(&heads_up("one"))
            ))
            .is_empty(),
            "the notice is still up"
        );
        assert_eq!(
            keys(&feed(&mut machine, &stashed(main_view()), Some(&idle()))),
            [Key::Down],
            "the notice went: reopen the agent"
        );
        assert_eq!(
            keys(&feed(
                &mut machine,
                &stashed(agent_view(SLEEPER)),
                Some(&idle())
            )),
            [Key::Stash]
        );
        assert_eq!(machine.saved(), None, "the draft is back");
    }

    #[test]
    fn the_restore_never_stashes_over_text_in_the_box() {
        let mut machine = stashed_and_back_in_main();
        feed(&mut machine, &stashed(main_view()), Some(&idle()));
        let typed = with_box(agent_view(SLEEPER), "typed meanwhile", true);
        let actions = feed(&mut machine, &typed, Some(&idle()));
        assert!(!keys(&actions).contains(&Key::Stash), "{actions:?}");
    }

    #[test]
    fn a_wrong_agent_puts_the_draft_back_in_main() {
        let mut machine = stashed_and_back_in_main();
        feed(&mut machine, &stashed(main_view()), Some(&idle()));
        assert_eq!(
            keys(&feed(
                &mut machine,
                &stashed(agent_view("Sleeper B")),
                Some(&idle())
            )),
            [Key::Down],
            "another agent sits on the row: back to main"
        );
        let actions = feed(&mut machine, &stashed(main_view()), Some(&idle()));
        assert_eq!(keys(&actions), [Key::Stash]);
        assert_eq!(
            notices(&actions),
            [SessionNoticeKind::DraftReturnedToMain {
                agent: SLEEPER.to_owned()
            }]
        );
        assert_eq!(machine.saved(), None);
    }

    #[test]
    fn a_missing_row_puts_the_draft_back_in_main() {
        let mut machine = stashed_and_back_in_main();
        let row_gone = ViewReading {
            panel: vec!["main".to_owned()],
            ..stashed(main_view())
        };
        let actions = feed(&mut machine, &row_gone, Some(&idle()));
        assert_eq!(keys(&actions), [Key::Stash], "{actions:?}");
        assert_eq!(
            notices(&actions),
            [SessionNoticeKind::DraftReturnedToMain {
                agent: SLEEPER.to_owned()
            }]
        );
    }

    #[test]
    fn a_stash_gone_at_restore_drops_the_draft_with_a_log() {
        let mut machine = stashed_and_back_in_main();
        let actions = feed(&mut machine, &main_view(), Some(&idle()));
        assert!(keys(&actions).is_empty(), "{actions:?}");
        assert!(
            actions.iter().any(|action| matches!(action,
                ReturnAction::Log(text) if text.contains("no longer in the stash"))),
            "{actions:?}"
        );
        assert_eq!(machine.saved(), None);
        let after = feed(&mut machine, &main_view(), Some(&idle()));
        assert!(after.is_empty(), "nothing left to do: {after:?}");
    }

    #[test]
    fn a_finished_last_agent_restores_in_main() {
        let mut machine = stashed_and_back_in_main();
        let no_panel = ViewReading {
            panel: Vec::new(),
            panel_in_view: None,
            ..stashed(main_view())
        };
        let actions = feed(&mut machine, &no_panel, Some(&idle()));
        assert_eq!(keys(&actions), [Key::Stash], "no Down: {actions:?}");
        assert_eq!(
            notices(&actions),
            [SessionNoticeKind::DraftReturnedToMain {
                agent: SLEEPER.to_owned()
            }]
        );
        assert_eq!(machine.saved(), None);
    }

    #[test]
    fn a_failing_reopen_never_repeats() {
        let mut machine = stashed_and_back_in_main();
        let dialog = ViewReading {
            dialog: true,
            ..stashed(main_view())
        };
        let mut pressed = Vec::new();
        for _ in 0..5 {
            pressed.extend(keys(&feed(
                &mut machine,
                &stashed(main_view()),
                Some(&idle()),
            )));
            pressed.extend(keys(&feed(&mut machine, &dialog, Some(&idle()))));
        }
        assert_eq!(pressed, [Key::Down], "one reopen, then nothing");
        assert_eq!(machine.saved(), None, "the draft is dropped");
    }

    #[test]
    fn no_restore_while_main_box_holds_text() {
        let mut machine = stashed_and_back_in_main();
        let typing = with_box(main_view(), "typed in main", true);
        for _ in 0..2 {
            let actions = feed(&mut machine, &typing, Some(&idle()));
            assert!(keys(&actions).is_empty(), "{actions:?}");
        }
        assert!(machine.saved().is_some(), "the draft stays saved");
    }

    #[test]
    fn a_user_stash_drops_the_saved_draft() {
        let mut machine = stashed_and_back_in_main();
        let notice = heads_up("one");
        let typing = with_box(main_view(), "typed in main", true);
        assert_eq!(
            keys(&feed(&mut machine, &typing, Some(&notice))),
            [],
            "the notice is up"
        );
        let emptied = feed(&mut machine, &stashed(main_view()), Some(&notice));
        assert!(
            keys(&emptied).is_empty() && notices(&emptied).is_empty(),
            "{emptied:?}"
        );
        assert_eq!(machine.saved(), None, "the stash holds the user's text now");
        let after = feed(&mut machine, &stashed(main_view()), Some(&idle()));
        assert!(keys(&after).is_empty(), "no reopen: {after:?}");
    }

    #[test]
    fn a_sent_message_keeps_the_saved_draft() {
        let mut machine = stashed_and_back_in_main();
        let notice = heads_up("one");
        let typing = with_box(main_view(), "typed in main", true);
        assert_eq!(
            keys(&feed(&mut machine, &typing, Some(&notice))),
            [],
            "the notice is up"
        );
        let sent = feed(
            &mut machine,
            &stashed(main_view()),
            Some(&chat(ComposerState::Working, None)),
        );
        assert!(
            machine.saved().is_some(),
            "a send keeps the draft: {sent:?}"
        );
        assert_eq!(keys(&sent), [Key::Down], "and the restore goes on");
    }

    #[test]
    fn the_note_draft_is_followed_to_main_once() {
        let note = "Here is a note offered by a side agent: look at the logs";
        let in_agent = with_box(agent_view(SLEEPER), note, false);
        let notice = heads_up("one");
        let mut machine = ViewReturn::default();
        assert_eq!(
            keys(&feed(&mut machine, &in_agent, Some(&notice))),
            [Key::Down]
        );
        assert!(
            keys(&feed(
                &mut machine,
                &with_box(main_view(), note, false),
                Some(&notice)
            ))
            .is_empty(),
            "main reached with the note carried"
        );
        assert!(
            keys(&feed(&mut machine, &in_agent, Some(&idle()))).is_empty(),
            "the same note draft in an agent's view again"
        );
    }

    #[test]
    fn a_repeated_heads_up_returns_again() {
        let mut machine = ViewReturn::default();
        assert_eq!(
            keys(&feed(
                &mut machine,
                &agent_view(SLEEPER),
                Some(&heads_up("one"))
            )),
            [Key::Down]
        );
        assert_eq!(
            keys(&feed(&mut machine, &main_view(), Some(&idle()))),
            [],
            "main reached"
        );
        assert_eq!(
            keys(&feed(&mut machine, &agent_view(SLEEPER), Some(&idle()))),
            [],
            "no notice"
        );
        assert_eq!(
            keys(&feed(
                &mut machine,
                &agent_view(SLEEPER),
                Some(&heads_up("one"))
            )),
            [Key::Down],
            "the same text, shown again, returns again"
        );
    }

    #[test]
    fn trigger_b_waits_out_a_half_drawn_dialog() {
        let question = chat(
            ComposerState::Dialog,
            Some(prompt(ChatPromptKind::Question, "Pick a colour")),
        );
        let dialog = ViewReading {
            dialog: true,
            ..agent_view(SLEEPER)
        };
        let mut machine = ViewReturn::default();
        assert_eq!(
            keys(&feed(&mut machine, &dialog, Some(&question))),
            [],
            "the question is up"
        );
        let half_drawn = [
            (agent_view(SLEEPER), chat(ComposerState::Dialog, None)),
            (
                agent_view(SLEEPER),
                chat(
                    ComposerState::Overlay {
                        name: "help".to_owned(),
                    },
                    None,
                ),
            ),
            (dialog.clone(), idle()),
        ];
        for (view, screen) in &half_drawn {
            let actions = feed(&mut machine, view, Some(screen));
            assert!(
                keys(&actions).is_empty(),
                "{view:?} {screen:?}: {actions:?}"
            );
        }
        assert_eq!(
            keys(&feed(&mut machine, &agent_view(SLEEPER), Some(&idle()))),
            [Key::Down],
            "the question is gone and nothing owns the keys"
        );
    }

    #[test]
    fn a_question_draft_waits_for_main_to_work() {
        let question = chat(
            ComposerState::Dialog,
            Some(prompt(ChatPromptKind::Question, "Pick a colour")),
        );
        let dialog = ViewReading {
            dialog: true,
            ..agent_view(SLEEPER)
        };
        let mut machine = ViewReturn::default();
        feed(&mut machine, &dialog, Some(&question));
        let draft = with_box(agent_view(SLEEPER), "my draft", false);
        assert_eq!(
            keys(&feed(&mut machine, &draft, Some(&idle()))),
            [Key::Stash]
        );
        feed(&mut machine, &stashed(agent_view(SLEEPER)), Some(&idle()));
        assert_eq!(
            machine.saved().expect("saved").after,
            RestoreAfter::MainWorking
        );
        feed(&mut machine, &stashed(main_view()), Some(&idle()));
        assert!(
            keys(&feed(&mut machine, &stashed(main_view()), Some(&idle()))).is_empty(),
            "main is not working yet"
        );
        assert_eq!(
            keys(&feed(
                &mut machine,
                &stashed(main_view()),
                Some(&chat(ComposerState::Working, None))
            )),
            [Key::Down]
        );
    }
}
