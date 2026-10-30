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

use std::time::Duration;

use agent_api::{Key, KeyHolder, ViewGoal, ViewReading, ViewRefusal, ViewStep};
use protocol::{AgentId, ChatPromptKind, ChatScreen, ComposerState, SessionNoticeKind};
use tokio::time::Instant;

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
}

impl ViewReturn {
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
