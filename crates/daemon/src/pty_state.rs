//! Status detection for PTY-driven sessions.
//!
//! An interactive agent session's status comes from its hooks and from its
//! screen. [`watch`] feeds the session's grid and, once output settles, reads
//! it with the session's agent's screen rules
//! ([`crate::screen::rules::CompiledRules::read`]). What the screen shows is
//! published behind hysteresis, a hold after any other writer's change, and a
//! guard that refuses the write once the record has moved since the read. A
//! quiet screen is never read as idle on its own: a spinner still on screen
//! keeps a turn `Working` however long the PTY is silent.
//!
//! A plain shell ([`watch_plain_shell`]) runs the same screen check while
//! an agent runs in it: while hooks drive it, or once its screen has shown
//! the agent for a whole confirmation window. Once the shell's prompt has been
//! back on the grid's last row for a whole window, the shell is handed back
//! to its heuristic classifier:
//! - an output-vs-input byte-volume comparison: command output outpaces
//!   keystroke echo and promotes to `working`
//! - a terminal-title heartbeat: a title change counts as activity (see
//!   [`crate::osc_title::TitleActivity`])
//! - an idle timeout with hysteresis: promotion to `working` happens on the
//!   first qualifying chunk, but demotion back to `idle` waits for
//!   [`SHELL_IDLE_AFTER`] of silence

use crate::chat_mirror::ChatTap;
use crate::osc_title::TitleActivity;
use crate::pty::PtyHandle;
use crate::screen::SharedScreen;
use crate::screen::rules::{self, CompiledRules, Reading, ScreenState, Verdict};
use crate::session::{
    SessionEvent, SessionRecord, SessionRegistry, ShownAgent, push_recent_action,
};
use crate::typing_lane::{Holder, TypingTurn};
use crate::view_drive::{DRIVE_LIMIT, KEY_WAIT, ReturnAction, ViewReturn};
use agent_api::ViewReading;
use protocol::{AgentId, AttentionReason, ChatScreen, ComposerState, SessionStatus};
use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};
use tracing::{debug, info, warn};

/// Silence required before a plain shell classifier's `Working` settles back
/// to `Idle`. Deliberately asymmetric with promotion (which is immediate): a
/// command repainting once a second (an elapsed-time counter) must hold
/// `working` instead of flapping. 2.5 s clears a 1 Hz cadence with room for
/// scheduler jitter while still reading as "idle" promptly once the command
/// really stops.
const SHELL_IDLE_AFTER: Duration = Duration::from_millis(2500);

/// Sliding window used to compare input vs output byte volume. Chosen to be
/// long enough to smooth out inter-keystroke timing for slow typists, short
/// enough that the dot reacts to command output within a fraction of a second.
const VOLUME_WINDOW: Duration = Duration::from_millis(750);

/// Shells echo commands and prompts, but a real command's output should
/// outpace that echo quickly. PowerShell's `PSReadLine` drives this — one
/// keystroke can repaint the current line with syntax-highlighted colors,
/// emitting cursor-position escapes, color resets, and the redrawn line
/// (easily 60–200 bytes per char). Keep this loose enough for `ls`-class
/// commands but generous enough that typing doesn't flap the status dot.
const SHELL_ECHO_BUDGET_PER_INPUT_BYTE: u64 = 64;
const SHELL_MIN_WORKING_OUTPUT: u64 = 320;
const SHELL_MIN_WORKING_EXCESS: u64 = 256;

/// Quiet time after a chunk of output before a watcher reads the screen, so
/// a repaint split across chunks is read whole.
const CHECK_DEBOUNCE: Duration = Duration::from_millis(40);

/// The longest a check waits behind continuous output: a waiting dialog's
/// tool marker blinks every frame, which would otherwise defer it forever.
const CHECK_MAX_WAIT: Duration = Duration::from_millis(150);

/// The least time between two checks that both count toward a gated change.
const CONFIRM_SPACING: Duration = Duration::from_millis(150);

/// Agreeing checks a gated change needs before it publishes.
const CONFIRM_CHECKS: u8 = 3;

/// A gated change publishes on its first agreeing check this long after its
/// first one, however many have counted.
const CONFIRM_CAP: Duration = Duration::from_millis(700);

/// How long an idle reading must hold to end a hook-driven turn that no `Stop`
/// has ended. While a task is in progress each model step opens on a spinner
/// row no rule reads for a little over a second, and a window of
/// [`CONFIRM_CAP`] would publish `Idle` under it. A `Stop` that was lost still
/// ends the turn once this has passed, unless a working title holds it past
/// that (see [`TITLE_QUIET_LIMIT`]).
const IDLE_WITHOUT_STOP: Duration = Duration::from_secs(3);

/// How long a working title holds a turn no `Stop` has ended: a CLI that goes
/// quiet for this long hands the turn back, so a dead or hung one still reads
/// `Idle` without the owner doing anything.
const TITLE_QUIET_LIMIT: Duration = TURN_END_CEILING;

/// The rule that reads an Esc interrupt's row: an interrupted turn fires no
/// `Stop` and says so on screen, so it keeps the short window.
const INTERRUPT_RULE: &str = "idle_interrupted";

/// How long a watcher leaves the screen unread after another writer changes
/// the status or the hook stamp, and after it starts: the grid may still show
/// the frame from before the change.
const FOREIGN_WRITE_HOLD: Duration = Duration::from_millis(700);

/// How long a shell the spawn typed an agent's launch line into keeps its
/// classifier off, unless the agent shows on screen or its hooks take over
/// first: the injector's own limit for the agent to start.
const LAUNCH_HOLD: Duration = crate::inject::MIN_STARTUP_CAP;

/// The longest a turn end stays held. Past it a settled frame showing a turn
/// that runs, by its spinner or its title, drops the turn end; an overlay that
/// hides the screen keeps it until the overlay closes; anything else lands
/// `Idle`.
const TURN_END_CEILING: Duration = Duration::from_secs(60);

/// How soon the view switch reads the screen again after another writer
/// held the session's typing lane.
const LANE_RETRY: Duration = Duration::from_millis(500);

/// The longest the view switch holds the typing lane: two whole drives and a
/// key's wait. Past it the switch is abandoned and the lane given back.
const VIEW_TURN_LIMIT: Duration = DRIVE_LIMIT.saturating_mul(2).saturating_add(KEY_WAIT);

pub struct AttentionEvent {
    pub session_id: String,
    pub reason: AttentionReason,
}

/// What a plain shell's status watcher reads: the session's output broadcast,
/// the byte counts of the user input forwarded to it, the grid it feeds that
/// output into and reads the agent's screen from, the agent's rules, and the
/// chat mirror's tap each reading of a running agent's screen goes to.
pub struct ShellStreams {
    pub output: broadcast::Receiver<Vec<u8>>,
    pub input_pulses: mpsc::UnboundedReceiver<usize>,
    pub screen: SharedScreen,
    pub rules: Arc<CompiledRules>,
    pub chat: ChatTap,
}

/// What an agent session's status watcher reads: the session's output
/// broadcast, the grid it feeds that output into and reads the status from,
/// the agent's rules, and the chat mirror's tap each reading goes to.
pub struct AgentStreams {
    pub output: broadcast::Receiver<Vec<u8>>,
    pub screen: SharedScreen,
    pub rules: Arc<CompiledRules>,
    pub chat: ChatTap,
}

/// What a screen check reads and hands its reading to: the grid, the agent's
/// rules and the chat mirror's tap.
struct Sight {
    screen: SharedScreen,
    rules: Arc<CompiledRules>,
    chat: ChatTap,
}

/// Spawn a background task that feeds an agent session's grid from its PTY
/// output and sets the session's status from what the grid shows.
///
/// Each chunk is fed into `streams.screen`; a check then reads the screen
/// [`CHECK_DEBOUNCE`] after the output pauses, or [`CHECK_MAX_WAIT`] into a
/// burst. Entering `AwaitingInput` from `Idle` or `Working`, and a spinner
/// promoting `Idle` to `Working`, publish without a window, but only from a
/// settled frame: a check after output paused for [`CHECK_DEBOUNCE`], or two
/// executed checks in a row that agree. A dialog raises `AwaitingInput` only
/// on its edge: once the session has entered or been taken off a prompt the
/// screen still shows, a dialog raises nothing until a check has read
/// something else (a chat answer leaves the answered dialog painted until
/// the agent redraws). Every other change (leaving
/// `AwaitingInput`, `Working` to `Idle`) publishes only after
/// [`CONFIRM_CHECKS`] agreeing checks at least [`CONFIRM_SPACING`] apart, or on
/// the first agreeing check [`CONFIRM_CAP`] after the window opened, all read
/// against the same record. A reading taken from the title alone, an overlay,
/// or a screen no rule matches never writes and breaks any open window.
///
/// Beside an `Idle` record whose screen body reads `Idle`, the agent's
/// background rules ([`CompiledRules::read_background`]) set the record's
/// `background` flag after the same window, and clear it after one only when
/// this watcher set it: a flag a hook set, or one set before another writer's
/// change or a status change, is left for its own writer to clear.
///
/// When another writer (a hook event, a chat answer, an exit) changes the
/// record's status, hook stamp or hook-driven flag, the screen is left unread
/// for [`FOREIGN_WRITE_HOLD`], and so it is when the watcher starts (while a
/// reattached session's replay lands), after the output stream lagged, and
/// when a check finds the grid resized; the screen is read again once the
/// hold ends, output or not. The write itself is refused unless the
/// record still holds the status, hook stamp and hook-driven flag the window
/// read (see [`apply_screen_status`]).
///
/// A main `Stop` on this record is held as its `turn_end`, the status staying
/// `Working`, and the watcher alone resolves it: the first settled frame after
/// the hold that shows no sign of hooks lands `Idle` at once, and a frame
/// showing the agent idle lands it through the usual window. A main hook event
/// or a later `Stop` is the other way out. At [`TURN_END_CEILING`], on a
/// settled frame, a spinner drops it and anything else lands `Idle`.
///
/// `attention_tx` receives one event each time the screen moves the session
/// into `AwaitingInput`. `streams.rules` are the screen rules of the session's
/// agent.
pub fn watch(
    registry: &Arc<SessionRegistry>,
    session_id: String,
    streams: AgentStreams,
    attention_tx: mpsc::UnboundedSender<AttentionEvent>,
) {
    let AgentStreams {
        mut output,
        screen,
        rules,
        chat,
    } = streams;
    let sight = Sight {
        screen,
        rules,
        chat,
    };
    let mut watcher = ScreenWatch::new(registry, session_id, sight, attention_tx);
    tokio::spawn(async move { watcher.run_agent(&mut output).await });
}

/// What a check reads the screen for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// An agent session: the screen sets its status.
    Agent,
    /// A plain shell running an agent: the screen sets its status, until
    /// the agent's screen is gone.
    ShellAgent,
    /// A plain shell not running an agent: the screen is read only to spot
    /// one.
    Shell,
}

/// What a check found that its caller acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Checked {
    /// Nothing for the caller to do.
    Done,
    /// The record is gone.
    Gone,
    /// A shell's hook-driven flag moved since the last read.
    HooksFlipped,
    /// A shell's screen showed an agent for a whole window.
    AgentShown,
    /// The agent's screen was gone from a shell for a whole window, and the
    /// shell was handed back to its classifier.
    AgentEnded,
}

/// What a record change seen by the wake asks of the watcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Moved {
    /// A shell's hook-driven flag flipped; the shell watcher acts on it.
    HooksFlipped,
    /// The screen is held off.
    Held,
    /// Nothing: a shell not running an agent writes nothing from its screen.
    Ignored,
}

/// One session's screen check: its inputs, and the state of the check it has
/// scheduled and the change it is confirming. The agent watcher runs it alone;
/// the plain-shell watcher runs it beside its classifier.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags of one check; a state machine would not describe them"
)]
struct ScreenWatch {
    registry: Arc<SessionRegistry>,
    session_id: String,
    screen: SharedScreen,
    attention_tx: mpsc::UnboundedSender<AttentionEvent>,
    /// The screen rules of the session's agent.
    rules: Arc<CompiledRules>,
    /// Where each reading of an agent's screen goes for the chat.
    chat: ChatTap,
    wake: RecordWake,
    confirm: Confirm,
    /// When the last chunk of output arrived; a check at least
    /// [`CHECK_DEBOUNCE`] after it reads a settled frame.
    last_output: Option<Instant>,
    /// The grid's size at the last executed check.
    size: Option<(u16, u16)>,
    /// The verdict of the last executed check.
    last_verdict: Option<Verdict>,
    /// The rule the last executed check matched, kept for the `status_trace`
    /// line a status change writes.
    last_rule: Option<Arc<str>>,
    /// The last executed check read a title rule saying the turn runs.
    title_working: bool,
    /// Whether a dialog on screen may raise `AwaitingInput`: false from the
    /// moment the session enters or leaves a prompt that the screen still
    /// shows, until a check reads anything other than a dialog. A dialog left
    /// painted after a chat answer cleared it raises nothing.
    dialog_edge_armed: bool,
    /// The subagent in view at the last reading of a dialog; `None` for a
    /// dialog in main's view, and once a settled reading shows no dialog.
    subagent_dialog: Option<String>,
    /// Whether the record's background flag is this watcher's own write: the
    /// screen clears only a flag it set. Any other writer's change to the
    /// record, and any status change, drops it.
    screen_set_background: bool,
    /// What the checks have read of the turn end the record holds.
    turn_end: TurnEndWatch,
    /// Takes the terminal back to main for a prompt seen in an agent's view.
    view_return: ViewReturn,
    /// The session's typing lane, held by the view return from the reading
    /// it first acts on until one leaves it idle.
    view_turn: Option<TypingTurn>,
    /// When the view return took the typing lane it holds.
    view_turn_since: Option<Instant>,
    /// The record was seen with no PTY, and that was logged.
    no_pty_logged: bool,
    /// When the view return last typed a key, until a reading is handed to
    /// it again: a reading after new output, or the one at the key's
    /// deadline ([`KEY_WAIT`] after it), which every check re-arms.
    view_key_at: Option<Instant>,
}

/// What a watcher's checks have read of the turn end the record holds.
#[derive(Default)]
struct TurnEndWatch {
    /// The `Stop` these readings belong to.
    event_time: Option<u128>,
    /// A check read the hooks row.
    hooks_seen: bool,
    /// A check read a settled frame.
    read: bool,
}

impl TurnEndWatch {
    /// Start afresh when `event_time` names a different `Stop`.
    fn track(&mut self, event_time: u128) {
        if self.event_time != Some(event_time) {
            *self = Self {
                event_time: Some(event_time),
                ..Self::default()
            };
        }
    }
}

/// What a reading says about a held turn end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnEndSign {
    /// The hooks row.
    Hooks,
    /// A spinner, or the title of a turn that runs.
    Spinner,
    /// The agent idle or asking: the screen's own write resolves it.
    Settled,
    /// An overlay, an unmatched screen, or the title alone.
    Nothing,
}

impl TurnEndSign {
    fn of(reading: &Reading) -> Self {
        match reading.verdict {
            Verdict::HooksRunning => Self::Hooks,
            Verdict::State(ScreenState::Working) => Self::Spinner,
            Verdict::State(_) if reading.from_title => Self::Nothing,
            Verdict::State(ScreenState::Idle | ScreenState::AwaitingInput) => Self::Settled,
            Verdict::Keep | Verdict::Background | Verdict::NoMatch => Self::Nothing,
        }
    }
}

/// What one screen check read from one frame.
struct ScreenRead {
    reading: Reading,
    /// A title rule of the agent's table matches the frame: the title says a
    /// turn runs, whatever rule the body gave.
    title_working: bool,
    /// The background rule that fires on the frame.
    background: Option<Arc<str>>,
    /// The last non-empty row is a shell's prompt.
    prompt: bool,
    /// The plugin's view reading of an agent's screen, taken whichever
    /// conversation is in view and whether or not the view return asks.
    agent_view: Option<ViewReading>,
    /// The plugin's view and chat readings, when asked for.
    view: Option<(ViewReading, Option<ChatScreen>)>,
}

/// When the next check runs, and the change it is confirming.
struct Confirm {
    window: Option<Window>,
    /// The next check scheduled by output or by a hold; `None` when none is
    /// pending.
    check_at: Option<Instant>,
    /// When the current burst of output began, which bounds the debounce.
    burst_from: Option<Instant>,
    /// Checks before this instant are skipped.
    hold_until: Instant,
    /// The record and status the last executed check asked to publish at
    /// once but could not, because output had not settled.
    candidate: Option<(StatusRead, SessionStatus)>,
}

/// What a window confirms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// The status the screen shows.
    Status(SessionStatus),
    /// An agent's screen, in a shell not yet running one.
    AgentShown,
    /// The agent's screen gone, in a shell running an agent.
    AgentGone,
    /// The background flag the screen shows beside `Idle`.
    Background(bool),
}

/// A gated change being confirmed: what it would publish, the record it was
/// read against, and its agreeing checks so far.
struct Window {
    target: Target,
    from: StatusRead,
    agreeing: u8,
    opened: Instant,
    /// When the last check that counted ran.
    counted: Instant,
}

impl Confirm {
    fn new(start: Instant) -> Self {
        Self {
            window: None,
            check_at: None,
            burst_from: None,
            hold_until: start + FOREIGN_WRITE_HOLD,
            candidate: None,
        }
    }

    /// Schedule the check a chunk of output asks for; answers whether it is
    /// already due, which happens once a burst has run past
    /// [`CHECK_MAX_WAIT`].
    fn schedule(&mut self, now: Instant) -> bool {
        let burst_from = *self.burst_from.get_or_insert(now);
        let at = (now + CHECK_DEBOUNCE).min(burst_from + CHECK_MAX_WAIT);
        self.check_at = Some(at);
        at <= now
    }

    /// The next check due: the debounced one, the window's next confirmation,
    /// or a held turn end's `ceiling`, whichever comes first.
    fn next_check(&self, ceiling: Option<Instant>) -> Option<Instant> {
        let confirm = self
            .window
            .as_ref()
            .map(|window| window.counted + CONFIRM_SPACING);
        [self.check_at, confirm, ceiling]
            .into_iter()
            .flatten()
            .min()
    }

    /// Read the screen at `at` at the latest.
    fn read_by(&mut self, at: Instant) {
        self.check_at = Some(self.check_at.map_or(at, |due| due.min(at)));
    }

    /// Drop the window and any candidate, leave the screen unread for
    /// [`FOREIGN_WRITE_HOLD`] from `now`, and read it once the hold ends.
    fn hold(&mut self, now: Instant) {
        self.window = None;
        self.candidate = None;
        self.hold_until = now + FOREIGN_WRITE_HOLD;
        self.check_at = Some(self.hold_until);
    }

    /// Count one check that reads `target` against `read`; answers whether
    /// the gated change publishes now. A check that disagrees with the open
    /// window, or reads a record that moved, opens a new one.
    fn agree(&mut self, read: StatusRead, target: Target, now: Instant) -> bool {
        self.agree_after(read, target, now, false)
    }

    /// [`Self::agree`]; with `no_stop`, the change ends a hook-driven turn no
    /// `Stop` has ended, and publishes only after [`IDLE_WITHOUT_STOP`],
    /// however many checks have counted.
    fn agree_after(
        &mut self,
        read: StatusRead,
        target: Target,
        now: Instant,
        no_stop: bool,
    ) -> bool {
        let due = match self.window.as_mut() {
            Some(window) if window.target == target && window.from == read => {
                if now >= window.counted + CONFIRM_SPACING {
                    window.agreeing += 1;
                    window.counted = now;
                }
                if no_stop {
                    now >= window.opened + IDLE_WITHOUT_STOP
                } else {
                    window.agreeing >= CONFIRM_CHECKS || now >= window.opened + CONFIRM_CAP
                }
            }
            _ => {
                self.window = Some(Window {
                    target,
                    from: read,
                    agreeing: 1,
                    opened: now,
                    counted: now,
                });
                false
            }
        };
        if due {
            self.window = None;
        }
        due
    }
}

impl ScreenWatch {
    /// The next check due. A held turn end's ceiling waits for the frame to
    /// settle and for any hold to end, so a ceiling already past never spins.
    /// An overlay on screen skips it: the check would keep the hold and write
    /// nothing, and the output that closes the overlay asks for a check itself.
    fn next_check(&self) -> Option<Instant> {
        let overlay = self.last_verdict == Some(Verdict::Keep);
        let ceiling = self.wake.ceiling().filter(|_| !overlay).map(|ceiling| {
            let settled = self.last_output.map_or(ceiling, |at| at + CHECK_DEBOUNCE);
            ceiling.max(settled).max(self.confirm.hold_until)
        });
        self.confirm.next_check(ceiling)
    }

    /// Subscribe to the registry before the watcher's task starts, and hold
    /// the screen unread for [`FOREIGN_WRITE_HOLD`] from now.
    fn new(
        registry: &Arc<SessionRegistry>,
        session_id: String,
        sight: Sight,
        attention_tx: mpsc::UnboundedSender<AttentionEvent>,
    ) -> Self {
        let wake = RecordWake::new(registry, &session_id);
        Self {
            registry: Arc::clone(registry),
            session_id,
            screen: sight.screen,
            attention_tx,
            rules: sight.rules,
            chat: sight.chat,
            wake,
            confirm: Confirm::new(Instant::now()),
            last_output: None,
            size: None,
            last_verdict: None,
            last_rule: None,
            title_working: false,
            dialog_edge_armed: true,
            subagent_dialog: None,
            screen_set_background: false,
            turn_end: TurnEndWatch::default(),
            view_return: ViewReturn::default(),
            view_turn: None,
            view_turn_since: None,
            no_pty_logged: false,
            view_key_at: None,
        }
    }

    async fn run_agent(&mut self, output: &mut broadcast::Receiver<Vec<u8>>) {
        loop {
            let next_check = self.next_check();
            // In arm order: output is fed before a due check reads the grid,
            // and a check already due runs inline, since this biased select
            // would otherwise starve the timer arm under continuous output.
            tokio::select! {
                biased;
                msg = output.recv() => match msg {
                    Ok(bytes) => {
                        let now = Instant::now();
                        if self.feed(&bytes, now) && self.check(now, Mode::Agent) == Checked::Gone {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => self.lagged(n),
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                event = self.wake.events.recv(), if self.wake.open => {
                    let before = self.wake.seen;
                    if self.wake.changed(&self.registry, &self.session_id, event) {
                        self.record_moved(before, Mode::Agent, Instant::now());
                    }
                },
                () = sleep_until(next_check.unwrap_or_else(Instant::now)), if next_check.is_some() => {
                    if self.check(Instant::now(), Mode::Agent) == Checked::Gone {
                        return;
                    }
                }
            }
        }
    }

    /// Feed one chunk of output into the grid and schedule the check it asks
    /// for; answers whether that check is already due.
    fn feed(&mut self, bytes: &[u8], now: Instant) -> bool {
        crate::sync::lock(&self.screen).feed(bytes);
        self.last_output = Some(now);
        self.confirm.schedule(now)
    }

    /// The output stream skipped `n` chunks: the grid may hold a cut escape
    /// sequence and a frame missing its rows.
    fn lagged(&mut self, n: u64) {
        warn!(session_id = %self.session_id, lagged = n, "pty state lagged");
        crate::sync::lock(&self.screen).reset_parser();
        self.confirm.hold(Instant::now());
    }

    /// One check of the screen; then, while a view key is outstanding, the
    /// next read is due no later than the key's deadline, so a check that
    /// handed no reading to the view return never drops the read the key is
    /// owed.
    fn check(&mut self, now: Instant, mode: Mode) -> Checked {
        self.mind_view_turn(now, mode);
        let checked = self.check_frame(now, mode);
        if let Some(deadline) = self
            .view_key_at
            .map(|at| at + KEY_WAIT)
            .filter(|&deadline| deadline > now)
        {
            self.confirm.read_by(deadline);
        }
        checked
    }

    /// Read the record, then the screen, and act on what the screen shows
    /// when the rules allow it.
    fn check_frame(&mut self, now: Instant, mode: Mode) -> Checked {
        let (read, quiet) = match self.read_record(now, mode) {
            Ok(found) => found,
            Err(checked) => return checked,
        };
        // A frame read after a view key but before any output came is the
        // frame the key was pressed on: it is read again only at KEY_WAIT.
        let view_due = self.view_key_at.is_none_or(|at| {
            self.last_output.is_some_and(|output| output > at) || now >= at + KEY_WAIT
        });
        let view_agent = if mode == Mode::Shell {
            None
        } else {
            self.session_agent()
        };
        let target = if quiet && view_due && mode != Mode::Shell {
            self.view_target()
        } else {
            None
        };
        let Some(ScreenRead {
            mut reading,
            title_working,
            mut background,
            prompt,
            agent_view,
            view,
        }) = self.read_screen(mode, view_agent.as_ref(), target.is_some())
        else {
            self.confirm.hold(now);
            return Checked::Done;
        };
        // Another agent's conversation in view is an overlay: the screen shows
        // that agent's turn, not the main one's, so it writes neither the
        // status nor the background flag, and the windows it would confirm
        // reset. A dialog is the one exception: either agent's dialog needs
        // the user, and the view return never goes back to main while one is
        // up. A held turn end lands through its own path only while no hooks
        // row was read first; one still held at `TURN_END_CEILING` stays held
        // while this overlay is up, and `turn_end_ceiling` writes no `Idle`.
        if agent_view
            .as_ref()
            .is_some_and(|view| view.agent_in_view.is_some())
        {
            if reading.verdict != Verdict::State(ScreenState::AwaitingInput) {
                reading.verdict = Verdict::Keep;
                reading.rule = Some(Arc::from("agent_in_view"));
            }
            background = None;
        }
        self.follow_subagent_dialog(read, &reading, agent_view.as_ref(), quiet);
        if let (Some((pty, agent)), Some((view, chat))) = (&target, &view) {
            self.drive_view(pty, agent, view, chat.as_ref(), now);
        }
        debug!(
            session_id = %self.session_id,
            rule = ?reading.rule,
            verdict = ?reading.verdict,
            from_title = reading.from_title,
            title_working,
            background = ?background,
            prompt,
            status = ?read.status,
            ?mode,
            quiet,
            "pty_state screen check"
        );
        self.last_verdict = Some(reading.verdict);
        self.last_rule.clone_from(&reading.rule);
        self.title_working = title_working;
        if reading.verdict != Verdict::State(ScreenState::AwaitingInput) {
            self.dialog_edge_armed = true;
        }
        if self.turn_end_ceiling(read, &reading, background.is_some(), quiet, now) {
            return Checked::Done;
        }
        match mode {
            Mode::Shell => self.watch_for_agent(read, shows_agent(&reading) && !prompt, quiet, now),
            Mode::ShellAgent if prompt => self.watch_for_agent_end(read, now),
            Mode::Agent | Mode::ShellAgent
                if self.follow_turn_end(read, &reading, background.is_some(), quiet) =>
            {
                Checked::Done
            }
            Mode::ShellAgent if !shows_agent(&reading) => {
                // An agent screen no rule knows, or the agent's title alone, with
                // the shell's prompt not back: the agent may still run. Nothing
                // writes, and any open window waits for the next check.
                self.confirm.candidate = None;
                Checked::Done
            }
            Mode::Agent | Mode::ShellAgent => {
                self.follow_screen(read, &reading, background.is_some(), quiet, now);
                Checked::Done
            }
        }
    }

    /// End the prompt of a dialog a subagent's view has stopped showing. A
    /// subagent's hooks leave a pending prompt alone and its view writes no
    /// status, so this edge is what ends it: the last reading in this same
    /// view was a dialog, and this settled one is not. A half-drawn frame
    /// counts for nothing, and any other non-dialog reading ends nothing: the
    /// view moved, and a dialog out of sight may still be up. Only the rules
    /// start the edge, since the plugin's view also reads an overlay as a
    /// dialog, and a reading the plugin still calls a dialog keeps it: a
    /// footer no rule knows, its dialog still up. A clear refused because the
    /// record moved since `read` keeps the edge for the next reading.
    fn follow_subagent_dialog(
        &mut self,
        read: StatusRead,
        reading: &Reading,
        agent_view: Option<&ViewReading>,
        quiet: bool,
    ) {
        let in_view = agent_view.and_then(|view| view.agent_in_view.as_deref());
        if reads_dialog(reading) {
            self.subagent_dialog = in_view.map(str::to_owned);
            return;
        }
        if !quiet {
            return;
        }
        if in_view.is_none() || self.subagent_dialog.as_deref() != in_view {
            self.subagent_dialog = None;
            return;
        }
        if agent_view.is_some_and(|view| view.dialog) {
            return;
        }
        let mut refused = false;
        self.registry
            .clear_answered_prompt_if(&self.session_id, |record| {
                refused = moved_since(record, read);
                !refused
            });
        if !refused {
            self.subagent_dialog = None;
        }
    }

    /// Note a dialog on the grid in a subagent's view while a hold keeps the
    /// screen from being read, so one answered inside the hold still has its
    /// edge. Anything else on the grid changes nothing until a reading.
    fn note_held_off_dialog(&mut self) {
        let Some(agent) = self.session_agent() else {
            return;
        };
        let screen = crate::sync::lock(&self.screen);
        let Some(view) = crate::screen::dialogs::read_view(&agent, &screen) else {
            return;
        };
        if view.agent_in_view.is_some() && reads_dialog(&self.rules.read(&screen)) {
            self.subagent_dialog = view.agent_in_view;
        }
    }

    /// The first half of a check: the record as it stands, and whether the
    /// output has settled. `Err` carries what ends the check before the screen
    /// is read: a hold, a gone or finished record, or a record another writer
    /// moved since the last read.
    fn read_record(&mut self, now: Instant, mode: Mode) -> Result<(StatusRead, bool), Checked> {
        self.confirm.check_at = None;
        // Output since the last chunk's debounce: the frame may be half drawn,
        // and the burst goes on, so the next max-wait counts from here.
        let settled_at = self.last_output.map(|at| at + CHECK_DEBOUNCE);
        let quiet = settled_at.is_none_or(|at| now >= at);
        self.confirm.burst_from = (!quiet).then_some(now);
        if now < self.confirm.hold_until {
            self.confirm.check_at = Some(self.confirm.hold_until);
            if mode != Mode::Shell {
                self.note_held_off_dialog();
            }
            return Err(Checked::Done);
        }
        let Some(read) = record_status(&self.registry, &self.session_id) else {
            return Err(Checked::Gone);
        };
        self.note_read(read, mode, now)?;
        if matches!(read.status, SessionStatus::Stopped | SessionStatus::Error) {
            self.confirm.window = None;
            self.confirm.candidate = None;
            return Err(Checked::Done);
        }
        if !quiet {
            // Read the frame again once it settles, should no output follow.
            self.confirm.check_at = settled_at;
        }
        Ok((read, quiet))
    }

    /// Take `read` as the record last seen. `Err` carries what ends the check:
    /// another writer changed the record and its broadcast has not arrived
    /// yet, so the grid predates that change too.
    fn note_read(&mut self, read: StatusRead, mode: Mode, now: Instant) -> Result<(), Checked> {
        let before = self.wake.seen;
        if !self.wake.note(read) {
            return Ok(());
        }
        match self.record_moved(before, mode, now) {
            Moved::HooksFlipped => Err(Checked::HooksFlipped),
            Moved::Held => Err(Checked::Done),
            Moved::Ignored => Ok(()),
        }
    }

    /// Resolve a held turn end that has reached [`TURN_END_CEILING`], on a
    /// settled frame only: a half-drawn spinner must not land a false `Idle`.
    /// A spinner or a working title drops it: no status is written. An overlay
    /// hides what the turn is doing, so it keeps the hold and writes nothing,
    /// and the reading after the overlay resolves it, or a weak idle lands it
    /// with its flag; meanwhile the check goes on, so a shell's prompt can
    /// still hand the shell back. Anything else lands `Idle`, with the flag
    /// the frame shows (`shown`). Answers whether that decided the check.
    fn turn_end_ceiling(
        &mut self,
        read: StatusRead,
        reading: &Reading,
        shown: bool,
        quiet: bool,
        now: Instant,
    ) -> bool {
        if !quiet
            || read.held.is_none_or(|held| now < held.ceiling)
            || reading.verdict == Verdict::Keep
        {
            return false;
        }
        self.confirm.window = None;
        self.confirm.candidate = None;
        if TurnEndSign::of(reading) != TurnEndSign::Spinner {
            self.publish_as(read, SessionStatus::Idle, "ceiling", shown);
            return true;
        }
        if clear_turn_end(&self.registry, &self.session_id, read) {
            // This watcher's own write, so its broadcast starts no hold.
            self.wake.seen = Some(StatusRead { held: None, ..read });
            info!(session_id = %self.session_id, why = "ceiling_spinner", "turn end dropped");
        }
        true
    }

    /// Follow a held turn end through one reading; answers whether that
    /// decided the check. Otherwise the reading goes the usual way, which
    /// writes nothing but what an idle or asking screen publishes. A spinner
    /// decides nothing: a blocked turn resolves by its next main hook event,
    /// and one between the hooks row and the idle box must keep what is held.
    fn follow_turn_end(
        &mut self,
        read: StatusRead,
        reading: &Reading,
        shown: bool,
        quiet: bool,
    ) -> bool {
        let Some(held) = read.held else {
            self.turn_end = TurnEndWatch::default();
            return false;
        };
        self.turn_end.track(held.event_time);
        let sign = TurnEndSign::of(reading);
        let first_read = quiet && !self.turn_end.read;
        // A working title may outlive the turn: it uses up no read.
        self.turn_end.read |= quiet && !(reading.from_title && sign == TurnEndSign::Spinner);
        self.turn_end.hooks_seen |= sign == TurnEndSign::Hooks;
        if sign != TurnEndSign::Nothing || !first_read || self.turn_end.hooks_seen {
            return false;
        }
        self.confirm.window = None;
        self.confirm.candidate = None;
        self.publish_as(read, SessionStatus::Idle, "no_evidence", shown);
        true
    }

    /// The screen's reading, the background rule that fires on it, and
    /// whether its last non-empty row is a shell's prompt; `None` when the
    /// grid's size changed since the last executed check: the frame under the
    /// new size may be half drawn. A frame that is read, of an agent's
    /// screen (`mode` not [`Mode::Shell`]), goes to the chat mirror too.
    /// With `view_agent`, the same frame's view reading by that agent's
    /// plugin comes back too, and with `view_wanted` its chat reading beside
    /// it.
    fn read_screen(
        &mut self,
        mode: Mode,
        view_agent: Option<&AgentId>,
        view_wanted: bool,
    ) -> Option<ScreenRead> {
        let (size, resized, read) = {
            let screen = crate::sync::lock(&self.screen);
            let size = screen.size();
            let resized = self.size.is_some_and(|last| last != size);
            if !resized && mode != Mode::Shell {
                self.chat.observe(&screen);
            }
            let wanted = view_agent.filter(|_| !resized);
            let agent_view =
                wanted.and_then(|agent| crate::screen::dialogs::read_view(agent, &screen));
            let view = if view_wanted {
                wanted
                    .zip(agent_view.clone())
                    .map(|(agent, view)| (view, crate::screen::dialogs::read(agent, &screen)))
            } else {
                None
            };
            let read = ScreenRead {
                reading: self.rules.read(&screen),
                title_working: self.rules.title_reads_working(&screen),
                background: self.rules.read_background(&screen),
                prompt: rules::last_row_is_shell_prompt(&screen),
                agent_view,
                view,
            };
            (size, resized, read)
        };
        self.size = Some(size);
        (!resized).then_some(read)
    }

    /// Give back the typing lane when the session runs no agent, and when the
    /// view return has held it longer than [`VIEW_TURN_LIMIT`]: then its
    /// switch is abandoned too.
    fn mind_view_turn(&mut self, now: Instant, mode: Mode) {
        if mode == Mode::Shell {
            self.release_view_turn();
            return;
        }
        let Some(since) = self.view_turn_since else {
            return;
        };
        if now.duration_since(since) > VIEW_TURN_LIMIT {
            self.view_return.abandon();
            self.release_view_turn();
            warn!(
                session_id = %self.session_id,
                "the view switch held the typing lane too long; it is released"
            );
        }
    }

    fn release_view_turn(&mut self) {
        self.view_turn = None;
        self.view_turn_since = None;
    }

    /// The agent whose plugin reads this session's screen; `None` when the
    /// record is gone.
    fn session_agent(&self) -> Option<AgentId> {
        let arc = self.registry.get(&self.session_id)?;
        let agent = crate::sync::lock(&arc).agent.clone();
        Some(agent)
    }

    /// The session's PTY and agent, for the view return to type into and
    /// read with; `None` when the record is gone or has no PTY (logged
    /// once). Either way the view return gives back the typing lane.
    fn view_target(&mut self) -> Option<(Arc<PtyHandle>, AgentId)> {
        let Some(arc) = self.registry.get(&self.session_id) else {
            self.release_view_turn();
            return None;
        };
        let (pty, agent) = {
            let record = crate::sync::lock(&arc);
            (record.pty.clone(), record.agent.clone())
        };
        let Some(pty) = pty else {
            self.release_view_turn();
            if !self.no_pty_logged {
                self.no_pty_logged = true;
                info!(
                    session_id = %self.session_id,
                    "the session has no PTY; its terminal view is never switched"
                );
            }
            return None;
        };
        Some((pty, agent))
    }

    /// Hand one settled reading to the view return and carry out what it
    /// answers: its keys go to the PTY (never as the owner's input), its
    /// notices to the clients. After a key the screen is read again
    /// [`KEY_WAIT`] later even when no output comes.
    ///
    /// The view return acts only while it holds the session's typing lane:
    /// with another writer in it, the reading is skipped and the screen read
    /// again [`LANE_RETRY`] later. The lane is given back after a reading
    /// that pressed nothing and leaves no switch under way.
    fn drive_view(
        &mut self,
        pty: &PtyHandle,
        agent: &AgentId,
        view: &ViewReading,
        chat: Option<&ChatScreen>,
        now: Instant,
    ) {
        if self.view_turn.is_none() {
            let Some(turn) = pty.typing().try_take(Holder::ViewReturn) else {
                debug!(
                    session_id = %self.session_id,
                    holder = ?pty.typing().holder(),
                    "another writer holds the typing lane; the view return waits"
                );
                self.confirm.read_by(now + LANE_RETRY);
                return;
            };
            self.view_turn = Some(turn);
            self.view_turn_since = Some(now);
        }
        let mut pressed = false;
        for action in self.view_return.on_reading(agent, view, chat, now) {
            match action {
                ReturnAction::Press(key) => {
                    debug!(session_id = %self.session_id, ?key, "view return key");
                    if let Some(turn) = &self.view_turn {
                        turn.press(pty, key.to_bytes());
                    }
                    pressed = true;
                }
                ReturnAction::Notice(notice) => {
                    self.registry.announce_notice(&self.session_id, notice);
                }
                ReturnAction::Log(text) => info!(session_id = %self.session_id, "{text}"),
                ReturnAction::Warn(text) => warn!(session_id = %self.session_id, "{text}"),
            }
        }
        self.view_key_at = pressed.then_some(now);
        if !pressed && !self.view_return.busy() {
            self.release_view_turn();
        }
    }

    /// Publish the status the screen shows, at once or after its window;
    /// with no status to publish, follow the background work it shows
    /// (`background`).
    fn follow_screen(
        &mut self,
        read: StatusRead,
        reading: &Reading,
        background: bool,
        quiet: bool,
        now: Instant,
    ) {
        let Some(target) = screen_target(reading).filter(|&target| target != read.status) else {
            self.confirm.candidate = None;
            self.follow_background(read, reading, background, now);
            return;
        };
        if publishes_at_once(read.status, target) {
            self.publish_at_once(read, target, quiet);
        } else {
            self.confirm.candidate = None;
            let no_stop = Self::turn_without_stop(read, reading, target);
            if no_stop && self.title_holds_turn(now) {
                self.confirm.window = None;
                return;
            }
            if self
                .confirm
                .agree_after(read, Target::Status(target), now, no_stop)
            {
                // The frame that publishes carries its own background flag.
                self.publish_as(read, target, "screen", background);
            }
        }
    }

    /// Whether `target` ends a hook-driven turn that no `Stop` has ended, and
    /// so waits for [`IDLE_WITHOUT_STOP`] before it publishes: a turn whose
    /// `Stop` was lost. An interrupt reads as its own end, so it keeps the
    /// short window.
    fn turn_without_stop(read: StatusRead, reading: &Reading, target: SessionStatus) -> bool {
        target == SessionStatus::Idle
            && read.status == SessionStatus::Working
            && read.hook_driven
            && read.held.is_none()
            && reading.rule.as_deref() != Some(INTERRUPT_RULE)
    }

    /// Whether a working title still holds a turn no `Stop` has ended: true
    /// until the terminal has been quiet for [`TITLE_QUIET_LIMIT`], when the
    /// check it schedules lets the idle screen's window open.
    fn title_holds_turn(&mut self, now: Instant) -> bool {
        let Some(limit) = self
            .last_output
            .map(|at| at + TITLE_QUIET_LIMIT)
            .filter(|&limit| self.title_working && now < limit)
        else {
            return false;
        };
        self.confirm.read_by(limit);
        true
    }

    /// Set the record's background flag after a window of checks showing
    /// background work (`shown`), or clear one this watcher set after a window
    /// showing none. Only an `Idle` record whose screen body reads `Idle`
    /// counts; anything else breaks the window.
    fn follow_background(
        &mut self,
        read: StatusRead,
        reading: &Reading,
        shown: bool,
        now: Instant,
    ) {
        let Some(on) = self.background_target(read, reading, shown) else {
            self.confirm.window = None;
            return;
        };
        if self.confirm.agree(read, Target::Background(on), now)
            && apply_screen_background(&self.registry, &self.session_id, read, on)
        {
            self.screen_set_background = on;
        }
    }

    /// The background flag the screen asks the record for, or `None` when it
    /// asks for no change: the record or the screen body is not `Idle`, the
    /// flag already says what the screen shows, or the screen shows none and
    /// the flag is another writer's.
    fn background_target(&self, read: StatusRead, reading: &Reading, shown: bool) -> Option<bool> {
        let idle_body = reading.verdict == Verdict::State(ScreenState::Idle) && !reading.from_title;
        if !idle_body || read.status != SessionStatus::Idle {
            return None;
        }
        match (shown, record_background(&self.registry, &self.session_id)?) {
            (true, false) => Some(true),
            (false, true) if self.screen_set_background => Some(false),
            _ => None,
        }
    }

    /// In a shell not running an agent, count a settled check whose screen
    /// shows an agent with no shell prompt under it (`shown`); answers
    /// [`Checked::AgentShown`] once a whole window has. A check inside a
    /// burst of output neither counts nor breaks the window.
    fn watch_for_agent(
        &mut self,
        read: StatusRead,
        shown: bool,
        quiet: bool,
        now: Instant,
    ) -> Checked {
        if !quiet {
            return Checked::Done;
        }
        self.confirm.candidate = None;
        if !shown {
            self.confirm.window = None;
            return Checked::Done;
        }
        if self.confirm.agree(read, Target::AgentShown, now) {
            Checked::AgentShown
        } else {
            Checked::Done
        }
    }

    /// In a shell running an agent, count a check whose last row is the
    /// shell's prompt; once a whole window has, hand the shell back to its classifier
    /// (see [`hand_shell_back`]) and answer [`Checked::AgentEnded`]. A write
    /// that lands is this watcher's own, so its broadcast starts no hold.
    fn watch_for_agent_end(&mut self, read: StatusRead, now: Instant) -> Checked {
        self.confirm.candidate = None;
        if !self.confirm.agree(read, Target::AgentGone, now) {
            return Checked::Done;
        }
        match hand_shell_back(&self.registry, &self.session_id, read) {
            Some(after) => {
                self.wake.seen = Some(after);
                self.screen_set_background = false;
                Checked::AgentEnded
            }
            None => Checked::Done,
        }
    }

    /// Publish a change that needs no window, once the frame is settled: read
    /// after output paused for [`CHECK_DEBOUNCE`], or by two executed checks
    /// in a row. Entering `AwaitingInput` also needs the dialog edge armed.
    fn publish_at_once(&mut self, read: StatusRead, target: SessionStatus, quiet: bool) {
        self.confirm.window = None;
        if target == SessionStatus::AwaitingInput && !self.dialog_edge_armed {
            self.confirm.candidate = None;
            return;
        }
        if quiet || self.confirm.candidate == Some((read, target)) {
            self.confirm.candidate = None;
            self.publish_as(read, target, "screen", false);
        } else {
            self.confirm.candidate = Some((read, target));
        }
    }

    /// Write `next` over `from`, with `shown` the background reading of the
    /// frame that asks for it, logging a turn end's landing as `how`; answers
    /// whether it wrote. A write that lands is this watcher's own, so its
    /// broadcast starts no hold; the flag written beside `Idle` is the screen's
    /// own only when no held turn end also reported background work.
    fn publish_as(
        &mut self,
        from: StatusRead,
        next: SessionStatus,
        how: &'static str,
        shown: bool,
    ) -> bool {
        let wrote = apply_screen_status(
            &self.registry,
            &self.session_id,
            from,
            ScreenWrite { next, shown },
            &self.attention_tx,
        );
        if wrote {
            debug!(
                target: "status_trace",
                session = %self.session_id,
                from = ?from.status,
                to = ?next,
                rule = ?self.last_rule,
                how,
                "status changed by a screen read"
            );
            // Leaving `AwaitingInput` cleared the prompt with the write
            // (`apply_watcher_status`), which is this watcher's own too.
            let left_prompt =
                from.status == SessionStatus::AwaitingInput && next != SessionStatus::AwaitingInput;
            self.wake.seen = Some(StatusRead {
                status: next,
                pending: from.pending && !left_prompt,
                held: None,
                ..from
            });
            self.screen_set_background = next == SessionStatus::Idle
                && shown
                && !from.held.is_some_and(|held| held.background);
            if from.held.is_some() && next == SessionStatus::Idle {
                info!(session_id = %self.session_id, how, "turn end landed");
            }
            if next == SessionStatus::AwaitingInput {
                self.dialog_edge_armed = false;
            }
        }
        wrote
    }

    /// The record moved from `before` to what the wake now holds. A shell's
    /// hook-driven flip is left to the shell watcher; a shell not running
    /// an agent ignores any other move; otherwise the screen is held off.
    /// Whichever, a background flag the screen set is no longer its own.
    fn record_moved(&mut self, before: Option<StatusRead>, mode: Mode, now: Instant) -> Moved {
        self.screen_set_background = false;
        let hooks = |read: Option<StatusRead>| read.is_some_and(|read| read.hook_driven);
        if mode != Mode::Agent && hooks(before) != hooks(self.wake.seen) {
            return Moved::HooksFlipped;
        }
        if mode == Mode::Shell {
            return Moved::Ignored;
        }
        self.foreign_write(before, now);
        Moved::Held
    }

    /// Another writer moved the record from `before` to what the wake last
    /// saw: hold off the screen, which drops any check already scheduled, so
    /// a subagent's dialog on the grid is noted now. A move off
    /// `AwaitingInput` while the screen showed the dialog (a chat answer)
    /// disarms the dialog edge, since the dialog stays painted until the
    /// agent redraws.
    fn foreign_write(&mut self, before: Option<StatusRead>, now: Instant) {
        self.note_held_off_dialog();
        let awaiting = |read: Option<StatusRead>| {
            read.is_some_and(|read| read.status == SessionStatus::AwaitingInput)
        };
        let dialog_shown = self.last_verdict == Some(Verdict::State(ScreenState::AwaitingInput));
        if awaiting(before) && !awaiting(self.wake.seen) && dialog_shown {
            self.dialog_edge_armed = false;
        }
        self.confirm.hold(now);
    }
}

/// Whether the rules read a dialog.
fn reads_dialog(reading: &Reading) -> bool {
    reading.verdict == Verdict::State(ScreenState::AwaitingInput)
}

/// The status a screen reading asks for, or `None` when it asks for none: an
/// overlay, an unmatched screen, or a state read from the title alone, which
/// never writes.
fn screen_target(reading: &Reading) -> Option<SessionStatus> {
    match reading.verdict {
        Verdict::State(_) if reading.from_title => None,
        Verdict::State(ScreenState::Working) => Some(SessionStatus::Working),
        Verdict::State(ScreenState::AwaitingInput) => Some(SessionStatus::AwaitingInput),
        Verdict::State(ScreenState::Idle) => Some(SessionStatus::Idle),
        Verdict::Keep | Verdict::HooksRunning | Verdict::Background | Verdict::NoMatch => None,
    }
}

/// Whether a reading comes from the agent's screen: a rule over the screen body
/// matched, a state or an overlay. A title alone, which outlives the CLI that
/// set it, does not count.
fn shows_agent(reading: &Reading) -> bool {
    !reading.from_title && reading.verdict != Verdict::NoMatch
}

/// Whether the screen's change from `from` to `to` publishes on one check:
/// a spinner promoting `Idle`, and a dialog raising `AwaitingInput`. Every
/// other change waits for its window.
fn publishes_at_once(from: SessionStatus, to: SessionStatus) -> bool {
    matches!(
        (from, to),
        (SessionStatus::Idle, SessionStatus::Working)
            | (
                SessionStatus::Idle | SessionStatus::Working,
                SessionStatus::AwaitingInput
            )
    )
}

/// Whether `record` moved since `from` was read (see [`StatusRead`]'s
/// equality), or is stopped or failed: either refuses a screen write.
fn moved_since(record: &SessionRecord, from: StatusRead) -> bool {
    status_read(record) != from
        || matches!(record.status, SessionStatus::Stopped | SessionStatus::Error)
}

/// A status the screen asks for, and the background reading of the frame that
/// asks for it.
#[derive(Clone, Copy)]
struct ScreenWrite {
    next: SessionStatus,
    shown: bool,
}

/// Write the status the screen shows, refused when the record moved since
/// `from` was read (see [`moved_since`]; under a held turn end its `Stop`
/// stands in for the hook stamp) or is stopped or failed; answers whether it
/// wrote.
///
/// A hook event, a chat answer or an exit that landed after `from` was read
/// wins. A refused write neither broadcasts nor touches the sidecar. A write
/// that lands goes through [`apply_watcher_status`], so it clears
/// `background` and, leaving `AwaitingInput`, the pending prompt; it never
/// writes `hook_driven` or the hook stamp. On entry to `AwaitingInput` the
/// one [`AttentionEvent`] is sent after the registry update returns.
///
/// A write of `Idle` carries the background work `write.shown` on the frame that
/// asks for it and consumes a held turn end in the same update: either's flag
/// lands beside the status, since a broadcast of `Idle` without it would arm
/// a client's Finished hold, and the alert follows that update's broadcast.
fn apply_screen_status(
    registry: &Arc<SessionRegistry>,
    session_id: &str,
    from: StatusRead,
    write: ScreenWrite,
    attention_tx: &mpsc::UnboundedSender<AttentionEvent>,
) -> bool {
    let ScreenWrite { next, shown } = write;
    let mut wrote = false;
    let mut entered_awaiting = false;
    registry.update_if_changed_then(session_id, |rec| {
        if moved_since(rec, from) {
            return None;
        }
        let held = rec.turn_end.take_if(|_| next == SessionStatus::Idle);
        entered_awaiting = apply_watcher_status(rec, next);
        wrote = rec.status == next;
        if !wrote {
            return None;
        }
        if next == SessionStatus::Idle {
            rec.background = shown || held.as_ref().is_some_and(|held| held.background);
        }
        Some(
            held.and_then(|held| held.alert)
                .map(|alert| alert.event(session_id)),
        )
    });
    if entered_awaiting {
        announce_awaiting_input(session_id, attention_tx);
    }
    wrote
}

/// Forget the turn end `from` held, refused unless the record still holds
/// what `from` read; answers whether it wrote. Only the turn end changes, so
/// the update raises no Attention and leaves the status as it stands.
fn clear_turn_end(registry: &Arc<SessionRegistry>, session_id: &str, from: StatusRead) -> bool {
    let mut wrote = false;
    registry.update_if_changed(session_id, |rec| {
        if moved_since(rec, from) {
            return false;
        }
        rec.turn_end = None;
        wrote = true;
        true
    });
    wrote
}

/// Write the background flag `on` the screen shows, refused unless the record
/// still holds the status, hook stamp and hook-driven flag of `from` and is
/// `Idle`; answers whether it wrote.
///
/// Only the flag changes: never the status or its time, the hook stamp,
/// `hook_driven` or the pending prompt, so it raises no Attention, and it never
/// reaches the sidecar, which holds no background flag.
fn apply_screen_background(
    registry: &Arc<SessionRegistry>,
    session_id: &str,
    from: StatusRead,
    on: bool,
) -> bool {
    let mut wrote = false;
    registry.update_if_changed(session_id, |rec| {
        if moved_since(rec, from) || rec.status != SessionStatus::Idle || rec.background == on {
            return false;
        }
        rec.background = on;
        wrote = true;
        true
    });
    wrote
}

/// The record's background flag; `None` once the record is gone.
fn record_background(registry: &Arc<SessionRegistry>, session_id: &str) -> Option<bool> {
    let arc = registry.get(session_id)?;
    let background = crate::sync::lock(&arc).background;
    Some(background)
}

/// Hand a shell whose agent has ended back to its own classifier: clear
/// `hook_driven` and `background` (the agent's background tasks ended with it),
/// and end a `Working` or `AwaitingInput` turn at `Idle` through
/// [`apply_watcher_status`] (an `Idle` stays as it is). Refused, like
/// [`apply_screen_status`], unless the record still holds what `from` read;
/// answers the record as written, or `None` when refused. It is the only
/// screen write of `hook_driven`, and it never writes the hook stamp. An agent
/// typed into its shell at spawn whose `SessionEnd` never came ends here, and
/// its end is announced once the update is broadcast. A held turn end's alert
/// follows that update; its background flag ended with the agent.
fn hand_shell_back(
    registry: &Arc<SessionRegistry>,
    session_id: &str,
    from: StatusRead,
) -> Option<StatusRead> {
    let mut after = None;
    let mut agent_exited = false;
    let mut promoted = false;
    registry.update_if_changed_then(session_id, |rec| {
        if moved_since(rec, from) {
            return None;
        }
        let in_turn = matches!(
            rec.status,
            SessionStatus::Working | SessionStatus::AwaitingInput
        );
        agent_exited = rec.end_agent_in_shell();
        if agent_exited {
            // The title the agent painted named it, not the shell it ran in;
            // one the shell set for itself is kept.
            crate::osc_title::clear_agent_title(&mut rec.terminal_title, &rec.agent);
        }
        promoted = rec.promoted_shell();
        let changed = rec.hook_driven || rec.background || in_turn || agent_exited;
        rec.hook_driven = false;
        rec.background = false;
        let held = rec.turn_end.take();
        if held.is_some() {
            info!(session_id, how = "hand_back", "turn end landed");
        }
        if in_turn {
            apply_watcher_status(rec, SessionStatus::Idle);
        }
        after = Some(StatusRead {
            status: rec.status,
            hook_driven: false,
            main_hook_time: rec.main_hook_time,
            pending: rec.pending_input.is_some(),
            held: None,
        });
        let alert = held.and_then(|held| held.alert);
        changed.then(|| alert.map(|alert| alert.event(session_id)))
    });
    if agent_exited {
        // A promoted shell's waiter lives only while its agent runs.
        if promoted {
            registry.discovery().deregister(session_id);
        }
        registry.announce_agent_exited(session_id);
    }
    after
}

/// The fields a screen sighting of an agent may change on a shell's record.
#[derive(PartialEq, Eq)]
struct SightingFields {
    agent_in_shell: bool,
    shell_label: Option<String>,
    hook_agent: Option<protocol::AgentId>,
    program_name: Option<String>,
    conversation_id: Option<String>,
    agent_exit_announced: bool,
}

fn sighting_fields(rec: &SessionRecord) -> SightingFields {
    SightingFields {
        agent_in_shell: rec.agent_in_shell,
        shell_label: rec.shell_label.clone(),
        hook_agent: rec.hook_agent.clone(),
        program_name: rec.program_name.clone(),
        conversation_id: rec.conversation_id.clone(),
        agent_exit_announced: rec.agent_exit_announced,
    }
}

/// An agent is running in session `session_id`'s shell: a plain shell is
/// promoted to one running that agent, or a promoted one starts its run
/// again, in one registry update (see
/// [`crate::session::SessionRecord::agent_shown_in_plain_shell`]), and what
/// happened is answered. No conversation is looked for here: the screen path
/// starts its waiter after this, and the hook path needs none, since its
/// `SessionStart` names the conversation.
pub(crate) fn promote_plain_shell(registry: &Arc<SessionRegistry>, session_id: &str) -> ShownAgent {
    let mut shown = ShownAgent::Ignored;
    // A sighting that changes no field (a running agent seen again) is
    // neither broadcast nor written to the sidecar.
    registry.update_if_changed(session_id, |rec| {
        let before = sighting_fields(rec);
        shown = rec.agent_shown_in_plain_shell();
        sighting_fields(rec) != before
    });
    shown
}

/// The screen showed an agent in session `session_id`'s shell for a whole
/// window: a plain shell is promoted to one running that agent, or a promoted
/// one starts its run again, in one registry update (see
/// [`crate::session::SessionRecord::agent_shown_in_plain_shell`]), and its
/// conversation is then looked for from its transcripts. An agent session's
/// record is left to its hooks.
fn agent_shown_in_shell(registry: &Arc<SessionRegistry>, session_id: &str) {
    let shown_at = chrono::Utc::now();
    let shown = promote_plain_shell(registry, session_id);
    if shown == ShownAgent::Ignored {
        return;
    }
    info!(
        session_id,
        ?shown,
        "plain shell: the screen shows an agent running in it"
    );
    let previous = match shown {
        ShownAgent::Restarted { previous } => previous,
        _ => None,
    };
    crate::conversations::discovery::watch_promoted_shell(registry, session_id, shown_at, previous);
}

/// How far the agent typed into a session's shell has come, for the chain
/// that holds its first prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentReading {
    /// No screen of the agent's that takes keys yet; `shell_prompt` when the
    /// shell's prompt is on the last row, which is before the agent started
    /// or after it exited.
    Starting { shell_prompt: bool },
    /// A dialog holds the keys: the rules read the screen as waiting for
    /// input, or the composer reads a dialog or an overlay.
    Dialog,
    /// The composer reads ready: the agent's box is empty and takes text.
    Ready,
}

/// How far the agent typed into session `session_id`'s shell has come, read
/// from the record's screen with the screen rules and the composer reading of
/// the record's agent. A record with no screen reads ready once its hooks
/// drive it. A session that is gone, stopped or failed reads as the shell's
/// prompt back, so a chain holding its first prompt drops it.
pub fn agent_reading(registry: &SessionRegistry, session_id: &str) -> AgentReading {
    let ended = AgentReading::Starting { shell_prompt: true };
    let Some(arc) = registry.get(session_id) else {
        return ended;
    };
    let (status, hooks, screen, agent) = {
        let record = crate::sync::lock(&arc);
        (
            record.status,
            record.hook_driven,
            record.screen.clone(),
            record.agent.clone(),
        )
    };
    if matches!(status, SessionStatus::Stopped | SessionStatus::Error) {
        return ended;
    }
    match screen {
        Some(screen) => read_screen(&agent, &crate::sync::lock(&screen)),
        None if hooks => AgentReading::Ready,
        None => AgentReading::Starting {
            shell_prompt: false,
        },
    }
}

/// [`agent_reading`] of `screen`, as `agent`'s plugin reads it.
fn read_screen(agent: &AgentId, screen: &crate::screen::Screen) -> AgentReading {
    if rules::last_row_is_shell_prompt(screen) {
        return AgentReading::Starting { shell_prompt: true };
    }
    let starting = AgentReading::Starting {
        shell_prompt: false,
    };
    let rules = rules::rules_for(agent);
    let reading = rules.read(screen);
    let composer = crate::view_drive::Reading::of(agent, screen)
        .chat
        .map(|chat| chat.composer);
    let dialog_composer = matches!(
        composer,
        Some(ComposerState::Dialog | ComposerState::Overlay { .. })
    );
    if (shows_agent(&reading) && reads_dialog(&reading)) || dialog_composer {
        AgentReading::Dialog
    } else if composer == Some(ComposerState::Ready) {
        AgentReading::Ready
    } else {
        starting
    }
}

/// What a spawn did to a plain shell just before its watcher starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellLaunch {
    /// A plain shell, or a reattached session: nothing was typed into it, or
    /// whatever was typed is long over.
    Plain,
    /// The spawn has just typed an agent's launch line into this shell: its
    /// echo and the agent's first paint are not a command's work.
    AgentTyped,
}

/// Spawn a background task that sets a plain shell's status.
///
/// The task feeds the shell's grid from its PTY output, in every mode. While
/// an agent runs in the shell (hooks drive it, or its screen showed the agent
/// for a whole window without them), it runs the agent watcher's screen check
/// (see [`watch`]), and `attention_tx` receives one event each time the screen
/// moves the session into `AwaitingInput`. Once the grid's last non-empty row
/// has been the shell's prompt for a whole window, the agent has ended (its
/// last frame stays painted above the prompt): the shell is handed back to
/// its classifier (see [`hand_shell_back`]). A screen no agent rule matches,
/// or the agent's title alone, with no prompt back, keeps the agent running.
/// Without hooks, the shell enters that mode once settled output (read after
/// [`CHECK_DEBOUNCE`] of quiet, never inside a burst) has shown the agent's
/// screen, with no shell prompt under it, for a whole window.
///
/// A shell not running an agent keeps its classifier: command output sets the
/// status to `Working`, while echoed input and prompt redraws leave it
/// `Idle`, and [`SHELL_IDLE_AFTER`] of silence ends a `Working`. `initial` is
/// the status the session's record already carries; the classifier only
/// moves between `Idle` and `Working`, so any other stored status seeds
/// `Idle` (see [`State::seed_plain_shell`]). Output during the watcher's
/// starting [`FOREIGN_WRITE_HOLD`] (a reattached session's replay) only feeds
/// the grid. When hooks start or stop driving the shell, the classifier's
/// state is seeded again from the record and the screen is held off for
/// [`FOREIGN_WRITE_HOLD`]; on every return to the classifier, its volume
/// windows, title and silence start afresh. After a spawn typed an agent's
/// launch line into the shell (`launch` is [`ShellLaunch::AgentTyped`]), the
/// classifier and its idle tick stay off for [`LAUNCH_HOLD`] or until the
/// agent shows on screen, its hooks take over or it ends, so the line's echo
/// and the agent's first paint never read as a turn. `streams.rules` are the screen
/// rules the check reads the screen with: those of the session's agent.
pub fn watch_plain_shell(
    registry: &Arc<SessionRegistry>,
    session_id: String,
    streams: ShellStreams,
    attention_tx: mpsc::UnboundedSender<AttentionEvent>,
    initial: SessionStatus,
    launch: ShellLaunch,
) {
    let start = Instant::now();
    let ShellStreams {
        mut output,
        input_pulses,
        screen,
        rules,
        chat,
    } = streams;
    let sight = Sight {
        screen,
        rules,
        chat,
    };
    let watch = ScreenWatch::new(registry, session_id, sight, attention_tx);
    let mut watcher = ShellWatcher {
        replay_until: watch.confirm.hold_until,
        watch,
        input_pulses,
        state: State::seed_plain_shell(initial),
        quiet_from: start,
        idle_checked: false,
        in_window: VecDeque::new(),
        out_window: VecDeque::new(),
        title_activity: TitleActivity::new(),
        agent_on_screen: false,
        launching_until: match launch {
            ShellLaunch::AgentTyped => Some(start + LAUNCH_HOLD),
            ShellLaunch::Plain => None,
        },
    };
    tokio::spawn(async move { watcher.run(&mut output).await });
}

/// One plain shell's status watcher: the screen check it runs while an agent
/// runs in the shell, and the classifier it runs otherwise.
struct ShellWatcher {
    watch: ScreenWatch,
    /// The end of the watcher's starting hold: output before it is a
    /// reattached session's replay, not shell activity.
    replay_until: Instant,
    input_pulses: mpsc::UnboundedReceiver<usize>,
    /// The classifier's own view of the status.
    state: State,
    /// When the current stretch of silence began: the last output, or the
    /// last record change.
    quiet_from: Instant,
    /// The idle tick fires once per stretch of silence.
    idle_checked: bool,
    in_window: VecDeque<(Instant, u32)>,
    out_window: VecDeque<(Instant, u32)>,
    title_activity: TitleActivity,
    /// Whether the screen showed an agent for a whole window while hooks were
    /// not driving the shell: the agent started without hooks.
    agent_on_screen: bool,
    /// The end of the launch hold of a shell the spawn typed an agent's
    /// launch line into ([`ShellLaunch::AgentTyped`]): until then, or until
    /// the agent shows on screen, its hooks take over or it ends, the
    /// classifier and the idle tick stay off. `None` once the launch is over,
    /// and for a [`ShellLaunch::Plain`] shell.
    launching_until: Option<Instant>,
}

impl ShellWatcher {
    /// Whether an agent runs in the shell: hooks drive it, or its screen
    /// showed one.
    fn mode(&self) -> Mode {
        let hooks = self.watch.wake.seen.is_some_and(|read| read.hook_driven);
        if hooks || self.agent_on_screen {
            Mode::ShellAgent
        } else {
            Mode::Shell
        }
    }

    /// Whether the agent the spawn typed into the shell is still starting at
    /// `now`: its launch line's echo and first paint are not shell activity.
    fn launching(&self, now: Instant) -> bool {
        self.launching_until.is_some_and(|until| now < until) && self.mode() == Mode::Shell
    }

    async fn run(&mut self, output: &mut broadcast::Receiver<Vec<u8>>) {
        loop {
            let mode = self.mode();
            let next_check = self.watch.next_check();
            let next_idle_check = self.quiet_from + SHELL_IDLE_AFTER;
            let idle_due = mode == Mode::Shell
                && !self.launching(Instant::now())
                && !self.idle_checked
                && plain_shell_idle_due(&self.watch.registry, &self.watch.session_id, self.state);
            // In arm order: output is fed before a due check reads the grid,
            // and a record change re-arms the idle deadline before a stale
            // deadline can fire.
            tokio::select! {
                biased;
                msg = output.recv() => match msg {
                    Ok(bytes) => {
                        if self.on_output(&bytes, mode).is_break() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => self.watch.lagged(n),
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                pulse = self.input_pulses.recv() => {
                    if let Some(n) = pulse {
                        let now = Instant::now();
                        push_window(&mut self.in_window, now, n);
                        trim_window(&mut self.in_window, now);
                    }
                },
                event = self.watch.wake.events.recv(), if self.watch.wake.open => {
                    let before = self.watch.wake.seen;
                    if self.watch.wake.changed(&self.watch.registry, &self.watch.session_id, event) {
                        self.record_moved(before, mode);
                    }
                },
                () = sleep_until(next_check.unwrap_or_else(Instant::now)), if next_check.is_some() => {
                    if self.check(Instant::now(), mode).is_break() {
                        return;
                    }
                },
                () = sleep_until(next_idle_check), if idle_due => {
                    // A record change whose broadcast is still on its way
                    // starts a new stretch instead of meeting the old deadline.
                    let before = self.watch.wake.seen;
                    if self.watch.wake.reread(&self.watch.registry, &self.watch.session_id) {
                        self.record_moved(before, mode);
                    } else {
                        self.idle_checked = true;
                        on_plain_shell_idle(
                            &self.watch.registry,
                            &self.watch.session_id,
                            &mut self.state,
                        );
                    }
                }
            }
        }
    }

    /// Feed one chunk into the grid and the volume windows, run the classifier
    /// on it while no agent is running, and run the screen check if it is
    /// due. A replay chunk only feeds the grid. Breaks once the record is gone.
    fn on_output(&mut self, bytes: &[u8], mode: Mode) -> ControlFlow<()> {
        let now = Instant::now();
        if now < self.replay_until {
            // The title the replay leaves is the one later chunks compare to.
            self.title_activity.feed(bytes);
            self.watch.feed(bytes, now);
            return ControlFlow::Continue(());
        }
        self.quiet_from = now;
        self.idle_checked = false;
        // Drain any input pulses that landed in our channel before (or
        // alongside) this output chunk so the echo budget is computed against
        // the right input volume: a fast typist's keystroke must not race
        // ahead of its own echo into the Working state.
        while let Ok(n) = self.input_pulses.try_recv() {
            push_window(&mut self.in_window, now, n);
        }
        push_window(&mut self.out_window, now, bytes.len());
        trim_window(&mut self.in_window, now);
        trim_window(&mut self.out_window, now);
        // A title change (e.g. a `cd` that repaints the cwd in the prompt)
        // counts as activity alongside command output.
        let title_changed = self.title_activity.feed(bytes);
        let due = self.watch.feed(bytes, now);
        if mode == Mode::Shell && !self.launching(now) {
            self.classify(title_changed);
        }
        if due {
            return self.check(now, mode);
        }
        ControlFlow::Continue(())
    }

    /// Run the shell classifier over the volume windows.
    fn classify(&mut self, title_changed: bool) {
        let registry = &self.watch.registry;
        let session_id = &self.watch.session_id;
        match record_status(registry, session_id) {
            Some(read) if read.hook_driven => {
                // Hooks have started driving the shell and their broadcast has
                // not arrived yet: a classifier promotion would overwrite a
                // hook-reported `AwaitingInput` with `Working`.
                self.state = State::seed_plain_shell(read.status);
            }
            Some(read) => {
                let in_sum = window_sum(&self.in_window);
                let out_sum = window_sum(&self.out_window);
                let next = if shell_output_is_working(in_sum, out_sum) || title_changed {
                    State::Working
                } else {
                    self.state
                };
                on_plain_shell_transition(registry, session_id, &mut self.state, next, read);
            }
            // The record is gone; there is nothing to write.
            None => {}
        }
    }

    /// Run the screen check and act on what it found. Breaks once the record
    /// is gone.
    fn check(&mut self, now: Instant, mode: Mode) -> ControlFlow<()> {
        match self.watch.check(now, mode) {
            Checked::Gone => return ControlFlow::Break(()),
            Checked::Done => {}
            Checked::HooksFlipped => self.hooks_flipped(now),
            Checked::AgentShown => {
                // Agent mode reads the screen at once, from a fresh window.
                self.launching_until = None;
                self.agent_on_screen = true;
                self.watch.confirm.check_at = Some(now);
                agent_shown_in_shell(&self.watch.registry, &self.watch.session_id);
            }
            Checked::AgentEnded => {
                self.launching_until = None;
                self.agent_on_screen = false;
                self.reseed();
                self.start_shell_mode(now);
            }
        }
        ControlFlow::Continue(())
    }

    /// The record moved from `before` to what the wake now holds: a new
    /// stretch of silence starts, and a hook-driven flip switches modes.
    fn record_moved(&mut self, before: Option<StatusRead>, mode: Mode) {
        let now = Instant::now();
        self.quiet_from = now;
        self.idle_checked = false;
        if self.watch.record_moved(before, mode, now) == Moved::HooksFlipped {
            self.hooks_flipped(now);
        }
    }

    /// Hooks started or stopped driving the shell: drop any window, seed the
    /// classifier from the record, and hold off the screen. The agent's
    /// launch, if one was typed, is over.
    fn hooks_flipped(&mut self, now: Instant) {
        self.launching_until = None;
        self.watch.confirm.hold(now);
        self.reseed();
        if self.mode() == Mode::Shell {
            self.start_shell_mode(now);
        }
    }

    /// The classifier takes the shell back: the volume, the title and the
    /// silence it saw while the agent ran are not the shell's activity.
    fn start_shell_mode(&mut self, now: Instant) {
        self.in_window.clear();
        self.out_window.clear();
        self.title_activity = TitleActivity::new();
        self.quiet_from = now;
        self.idle_checked = false;
    }

    /// Seed the classifier from the record the wake last saw.
    fn reseed(&mut self) {
        if let Some(read) = self.watch.wake.seen {
            self.state = State::seed_plain_shell(read.status);
        }
    }
}

/// Run a plain shell classifier's idle transition once a silence has lasted
/// [`SHELL_IDLE_AFTER`].
///
/// A record left `Working` by hooks that have ended is adopted first, so the
/// demotion writes even when no PTY chunk synced the local state since. A
/// record hooks drive is left to the screen check.
fn on_plain_shell_idle(registry: &Arc<SessionRegistry>, session_id: &str, state: &mut State) {
    if let Some(read) = record_status(registry, session_id).filter(|read| !read.hook_driven) {
        *state = State::seed_plain_shell(read.status);
        on_plain_shell_transition(registry, session_id, state, State::Idle, read);
    }
}

fn push_window(window: &mut VecDeque<(Instant, u32)>, now: Instant, bytes: usize) {
    let n = u32::try_from(bytes).unwrap_or(u32::MAX);
    window.push_back((now, n));
}

fn trim_window(window: &mut VecDeque<(Instant, u32)>, now: Instant) {
    let cutoff = now.checked_sub(VOLUME_WINDOW).unwrap_or(now);
    while let Some(&(t, _)) = window.front() {
        if t < cutoff {
            window.pop_front();
        } else {
            break;
        }
    }
}

fn window_sum(window: &VecDeque<(Instant, u32)>) -> u64 {
    window.iter().map(|(_, n)| u64::from(*n)).sum()
}

fn shell_output_is_working(input_bytes: u64, output_bytes: u64) -> bool {
    let budget = input_bytes
        .saturating_mul(SHELL_ECHO_BUDGET_PER_INPUT_BYTE)
        .saturating_add(SHELL_MIN_WORKING_EXCESS);
    output_bytes >= SHELL_MIN_WORKING_OUTPUT && output_bytes > budget
}

/// A plain shell classifier's own view of its status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Working,
}

impl State {
    /// The plain-shell classifier's starting point for a record already
    /// carrying `status`. The classifier only moves between `Idle` and
    /// `Working`, so every other stored status starts at `Idle`.
    fn seed_plain_shell(status: SessionStatus) -> Self {
        match status {
            SessionStatus::Working => Self::Working,
            SessionStatus::Spawning
            | SessionStatus::Idle
            | SessionStatus::AwaitingInput
            | SessionStatus::Stopped
            | SessionStatus::Error => Self::Idle,
        }
    }
}

impl From<State> for SessionStatus {
    fn from(s: State) -> Self {
        match s {
            State::Idle => SessionStatus::Idle,
            State::Working => SessionStatus::Working,
        }
    }
}

/// The record fields a watcher read before deciding a transition, and the
/// write re-checks under the record lock: a hook event or another writer's
/// change that landed in between wins over the watcher.
#[derive(Clone, Copy, Debug)]
struct StatusRead {
    status: SessionStatus,
    hook_driven: bool,
    /// The main agent's hook stamp: a subagent's event moves only the
    /// ordering stamp, and never counts as the record moving.
    main_hook_time: Option<u128>,
    /// Whether a prompt is pending: a subagent's prompt raised over a wait the
    /// screen raised moves only this.
    pending: bool,
    /// The turn end the record holds.
    held: Option<HeldRead>,
}

/// The turn end a [`StatusRead`] saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HeldRead {
    /// The `Stop` that holds it.
    event_time: u128,
    /// When it reaches [`TURN_END_CEILING`].
    ceiling: Instant,
    /// The `Stop` reported background work.
    background: bool,
}

/// Two reads match when the record has not moved between them. While a turn
/// end is held its `Stop` stands in for the main hook stamp. A subagent's
/// event moves only the ordering stamp, which is not compared: a window takes
/// about a second, and those events would otherwise restart it for as long as
/// the subagents work.
impl PartialEq for StatusRead {
    fn eq(&self, other: &Self) -> bool {
        self.status == other.status
            && self.hook_driven == other.hook_driven
            && self.held == other.held
            && self.pending == other.pending
            && (self.held.is_some() || self.main_hook_time == other.main_hook_time)
    }
}

impl Eq for StatusRead {}

/// What a watcher reads of `record`.
fn status_read(record: &SessionRecord) -> StatusRead {
    StatusRead {
        status: record.status,
        hook_driven: record.hook_driven,
        main_hook_time: record.main_hook_time,
        pending: record.pending_input.is_some(),
        held: record.turn_end.as_ref().map(|held| HeldRead {
            event_time: held.event_time,
            ceiling: held.held_at + TURN_END_CEILING,
            background: held.background,
        }),
    }
}

/// The record's current status, whether hooks drive its promotions, the main
/// agent's hook stamp (not the ordering stamp), whether a prompt is pending
/// and any held turn end, as one snapshot the caller carries into its write.
fn record_status(registry: &Arc<SessionRegistry>, session_id: &str) -> Option<StatusRead> {
    let arc = registry.get(session_id)?;
    let guard = crate::sync::lock(&arc);
    Some(status_read(&guard))
}

/// The registry's session events, read for one session: a hook event or
/// another writer changes the record's status, hook stamp or hook-driven flag
/// without any PTY byte, and the watcher has to react to that change (it
/// holds off reading its screen; a plain shell also restarts its idle
/// deadline, and switches modes when the hook-driven flag flips).
struct RecordWake {
    events: broadcast::Receiver<SessionEvent>,
    /// False once the registry's event stream has closed.
    open: bool,
    /// The record last seen; `None` once the record is gone.
    seen: Option<StatusRead>,
}

impl RecordWake {
    /// Subscribe before the watcher task starts, so an event that lands
    /// before its first poll is not missed.
    fn new(registry: &Arc<SessionRegistry>, session_id: &str) -> Self {
        Self {
            events: registry.subscribe(),
            open: true,
            seen: record_status(registry, session_id),
        }
    }

    /// Take one registry event; answers whether this session's record changed
    /// since the last one seen. A lagged stream re-reads the record, since the
    /// missed events may have changed it.
    fn changed(
        &mut self,
        registry: &Arc<SessionRegistry>,
        session_id: &str,
        event: Result<SessionEvent, broadcast::error::RecvError>,
    ) -> bool {
        match event {
            Ok(SessionEvent::Updated(snapshot, _)) if snapshot.id == session_id => {
                self.reread(registry, session_id)
            }
            Ok(_) => false,
            Err(broadcast::error::RecvError::Lagged(_)) => self.reread(registry, session_id),
            Err(broadcast::error::RecvError::Closed) => {
                self.open = false;
                false
            }
        }
    }

    /// Re-read the record; answers whether it moved since the last one seen.
    /// The idle tick calls it too, since a registry update writes the record
    /// before it broadcasts the change.
    fn reread(&mut self, registry: &Arc<SessionRegistry>, session_id: &str) -> bool {
        let mark = record_status(registry, session_id);
        let changed = mark != self.seen;
        self.seen = mark;
        changed
    }

    /// When the turn end last seen reaches [`TURN_END_CEILING`].
    fn ceiling(&self) -> Option<Instant> {
        Some(self.seen?.held?.ceiling)
    }

    /// [`Self::reread`] over a read the caller has already taken.
    fn note(&mut self, read: StatusRead) -> bool {
        let changed = Some(read) != self.seen;
        self.seen = Some(read);
        changed
    }
}

/// Whether a plain shell classifier's idle tick is due: its own `Working`, or
/// a record still reporting `Working` when the PTY goes quiet (one whose
/// hooks have since ended). A record hooks drive is the screen check's.
fn plain_shell_idle_due(registry: &Arc<SessionRegistry>, session_id: &str, state: State) -> bool {
    match record_status(registry, session_id) {
        Some(read) => {
            !read.hook_driven && (state == State::Working || read.status == SessionStatus::Working)
        }
        None => state == State::Working,
    }
}

/// Apply `next` to `record` when it is a real change.
///
/// Sets the status, clears the background flag (true only beside the `Idle`
/// a `Stop` or a held turn end writes after this) and records the
/// `"awaiting input"` action on entry. A held turn end is settled by the
/// registry update around it, as every status change does. Answers whether
/// the record entered `AwaitingInput`, so a caller holding a registry update
/// can raise the one `Attention` after that update returns.
pub fn apply_status(record: &mut SessionRecord, next: SessionStatus) -> bool {
    if record.status == next {
        return false;
    }
    record.status = next;
    record.background = false;
    if next == SessionStatus::AwaitingInput {
        push_recent_action(record, "awaiting input".to_string());
        return true;
    }
    false
}

/// Send the one `Attention` an entry into `AwaitingInput` raises. Call it
/// after the registry update has returned, so a slow consumer never holds the
/// record lock.
pub fn announce_awaiting_input(
    session_id: &str,
    attention_tx: &mpsc::UnboundedSender<AttentionEvent>,
) {
    let _ = attention_tx.send(AttentionEvent {
        session_id: session_id.to_string(),
        reason: AttentionReason::AwaitingInput,
    });
}

fn on_plain_shell_transition(
    registry: &Arc<SessionRegistry>,
    session_id: &str,
    current: &mut State,
    next: State,
    read: StatusRead,
) {
    if next == *current {
        return;
    }
    *current = next;
    registry.update(session_id, |rec| {
        if rec.hook_driven != read.hook_driven || rec.main_hook_time != read.main_hook_time {
            return;
        }
        apply_watcher_status(rec, SessionStatus::from(next));
    });
}

/// [`apply_status`] for a status watcher's own write: leaving `AwaitingInput`
/// also drops the prompt it was waiting on, since no hook will (a prompt
/// restored on reattach, or one dismissed while hooks are not driving).
fn apply_watcher_status(record: &mut SessionRecord, next: SessionStatus) -> bool {
    let was_awaiting = record.status == SessionStatus::AwaitingInput;
    let entered_awaiting = apply_status(record, next);
    if was_awaiting && record.status != SessionStatus::AwaitingInput {
        record.pending_input = None;
    }
    entered_awaiting
}

#[cfg(test)]
mod tests {
    use super::shell_output_is_working;

    #[test]
    fn shell_output_below_minimum_does_not_mark_working() {
        assert!(!shell_output_is_working(0, 120));
    }

    #[test]
    fn shell_echo_budget_suppresses_typed_input_redraws() {
        assert!(!shell_output_is_working(32, 220));
    }

    #[test]
    fn pwsh_syntax_highlight_burst_does_not_mark_working() {
        // Typing 4 chars into PSReadLine: ~100 bytes of redraw per char,
        // including cursor moves, color sets, and the re-emitted line tail.
        assert!(!shell_output_is_working(4, 400));
    }

    #[test]
    fn shell_command_output_marks_working() {
        assert!(shell_output_is_working(8, 1_200));
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod hysteresis_tests {
    use super::{
        AgentReading, AgentStreams, CHECK_DEBOUNCE, CONFIRM_SPACING, FOREIGN_WRITE_HOLD,
        IDLE_WITHOUT_STOP, LAUNCH_HOLD, SHELL_IDLE_AFTER, ScreenWrite, ShellLaunch, ShellStreams,
        State, TITLE_QUIET_LIMIT, agent_reading, apply_screen_status, hand_shell_back,
        on_plain_shell_transition, record_status, watch, watch_plain_shell,
    };
    use crate::paths::Dirs;
    use crate::screen::fixture_screen::screen_from;
    use crate::screen::rules::{CompiledRules, rules_for};
    use crate::screen::{Screen, SharedScreen};
    use crate::session::{SessionRecord, SessionRegistry};
    use agent_api::{LineMatcher, ScreenRegion, ScreenRule, ScreenRules, ScreenVerdict};
    use chrono::Utc;
    use protocol::{
        AppearanceOverrides, SessionKind, SessionMetrics, SessionMode, SessionRole, SessionStatus,
    };
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::{broadcast, mpsc};

    /// A `MakeWriter` that keeps what a subscriber formats, for the
    /// `status_trace` tests.
    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("the log buffer is not poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for LogCapture {
        type Writer = Self;

        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    impl LogCapture {
        fn text(&self) -> String {
            let bytes = self.0.lock().expect("the log buffer is not poisoned");
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    /// The subscriber a `status_trace` test captures with, filtered as the
    /// Detailed logging switch raises it.
    fn status_trace_subscriber(capture: LogCapture) -> impl tracing::Subscriber + Send + Sync {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("status_trace=debug"))
            .with_ansi(false)
            .with_writer(capture)
            .finish()
    }

    #[tokio::test(start_paused = true)]
    async fn a_screen_status_change_logs_the_rule_that_fired() {
        let capture = LogCapture::default();
        let _guard = tracing::subscriber::set_default(status_trace_subscriber(capture.clone()));
        let h = start("status-trace", false);
        pass_hold().await;
        h.chunk(permission_frame());
        advance(ms(50)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the dialog raises the prompt"
        );
        let text = capture.text();
        assert!(
            text.contains("status changed by a screen read"),
            "the screen's cause line is missing: {text}"
        );
        assert!(text.contains("to=AwaitingInput"), "{text}");
        assert!(
            text.contains("rule=Some("),
            "the rule that fired is not named: {text}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unchanged_screen_read_logs_nothing() {
        let capture = LogCapture::default();
        let _guard = tracing::subscriber::set_default(status_trace_subscriber(capture.clone()));
        let h = start("status-trace-steady", false);
        pass_hold().await;
        for _ in 0..5 {
            h.chunk(idle_frame(&[]));
            advance(ms(200)).await;
        }
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the screen shows idle throughout"
        );
        let text = capture.text();
        assert!(
            !text.contains("status changed by a screen read"),
            "an unchanged read logged a change: {text}"
        );
    }

    /// Cadence of the slowest TUI element the watcher must ride through
    /// without demoting: an elapsed-time counter repainting once a second.
    const HEARTBEAT: Duration = Duration::from_secs(1);

    fn scratch_dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("rt-pty-state-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch dir");
        Dirs {
            config: root.clone(),
            state_file: root.join("state.json"),
            handshake_file: root.join("daemon.json"),
            remote_config_file: root.join("remote.json"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
            bin_templates: None,
        }
    }

    fn test_record(id: &str) -> SessionRecord {
        SessionRecord {
            id: id.to_string(),
            label: id.to_string(),
            default_label: id.to_string(),
            user_label: None,
            role: SessionRole::Normal,
            kind: SessionKind::Standalone,
            members: Vec::new(),
            mode: SessionMode::Interactive,
            started_at: Utc::now(),
            status: SessionStatus::Idle,
            status_since: Some(Utc::now()),
            status_seq: 0,
            exit_code: None,
            metrics: SessionMetrics::default(),
            recent_actions: Vec::new(),
            pty: None,
            headless: None,
            workspace_id: None,
            agent: agent_registry::default_agent(),
            terminal_title: None,
            program_name: None,
            current_cwd: None,
            appearance: AppearanceOverrides::default(),
            spawn_config: None,
            is_abandoned: false,
            is_inactive: false,
            worktree_paths: Vec::new(),
            last_prompt: None,
            input_notifier: None,
            screen: None,
            scrollback_snapshot_req: None,
            spawn_origin: None,
            conversation_id: None,
            pending_input: None,
            last_hook_time: None,
            main_hook_time: None,
            hook_driven: false,
            background: false,
            turn_end: None,
            last_assistant_message: None,
            summary: None,
            busy_excluded: false,
            agent_in_shell: false,
            shell_label: None,
            agent_exit_announced: false,
            applied_size: None,
            size_client: None,
            work: crate::background_work::BackgroundWork::default(),
            hook_agent: None,
            hook_options: protocol::OptionValues::new(),
            resumed_from: Vec::new(),
        }
    }

    struct Harness {
        registry: Arc<SessionRegistry>,
        output: broadcast::Sender<Vec<u8>>,
        /// A plain shell watcher's input pulses; holding it keeps the channel
        /// open.
        input: mpsc::UnboundedSender<usize>,
        /// Keeps the input channel open when the watcher does not read it.
        _input: Option<mpsc::UnboundedReceiver<usize>>,
        attention: mpsc::UnboundedReceiver<super::AttentionEvent>,
        /// The grid an agent session's watcher feeds and reads.
        screen: SharedScreen,
    }

    const SESSION: &str = "s1";

    /// The grid the harness feeds: wide enough for the fixture rows.
    const COLS: u16 = 140;
    const ROWS: u16 = 40;

    fn start(tag: &str, plain_shell: bool) -> Harness {
        start_from(tag, plain_shell, SessionStatus::Idle)
    }

    /// Start a watcher over a record that already carries `initial`, the way a
    /// session reattached from its sidecar does.
    fn start_from(tag: &str, plain_shell: bool, initial: SessionStatus) -> Harness {
        let mut record = test_record(SESSION);
        record.status = initial;
        start_record(tag, plain_shell, record)
    }

    /// Start a watcher over `record` as the session's spawn or reattach left
    /// it.
    fn start_record(tag: &str, plain_shell: bool, record: SessionRecord) -> Harness {
        let rules = rules_for(&record.agent);
        start_in(scratch_dirs(tag), plain_shell, record, rules)
    }

    /// [`start_record`] over a registry in `dirs`, so a test can put a
    /// sidecar there first, with the screen rules `rules`. The record carries
    /// the grid its watcher feeds.
    fn start_in(
        dirs: Dirs,
        plain_shell: bool,
        record: SessionRecord,
        rules: Arc<CompiledRules>,
    ) -> Harness {
        start_launching_in(dirs, plain_shell, record, rules, ShellLaunch::Plain)
    }

    /// [`start_record`] for a plain shell the spawn did `launch` to.
    fn start_record_launching(tag: &str, record: SessionRecord, launch: ShellLaunch) -> Harness {
        let rules = rules_for(&record.agent);
        start_launching_in(scratch_dirs(tag), true, record, rules, launch)
    }

    /// [`start_in`] with the plain shell's launch kind `launch`.
    fn start_launching_in(
        dirs: Dirs,
        plain_shell: bool,
        mut record: SessionRecord,
        rules: Arc<CompiledRules>,
        launch: ShellLaunch,
    ) -> Harness {
        let initial = record.status;
        let screen: SharedScreen = Arc::new(Mutex::new(Screen::new(COLS, ROWS)));
        record.screen = Some(Arc::clone(&screen));
        let registry = SessionRegistry::new(dirs);
        registry.insert(record);
        let chat = crate::chat_mirror::ChatMirror::new(
            Arc::clone(&registry),
            crate::hook_status::LiveLines::default(),
            Arc::new(tokio::sync::watch::channel(0usize).0),
        )
        .tap(SESSION, &agent_registry::default_agent());
        let (output, output_rx) = broadcast::channel(64);
        let (input, input_rx) = mpsc::unbounded_channel();
        let (attention_tx, attention_rx) = mpsc::unbounded_channel();
        let unread_input = if plain_shell {
            watch_plain_shell(
                &registry,
                SESSION.to_string(),
                ShellStreams {
                    output: output_rx,
                    input_pulses: input_rx,
                    screen: Arc::clone(&screen),
                    rules,
                    chat,
                },
                attention_tx,
                initial,
                launch,
            );
            None
        } else {
            watch(
                &registry,
                SESSION.to_string(),
                AgentStreams {
                    output: output_rx,
                    screen: Arc::clone(&screen),
                    rules,
                    chat,
                },
                attention_tx,
            );
            Some(input_rx)
        };
        Harness {
            registry,
            output,
            input,
            _input: unread_input,
            attention: attention_rx,
            screen,
        }
    }

    impl Harness {
        fn status(&self) -> SessionStatus {
            let rec = self.registry.get(SESSION).expect("session registered");
            let guard = rec.lock().expect("record lock");
            guard.status
        }

        /// Change the record without the registry's `Updated` broadcast: the
        /// window a registry update leaves between its record write and its
        /// event, while it syncs the sidecar.
        fn set_record_unbroadcast(&self, f: impl FnOnce(&mut SessionRecord)) {
            let rec = self.registry.get(SESSION).expect("session registered");
            f(&mut rec.lock().expect("record lock"));
        }

        fn hook_driven(&self) -> bool {
            let rec = self.registry.get(SESSION).expect("session registered");
            let guard = rec.lock().expect("record lock");
            guard.hook_driven
        }

        /// Emit one elapsed-time title repaint, e.g. `ESC ] 0 ; ⠋ 3s BEL`.
        fn heartbeat(&self, tick: u32) {
            let frame = format!("\x1b]0;\u{280b} {tick}s\x07");
            self.output
                .send(frame.into_bytes())
                .expect("watcher still subscribed");
        }

        /// Change the session record the way a hook event does while the
        /// watcher is parked.
        fn set_record(&self, f: impl FnOnce(&mut SessionRecord)) {
            self.registry.update(SESSION, f);
        }

        /// Send one chunk of PTY output.
        fn chunk(&self, bytes: Vec<u8>) {
            self.output.send(bytes).expect("watcher still subscribed");
        }

        /// How many `Attention` events the watcher raised since the last call.
        fn attentions(&mut self) -> usize {
            let mut count = 0;
            while self.attention.try_recv().is_ok() {
                count += 1;
            }
            count
        }

        fn background(&self) -> bool {
            let rec = self.registry.get(SESSION).expect("session registered");
            rec.lock().expect("record lock").background
        }
    }

    /// Let the spawned watcher task run until it has nothing left to poll,
    /// without advancing the paused clock.
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    /// Move the paused clock on by `by`, firing every timer due on the way,
    /// then let the watcher run.
    async fn advance(by: Duration) {
        tokio::time::sleep(by).await;
        settle().await;
    }

    /// Move the paused clock past the hold the watcher starts with, or the
    /// one a foreign write began.
    async fn pass_hold() {
        settle().await;
        advance(FOREIGN_WRITE_HOLD + Duration::from_millis(1)).await;
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A full repaint: home, clear, then `rows` top to bottom.
    fn frame(rows: &[&str]) -> Vec<u8> {
        format!("\x1b[H\x1b[2J{}", rows.join("\r\n")).into_bytes()
    }

    /// A solid rule as wide as the fixtures' prompt box.
    fn rule() -> String {
        "─".repeat(134)
    }

    /// The status rows under the idle prompt box of `v03-ready.txt`.
    const READY_FOOTER: [&str; 2] = [
        concat!(
            "  sub: 5h  33% (4h07m) · 7d  84% / F   0% (5d16h)  |  ds: $  17.40  |  X: 252G free",
            "  |  Opus 5.5  |  master  |  C:\\Users\\lefto\\AppD…"
        ),
        "  ⏵⏵ auto mode on (shift+tab to cycle) · ← 2 agents",
    ];

    /// The spinner row of `v71-agents-running.txt`.
    const SPINNER_ROW: &str = "· Cooking… (21s · ↓ 1.1k tokens)";

    /// The row an Esc interrupt paints above the prompt box.
    const INTERRUPT_ROW: &str = "  ⎿  Interrupted · What should Claude do instead?";

    /// The spinner a turn keeps up while its `Stop` hooks run
    /// (`claude--v2302-turn-ending-stop-hooks--w119.txt`).
    const STOP_HOOKS_ROW: &str = "✢ Simmering… (running Stop hooks… 0/2 · 5s · ↓ 621 tokens)";

    /// The same spinner while `PreToolUse` hooks run.
    const PRE_TOOL_HOOKS_ROW: &str = "✢ Simmering… (running PreToolUse hooks… · 5s · ↓ 621 tokens)";

    /// The prompt box of `v03-ready.txt` with `above` over it and `footer`
    /// under it.
    fn box_frame(above: &[&str], footer: &[&str]) -> Vec<u8> {
        let rule = rule();
        let mut rows: Vec<&str> = above.to_vec();
        rows.extend([
            rule.as_str(),
            "❯ Try \"fix typecheck errors\"",
            rule.as_str(),
        ]);
        rows.extend(footer);
        frame(&rows)
    }

    /// The idle prompt box of `v03-ready.txt` with `above` over it.
    fn idle_frame(above: &[&str]) -> Vec<u8> {
        box_frame(above, &READY_FOOTER)
    }

    /// The Bash permission dialog of `c23-perm-bash.txt`.
    fn permission_frame() -> Vec<u8> {
        let rule = "─".repeat(135);
        let dashes = "╌".repeat(135);
        frame(&[
            "$ … Print the product of 6 and 7",
            "    Bash(python -c \"print(6*7)\")",
            "",
            rule.as_str(),
            " Bash command",
            " Print the product of 6 and 7",
            dashes.as_str(),
            " python -c \"print(6*7)\"",
            dashes.as_str(),
            " This command requires approval",
            "",
            " Do you want to proceed?",
            " ❯ 1. Yes",
            "   2. Yes, and don’t ask again for: python *",
            "   3. Yes, and switch to auto mode · auto mode handles these prompts for you",
            "   4. No",
            "",
            " Esc to cancel · Tab to amend",
        ])
    }

    /// The `AskUserQuestion` dialog of `c31-ask-q1.txt`.
    fn question_frame() -> Vec<u8> {
        let rule = format!("  {}", "─".repeat(133));
        frame(&[
            "• …",
            rule.as_str(),
            "  ──",
            "  ←  ☐ Next fn  ☐ Tests  ☐ Style  ✔ Submit  →",
            "",
            "  Which function should be added to calc.py next?",
            "",
            "  ❯ 1. sub",
            "       Subtraction: returns a - b.",
            "    2. div",
            "       Division: returns a / b, which needs a decision on division by zero.",
            "    3. pow",
            "       Exponentiation: returns a ** b.",
            "    4. Type something.",
            rule.as_str(),
            "  ──",
            "    5. Chat about this",
            "",
            "  Enter to select · Tab/Arrow keys to navigate · Esc to cancel",
        ])
    }

    /// The detailed-transcript overlay of `v96-ctrl-o.txt`.
    fn transcript_overlay_frame() -> Vec<u8> {
        let rule = "─".repeat(135);
        frame(&[
            "  The plant world sorts fruit quite differently from the kitchen.",
            "",
            "✻ Brewed for 26s · done 5:34 PM",
            "",
            rule.as_str(),
            concat!(
                "  Showing detailed transcript · ctrl+o to toggle · ↑↓ scroll",
                " · v to open in notepad · ? for shortcuts          verbose"
            ),
        ])
    }

    /// A finished turn with a background shell still running
    /// (`claude--idle-background-shell.txt`).
    fn background_shell_frame() -> Vec<u8> {
        box_frame(
            &["● ok", "", "✻ Churned for 6s · done 8:01 PM"],
            &["  ⏵⏵ bypass permissions on · 1 shell · ← for agents · ↓ to manage"],
        )
    }

    /// An idle box over launched subagents, with their panel under it
    /// (`claude--v2302-working-agents-panel--w119.txt`).
    fn subagent_panel_frame() -> Vec<u8> {
        box_frame(
            &[
                "● 2 background agents launched (↓ to manage)",
                "   ├ Write 150 animals list A",
                "   └ Write 150 animals list B",
                "",
                "● Running 2 agents…",
            ],
            &[
                "  ⏸ manual mode on · ← 11 agents",
                "",
                "  ● main",
                "  ◯ general-purpose  Write 150 animals list A                   3s · ↑ 188 tokens",
                "  ◯ general-purpose  Write 150 animals list B                    1s · ↑ 50 tokens",
            ],
        )
    }

    /// Move both hook stamps to 2, the way a main agent's hook event does.
    fn main_stamp(record: &mut SessionRecord) {
        record.last_hook_time = Some(2);
        record.main_hook_time = Some(2);
    }

    /// A record whose hooks report `status` at hook stamp 1.
    fn hook_record(status: SessionStatus) -> SessionRecord {
        let mut record = test_record(SESSION);
        record.status = status;
        record.hook_driven = true;
        record.last_hook_time = Some(1);
        record
    }

    async fn assert_heartbeat_holds_working(h: &Harness) {
        for tick in 0..5 {
            h.heartbeat(tick);
            settle().await;
            assert_eq!(h.status(), SessionStatus::Working, "tick {tick} promoted");
            tokio::time::sleep(HEARTBEAT).await;
            settle().await;
            assert_eq!(
                h.status(),
                SessionStatus::Working,
                "tick {tick}: demoted to idle between once-a-second repaints"
            );
        }
        tokio::time::sleep(SHELL_IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "silence past SHELL_IDLE_AFTER"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn once_a_second_repaint_does_not_flicker_plain_shell() {
        let h = start("shell", true);
        pass_hold().await;
        assert_heartbeat_holds_working(&h).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_spinner_promotes_idle_after_one_debounce() {
        let h = start("promote", false);
        pass_hold().await;
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(CHECK_DEBOUNCE * 3 / 4).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "read before the output settled"
        );
        advance(ms(15)).await;
        assert_eq!(h.status(), SessionStatus::Working);
    }

    /// Rules made of one regex rule: a row reading exactly `BUSY` is working.
    fn busy_rules() -> Arc<CompiledRules> {
        Arc::new(
            CompiledRules::try_compile(&ScreenRules {
                rules: vec![ScreenRule {
                    name: "busy".to_string(),
                    priority: 100,
                    region: ScreenRegion::BottomNonEmpty { rows: 4 },
                    matcher: LineMatcher::Regex("^BUSY$".to_string()),
                    not: Vec::new(),
                    verdict: ScreenVerdict::Working,
                }],
            })
            .expect("the busy rule compiles"),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn a_regex_rule_in_a_plugin_rule_set_decides_the_status() {
        let h = start_in(
            scratch_dirs("plugin-regex"),
            false,
            test_record(SESSION),
            busy_rules(),
        );
        pass_hold().await;
        h.chunk(frame(&["BUSY"]));
        advance(CHECK_DEBOUNCE + ms(15)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the plugin's regex rule did not set the status"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_plugin_without_rules_never_writes_a_status() {
        let empty = Arc::new(
            CompiledRules::try_compile(&ScreenRules::default()).expect("an empty table compiles"),
        );
        let h = start_in(
            scratch_dirs("plugin-empty"),
            false,
            test_record(SESSION),
            empty,
        );
        pass_hold().await;
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(Duration::from_secs(5)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "a session whose plugin has no rules took a status from its screen"
        );
    }

    #[test]
    fn agent_up_reads_the_plugins_rules() {
        let registry = SessionRegistry::new(scratch_dirs("agent-up"));
        let screen: SharedScreen = Arc::new(Mutex::new(Screen::new(COLS, ROWS)));
        let mut record = test_record(SESSION);
        record.screen = Some(Arc::clone(&screen));
        registry.insert(record);
        screen.lock().expect("screen lock").feed(&idle_frame(&[]));
        assert_eq!(
            agent_reading(&registry, SESSION),
            AgentReading::Ready,
            "Claude's idle prompt box did not count as the agent up"
        );
        screen
            .lock()
            .expect("screen lock")
            .feed(&box_frame(&[], &["PS C:\\work>"]));
        assert_eq!(
            agent_reading(&registry, SESSION),
            AgentReading::Starting { shell_prompt: true },
            "a shell prompt on the last row counted as the agent up"
        );
    }

    /// The "New MCP server found" dialog Claude shows at start in a folder
    /// whose `.mcp.json` names a server it has not been told about.
    fn mcp_server_screen() -> String {
        let rule = "─".repeat(120);
        [
            "PS C:\\work> claude",
            "",
            rule.as_str(),
            "  New MCP server found in this project: rt-driver",
            "",
            "  MCP servers may execute code or access system resources. All tool calls require approval.",
            "",
            "    Use this MCP server",
            "    Use this and all future MCP servers in this project",
            "  ❯ Continue without using this MCP server",
            "",
            "  Enter to confirm · Esc to cancel",
        ]
        .join("\r\n")
    }

    /// The reading of a registry whose session shows `text`.
    fn reading_of(tag: &str, text: &str) -> AgentReading {
        let registry = SessionRegistry::new(scratch_dirs(tag));
        let mut record = test_record(SESSION);
        record.screen = Some(Arc::new(Mutex::new(screen_from(text))));
        registry.insert(record);
        agent_reading(&registry, SESSION)
    }

    /// A registry whose session shows Claude's folder-trust dialog.
    fn trust_registry(tag: &str) -> Arc<SessionRegistry> {
        let registry = SessionRegistry::new(scratch_dirs(tag));
        let mut record = test_record(SESSION);
        let trust = include_str!("screen/fixtures/claude--trust-prompt.txt");
        record.screen = Some(Arc::new(Mutex::new(screen_from(trust))));
        registry.insert(record);
        registry
    }

    /// Start a chain on `pty` that types the launch line, then holds the
    /// first prompt on `registry`'s reading of its session.
    fn hold_first_prompt(registry: &Arc<SessionRegistry>, pty: &Arc<crate::pty::PtyHandle>) {
        use protocol::{InjectorStartup, InjectorStep, PromptInjector};
        let registry = Arc::clone(registry);
        let agent_up: crate::inject::AgentUp = Box::new(move || agent_reading(&registry, SESSION));
        let injector = |content: &str, startup| PromptInjector {
            steps: vec![InjectorStep::Text {
                content: content.to_owned(),
                newline: true,
            }],
            verify_mode_marker: None,
            startup,
        };
        let chain = vec![
            injector("claude", InjectorStartup::ShellPrompt),
            injector("hello", InjectorStartup::AgentTui),
        ];
        crate::inject::run_chain(
            SESSION.to_owned(),
            Arc::clone(pty),
            chain,
            Some(agent_up),
            agent_api::ReadySignal::default(),
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_first_prompt_is_dropped_when_its_record_is_removed() {
        let registry = trust_registry("held-removed");
        let (pty, _input, _exit) = crate::history::test_support::recording_pty();
        hold_first_prompt(&registry, &pty);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(pty.first_prompt_held(), "held behind the trust dialog");
        registry.remove(SESSION);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !pty.first_prompt_held(),
            "the prompt of a removed session was still held"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_first_prompt_is_dropped_when_its_session_stops() {
        let registry = trust_registry("held-stopped");
        let (pty, _input, _exit) = crate::history::test_support::recording_pty();
        hold_first_prompt(&registry, &pty);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(pty.first_prompt_held(), "held behind the trust dialog");
        registry.update(SESSION, |record| record.status = SessionStatus::Stopped);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !pty.first_prompt_held(),
            "the prompt of a stopped session was still held"
        );
    }

    #[test]
    fn agent_up_is_false_while_a_startup_dialog_holds_the_keys() {
        let trust = include_str!("screen/fixtures/claude--trust-prompt.txt");
        assert_eq!(
            reading_of("agent-up-trust", trust),
            AgentReading::Dialog,
            "the folder-trust dialog"
        );
        assert_eq!(
            reading_of("agent-up-mcp", &mcp_server_screen()),
            AgentReading::Dialog,
            "the new MCP server dialog"
        );
        assert_eq!(
            reading_of("agent-up-shell", "PS C:\\work> "),
            AgentReading::Starting { shell_prompt: true },
            "a shell prompt"
        );
    }

    /// A plain shell never waits for input, and its idle timer only fires from
    /// `Working`: seeded `AwaitingInput` it would never leave it.
    #[test]
    fn plain_shell_seeded_awaiting_input_starts_idle() {
        assert_eq!(
            State::seed_plain_shell(SessionStatus::AwaitingInput),
            State::Idle
        );
        assert_eq!(
            State::seed_plain_shell(SessionStatus::Working),
            State::Working
        );
        assert_eq!(State::seed_plain_shell(SessionStatus::Idle), State::Idle);
    }

    /// A session reattached `AwaitingInput` keeps it while its screen matches
    /// no rule, however long the PTY is quiet, and leaves it once the idle
    /// box has shown for the whole window.
    #[tokio::test(start_paused = true)]
    async fn a_restored_prompt_holds_on_an_unmatched_screen_and_demotes_on_the_idle_box() {
        let h = start_from("restored-awaiting", false, SessionStatus::AwaitingInput);
        pass_hold().await;
        h.chunk(frame(&["PS C:\\> "]));
        advance(Duration::from_secs(10)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a screen no rule matches moved the restored prompt"
        );
        h.chunk(idle_frame(&[]));
        advance(ms(299)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the idle box demoted before the window closed"
        );
        advance(ms(401)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the idle box demotes after the window"
        );
    }

    /// A replay that lands during the startup hold promotes the seeded `Idle`
    /// to `Working` once the hold ends.
    #[tokio::test(start_paused = true)]
    async fn a_reattach_replay_with_the_spinner_restores_working() {
        let mut h = start_from("reattach-spinner", false, SessionStatus::Idle);
        settle().await;
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(FOREIGN_WRITE_HOLD.saturating_sub(ms(10))).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the replay was read during the hold"
        );
        advance(ms(20)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the replayed spinner did not restore Working"
        );
        assert_eq!(h.attentions(), 0, "the replay raised Attention");
    }

    /// A replay of the idle box clears a restored `AwaitingInput` once the
    /// leave window closes, dropping the pending prompt with it.
    #[tokio::test(start_paused = true)]
    async fn a_reattach_replay_of_the_idle_box_clears_the_restored_prompt() {
        let mut h = start_record("reattach-answered", false, restored_prompt_record());
        settle().await;
        h.chunk(idle_frame(&[]));
        advance(FOREIGN_WRITE_HOLD.saturating_sub(ms(10))).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the replay was read during the hold"
        );
        advance(ms(20)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the idle box cleared the prompt before the window closed"
        );
        advance(ms(800)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the idle box did not clear the restored prompt"
        );
        assert_eq!(pending_input(&h), None, "the prompt left with the wait");
        assert_eq!(h.attentions(), 0, "the replay raised Attention");
    }

    /// A replay of the dialog the session was waiting on keeps the restored
    /// prompt, and raises no second Attention.
    #[tokio::test(start_paused = true)]
    async fn a_reattach_replay_of_the_open_dialog_keeps_the_restored_prompt() {
        let mut h = start_record("reattach-dialog", false, restored_prompt_record());
        settle().await;
        h.chunk(permission_frame());
        advance(FOREIGN_WRITE_HOLD.saturating_sub(ms(10))).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the replay was read during the hold"
        );
        advance(Duration::from_secs(10)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the replayed dialog moved the restored prompt"
        );
        assert!(
            pending_input(&h).is_some(),
            "the replayed dialog dropped the pending prompt"
        );
        assert_eq!(
            h.attentions(),
            0,
            "the replayed dialog raised Attention again"
        );
    }

    /// A replay no rule matches leaves a restored `AwaitingInput` as it was.
    #[tokio::test(start_paused = true)]
    async fn a_reattach_replay_no_rule_matches_keeps_the_restored_status() {
        let mut h = start_from("reattach-unmatched", false, SessionStatus::AwaitingInput);
        settle().await;
        h.chunk(frame(&["PS C:\\> "]));
        advance(FOREIGN_WRITE_HOLD.saturating_sub(ms(10))).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the replay was read during the hold"
        );
        advance(Duration::from_secs(10)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a screen no rule matches moved the restored prompt"
        );
        assert_eq!(h.attentions(), 0, "the replay raised Attention");
    }

    /// A hook-reported prompt holds while its dialog repaints, and while a
    /// spinner shows `PreToolUse` hooks running.
    #[tokio::test(start_paused = true)]
    async fn hook_awaiting_input_holds_through_dialog_repaints_and_running_hooks() {
        let h = start_record(
            "hook-survives-output",
            false,
            hook_record(SessionStatus::AwaitingInput),
        );
        pass_hold().await;
        for _ in 0..50 {
            h.chunk(permission_frame());
            advance(ms(20)).await;
        }
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "dialog repaints moved a hook-reported prompt"
        );
        h.chunk(idle_frame(&[PRE_TOOL_HOOKS_ROW]));
        advance(Duration::from_secs(10)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the running hooks' spinner moved a hook-reported prompt"
        );
    }

    /// A finished turn with a background shell still running keeps `Idle` and
    /// the background flag, however long the PTY is quiet.
    #[tokio::test(start_paused = true)]
    async fn a_turn_end_with_a_background_shell_keeps_idle_and_background() {
        let mut record = hook_record(SessionStatus::Idle);
        record.background = true;
        let h = start_record("hook-idle-background", false, record);
        pass_hold().await;
        h.chunk(background_shell_frame());
        advance(Duration::from_secs(10)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(h.background(), "the screen cleared the background flag");
    }

    /// While hooks own a plain shell, output the shell classifier would read
    /// as work must not promote a hook-reported `Idle`.
    #[tokio::test(start_paused = true)]
    async fn hook_plain_shell_idle_survives_working_output() {
        let h = start_record("shell-hook-idle", true, hook_record(SessionStatus::Idle));
        for tick in 0..3 {
            h.chunk(shell_output());
            settle().await;
            assert_eq!(
                h.status(),
                SessionStatus::Idle,
                "chunk {tick}: shell output promoted a hook-reported idle"
            );
            advance(ms(50)).await;
        }
    }

    /// Mark the record hook-driven `Working` at hook stamp `stamp`, the way a
    /// hook event does.
    fn set_hook_working(h: &Harness, stamp: u128) {
        h.set_record(|record| {
            record.hook_driven = true;
            record.status = SessionStatus::Working;
            record.last_hook_time = Some(stamp);
        });
    }

    /// A hook event already on the record but not yet broadcast, landing
    /// inside a window, refuses that window's publish: the next check reads
    /// the moved record and holds off the screen as for a broadcast write.
    /// The idle box still on screen after the hold opens a fresh window.
    async fn assert_unbroadcast_hook_write_refuses_the_publish(h: &Harness) {
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(ms(2500)).await;
        h.set_record_unbroadcast(main_stamp);
        // The window's next check reads the moved record and holds; unrefused,
        // the window opened 2.5 s ago would publish at 3 s.
        advance(ms(780)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a window published over a hook event its broadcast had not reached"
        );
        advance(FOREIGN_WRITE_HOLD + IDLE_WITHOUT_STOP + CONFIRM_SPACING).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the idle box after the hold did not demote"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unbroadcast_hook_write_during_a_window_refuses_the_publish() {
        let h = start_record(
            "hook-unbroadcast",
            false,
            hook_record(SessionStatus::Working),
        );
        assert_unbroadcast_hook_write_refuses_the_publish(&h).await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_unbroadcast_hook_write_during_a_window_refuses_the_publish_plain_shell() {
        let h = start_record(
            "shell-hook-unbroadcast",
            true,
            hook_record(SessionStatus::Working),
        );
        assert_unbroadcast_hook_write_refuses_the_publish(&h).await;
    }

    /// A hook-reported turn after a long idle, while the grid still shows the
    /// idle box: that stale frame does not demote before the window, and the
    /// spinner the turn paints inside the window cancels it.
    async fn assert_a_stale_idle_box_waits_for_the_window(h: &Harness) {
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(10)).await;
        assert_eq!(h.status(), SessionStatus::Idle, "idle from the start");
        set_hook_working(h, 1);
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(ms(100)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a stale idle box demoted the fresh hook-reported turn at once"
        );
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the turn's spinner did not cancel the window"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_idle_box_waits_for_the_window_and_a_spinner_cancels_it() {
        let h = start("hook-working-long-idle", false);
        assert_a_stale_idle_box_waits_for_the_window(&h).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_idle_box_waits_for_the_window_and_a_spinner_cancels_it_plain_shell() {
        let h = start("shell-hook-working-long-idle", true);
        assert_a_stale_idle_box_waits_for_the_window(&h).await;
    }

    /// The OSC title of a turn that runs.
    const WORKING_TITLE: &str = "\x1b]0;◐ Fixing tests\x07";
    /// The OSC title of an idle CLI.
    const IDLE_TITLE: &str = "\x1b]0;✳ Fixing tests\x07";

    /// The rows a task subject wider than the pane leaves: the spinner wraps
    /// over rows no rule reads, and the idle box under it wins.
    const WRAPPED_SPINNER_ROWS: [&str; 2] = [
        "✶ Fix the widget",
        "the fetch boundary (first pass), then run the full gate…… · thinking",
    ];

    /// `bytes` under the OSC `title`.
    fn under_title(title: &str, bytes: Vec<u8>) -> Vec<u8> {
        let mut frame = title.as_bytes().to_vec();
        frame.extend(bytes);
        frame
    }

    /// A task subject wider than the pane wraps the spinner over rows no rule
    /// reads, so the body reads the idle box while the title says the turn
    /// runs.
    #[tokio::test(start_paused = true)]
    async fn a_wrapped_spinner_under_a_working_title_keeps_a_turn_with_no_stop_working() {
        let h = start_record(
            "wrapped-spinner",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(under_title(
            WORKING_TITLE,
            idle_frame(&WRAPPED_SPINNER_ROWS),
        ));
        advance(FOREIGN_WRITE_HOLD + IDLE_WITHOUT_STOP + CONFIRM_SPACING).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "an idle box under a working title ended a turn with no Stop"
        );
    }

    /// The same rows in a shell the spawn typed the agent into: the shell's
    /// watcher runs the one check too.
    #[tokio::test(start_paused = true)]
    async fn a_shell_hosted_wrapped_spinner_under_a_working_title_keeps_its_turn_working() {
        let h = start_record(
            "shell-wrapped-spinner",
            true,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(under_title(
            WORKING_TITLE,
            idle_frame(&WRAPPED_SPINNER_ROWS),
        ));
        advance(FOREIGN_WRITE_HOLD + IDLE_WITHOUT_STOP + CONFIRM_SPACING).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a shell-hosted idle box under a working title ended the turn"
        );
    }

    /// A CLI that keeps repainting holds its turn past the quiet limit: the
    /// limit counts silence, never the turn.
    #[tokio::test(start_paused = true)]
    async fn a_repainting_wrapped_spinner_outlasts_the_quiet_limit() {
        let h = start_record(
            "repainting-spinner",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        for repaint in 0..3 {
            h.chunk(under_title(
                WORKING_TITLE,
                idle_frame(&WRAPPED_SPINNER_ROWS),
            ));
            advance(Duration::from_secs(30)).await;
            assert_eq!(
                h.status(),
                SessionStatus::Working,
                "repaint {repaint} let the turn end"
            );
        }
    }

    /// A dead CLI's title holds the turn only until the terminal has been
    /// quiet for the limit; the check it scheduled then lets the idle box's
    /// window open.
    #[tokio::test(start_paused = true)]
    async fn a_dead_cli_under_a_stale_working_title_ends_after_the_quiet_limit() {
        let h = start_record("dead-cli-title", false, hook_record(SessionStatus::Working));
        pass_hold().await;
        h.chunk(under_title(
            WORKING_TITLE,
            idle_frame(&WRAPPED_SPINNER_ROWS),
        ));
        advance(TITLE_QUIET_LIMIT.saturating_sub(IDLE_WITHOUT_STOP)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the title let go of the turn before its quiet limit"
        );
        advance(IDLE_WITHOUT_STOP + IDLE_WITHOUT_STOP + CONFIRM_SPACING).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "a dead CLI's title held the turn past its quiet limit"
        );
    }

    /// A title that turns working inside the idle box's window cancels it: the
    /// screen is no longer evidence the turn ended.
    #[tokio::test(start_paused = true)]
    async fn a_title_turning_working_mid_window_cancels_it() {
        let h = start_record(
            "title-mid-window",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(IDLE_WITHOUT_STOP / 2).await;
        h.chunk(WORKING_TITLE.as_bytes().to_vec());
        advance(IDLE_WITHOUT_STOP + CONFIRM_SPACING).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a title turning working mid-window did not cancel it"
        );
    }

    /// A title that turns idle ends the turn the working title was holding,
    /// through the idle box's own window.
    #[tokio::test(start_paused = true)]
    async fn a_lost_stop_under_a_working_title_ends_when_the_title_turns() {
        let h = start_record(
            "title-turns-idle",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(under_title(
            WORKING_TITLE,
            idle_frame(&WRAPPED_SPINNER_ROWS),
        ));
        advance(IDLE_WITHOUT_STOP + CONFIRM_SPACING).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the working title did not hold the lost Stop"
        );
        h.chunk(IDLE_TITLE.as_bytes().to_vec());
        advance(IDLE_WITHOUT_STOP + CONFIRM_SPACING).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the idle title left a lost Stop standing"
        );
    }

    /// A lost `Stop` still lands: the same rows under the idle title demote.
    #[tokio::test(start_paused = true)]
    async fn a_wrapped_spinner_row_under_the_idle_title_still_demotes_a_lost_stop() {
        let h = start_record("wrapped-idle", false, hook_record(SessionStatus::Working));
        pass_hold().await;
        h.chunk(under_title(IDLE_TITLE, idle_frame(&WRAPPED_SPINNER_ROWS)));
        advance(FOREIGN_WRITE_HOLD + IDLE_WITHOUT_STOP + CONFIRM_SPACING).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "a lost Stop stayed Working"
        );
    }

    /// A shell's title over the idle box is no sign of a turn on its own.
    #[tokio::test(start_paused = true)]
    async fn a_shell_title_over_the_idle_box_does_not_hold_a_turn() {
        let h = start_record("shell-title", true, hook_record(SessionStatus::Working));
        pass_hold().await;
        h.chunk(under_title("\x1b]0;MINGW64:/c\x07", idle_frame(&[])));
        advance(FOREIGN_WRITE_HOLD + IDLE_WITHOUT_STOP + CONFIRM_SPACING).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "a shell title held a hook-driven turn"
        );
    }

    /// A real `Stop` waits for nothing: the working title over the idle box
    /// lands it in the short window.
    #[tokio::test(start_paused = true)]
    async fn a_real_stop_under_a_working_title_lands_in_the_short_window() {
        let h = start_record(
            "stop-under-title",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.hook(2, &main_stop(false));
        h.chunk(under_title(WORKING_TITLE, idle_frame(&[])));
        pass_hold().await;
        advance(ms(850)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the title delayed a held Stop"
        );
        assert!(!h.holds_turn_end(), "the held turn end stayed");
    }

    /// An Esc interrupt fires no `Stop`: the interrupt row under the working
    /// title keeps the short window.
    #[tokio::test(start_paused = true)]
    async fn an_interrupt_row_under_a_working_title_keeps_the_short_window() {
        let h = start_record(
            "interrupt-title",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(under_title(WORKING_TITLE, idle_frame(&[INTERRUPT_ROW])));
        advance(ms(850)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the title lengthened the interrupt's window"
        );
    }

    /// A hook-driven `Working` whose CLI has ended must still demote: the
    /// clear leaves `hook_driven` false, and the local state may never have
    /// seen the `Working` chunk.
    #[tokio::test(start_paused = true)]
    async fn plain_shell_demotes_a_working_record_after_its_hooks_end() {
        let h = start("shell-hooks-ended", true);
        h.set_record(|record| {
            record.status = SessionStatus::Working;
            record.hook_driven = false;
        });
        tokio::time::sleep(SHELL_IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "silence demotes a Working left by ended hooks"
        );
    }

    /// The shell classifier never overwrites a hook-reported prompt, and the
    /// prompt holds while its dialog shows.
    #[tokio::test(start_paused = true)]
    async fn hook_plain_shell_awaiting_input_holds_while_the_prompt_shows() {
        let h = start_record(
            "shell-hook-awaiting",
            true,
            hook_record(SessionStatus::AwaitingInput),
        );
        pass_hold().await;
        for tick in 0..50 {
            h.chunk(permission_frame());
            advance(ms(20)).await;
            assert_eq!(
                h.status(),
                SessionStatus::AwaitingInput,
                "repaint {tick}: the dialog's output moved a hook-reported prompt"
            );
        }
        advance(Duration::from_secs(10)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the dialog on screen did not hold the prompt"
        );
    }

    /// A hook-reported prompt in a plain shell dismissed without a hook leaves
    /// Claude's idle box on screen: it demotes once the window closes, and
    /// hooks still drive the shell.
    #[tokio::test(start_paused = true)]
    async fn hook_plain_shell_awaiting_input_demotes_once_the_prompt_is_gone() {
        let h = start_record(
            "shell-hook-prompt-gone",
            true,
            hook_record(SessionStatus::AwaitingInput),
        );
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(ms(299)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the idle box demoted before the window closed"
        );
        advance(ms(401)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the idle box demotes after the window"
        );
        assert!(h.hook_driven(), "Claude's idle box released the hooks");
    }

    /// Once a `SessionEnd` has released a plain shell from its hooks, the
    /// shell's own output promotes it again.
    #[tokio::test(start_paused = true)]
    async fn plain_shell_classifier_runs_again_after_hook_driven_clears() {
        let h = start("shell-hook-release-promote", true);
        pass_hold().await;
        set_hook_working(&h, 1);
        settle().await;
        h.set_record(|record| {
            record.hook_driven = false;
            record.status = SessionStatus::Idle;
            record.last_hook_time = Some(2);
        });
        settle().await;
        h.heartbeat(0);
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the shell classifier promotes on its own output again"
        );
    }

    /// A plain shell without hooks still promotes on its own output.
    #[tokio::test(start_paused = true)]
    async fn non_hook_plain_shell_promotes_on_its_own_output() {
        let h = start("shell-promote", true);
        pass_hold().await;
        assert_eq!(h.status(), SessionStatus::Idle);
        h.heartbeat(0);
        settle().await;
        assert_eq!(h.status(), SessionStatus::Working);
    }

    /// The shell's own prompt back after `claude` has ended: no Claude row.
    fn shell_prompt_frame() -> Vec<u8> {
        frame(&[
            "PS C:\\work> dir",
            "",
            "    Directory: C:\\work",
            "",
            "PS C:\\work> ",
        ])
    }

    /// A command's output in a shell, enough for the shell classifier to read
    /// as work.
    fn shell_output() -> Vec<u8> {
        let rows: Vec<String> = (0..12)
            .map(|n| format!("-a---          10/22/2026  9:{n:02} AM           {n}042 file{n}.txt"))
            .collect();
        let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
        frame(&rows)
    }

    #[tokio::test(start_paused = true)]
    async fn a_hook_driven_shell_with_the_spinner_stays_working() {
        let h = start_record("shell-spinner", true, hook_record(SessionStatus::Working));
        pass_hold().await;
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(Duration::from_secs(10)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a quiet shell still showing Claude's spinner demoted hook-reported work"
        );
        assert!(
            h.hook_driven(),
            "the spinner is Claude's: hooks still drive"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_hook_driven_shell_dialog_raises_one_attention() {
        let mut h = start_record("shell-dialog", true, hook_record(SessionStatus::Working));
        pass_hold().await;
        h.chunk(permission_frame());
        advance(ms(50)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the dialog raises the prompt at once"
        );
        assert_eq!(h.attentions(), 1, "one Attention on entry");
        for _ in 0..20 {
            h.chunk(permission_frame());
            advance(ms(20)).await;
        }
        assert_eq!(
            h.attentions(),
            0,
            "repaints of the same dialog raised Attention"
        );
        assert_eq!(h.status(), SessionStatus::AwaitingInput);
    }

    #[tokio::test(start_paused = true)]
    async fn a_shell_prompt_after_claude_ends_leaves_claude_mode() {
        let h = start_record(
            "shell-claude-ends",
            true,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(shell_prompt_frame());
        advance(ms(299)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the shell prompt demoted before the window closed"
        );
        assert!(h.hook_driven(), "hooks released before the window closed");
        advance(ms(401)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "claude's end leaves the turn"
        );
        assert!(!h.hook_driven(), "claude's end hands the shell back");
    }

    #[tokio::test(start_paused = true)]
    async fn a_killed_claude_with_no_session_end_returns_the_shell_classifier() {
        let h = start_record(
            "shell-claude-killed",
            true,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(shell_prompt_frame());
        advance(ms(700)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "claude's end leaves the turn"
        );
        assert!(!h.hook_driven(), "claude's end hands the shell back");
        h.chunk(shell_output());
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the shell classifier promotes on its own output again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_hook_arriving_mid_window_keeps_claude_mode() {
        let h = start_record(
            "shell-hook-mid-window",
            true,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(shell_prompt_frame());
        advance(ms(100)).await;
        h.set_record(main_stamp);
        advance(ms(690)).await;
        assert!(
            h.hook_driven(),
            "a window opened before a newer hook event still released the hooks"
        );
        assert_eq!(h.status(), SessionStatus::Working);
        // The turn the hook reported repaints Claude's spinner.
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(Duration::from_secs(10)).await;
        assert!(h.hook_driven(), "claude's spinner released the hooks");
        assert_eq!(h.status(), SessionStatus::Working);
    }

    #[tokio::test(start_paused = true)]
    async fn claude_typed_without_hooks_enters_claude_mode() {
        let h = start("shell-claude-no-hooks", true);
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_millis(1500)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "Claude's idle box did not end the shell classifier's work"
        );
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(ms(50)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "Claude's spinner promotes at once"
        );
        advance(Duration::from_secs(10)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a quiet spinner demoted a shell running claude"
        );
        assert!(!h.hook_driven(), "the screen set the hooks' flag");
    }

    #[tokio::test(start_paused = true)]
    async fn a_title_left_behind_does_not_hold_claude_mode() {
        let h = start_record(
            "shell-stale-title",
            true,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        let mut titled = "\x1b]0;◐ Fixing tests\x07".as_bytes().to_vec();
        titled.extend(shell_prompt_frame());
        h.chunk(titled);
        advance(ms(299)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the shell prompt demoted before the window closed"
        );
        advance(ms(401)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "a working title held claude mode"
        );
        assert!(!h.hook_driven(), "a working title kept the hooks");
    }

    /// The prompt row a shell prints under the last frame of a `claude` that
    /// was killed: nothing erases that frame.
    const PROMPT_ROW: &[u8] = b"\r\nPS C:\\work> ";

    /// A command run under whatever the grid shows: its output rows, then the
    /// prompt back.
    fn command_run() -> Vec<u8> {
        let rows: Vec<String> = (0..12)
            .map(|n| format!("-a---          10/22/2026  9:{n:02} AM           {n}042 file{n}.txt"))
            .collect();
        format!("\r\n{}\r\nPS C:\\work> ", rows.join("\r\n")).into_bytes()
    }

    #[tokio::test(start_paused = true)]
    async fn a_killed_claude_leaves_its_frame_but_the_prompt_ends_claude_mode() {
        let h = start_record(
            "shell-killed-frame",
            true,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(ms(100)).await;
        h.chunk(PROMPT_ROW.to_vec());
        advance(ms(299)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the prompt row demoted before the window closed"
        );
        assert!(h.hook_driven(), "hooks released before the window closed");
        advance(ms(401)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "claude's end leaves the turn"
        );
        assert!(
            !h.hook_driven(),
            "the prompt under claude's frame kept claude mode"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unknown_claude_screen_keeps_claude_mode() {
        let mut record = hook_record(SessionStatus::AwaitingInput);
        record.pending_input = Some(protocol::PendingInput::Permission {
            tool_name: "Bash".to_owned(),
            summary: "cargo build".to_owned(),
        });
        let h = start_record("shell-unknown-screen", true, record);
        pass_hold().await;
        h.chunk(frame(&[
            "  Opening the external editor…",
            "",
            "  Save and close the editor to continue",
        ]));
        advance(Duration::from_secs(3)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a screen no rule matches ended claude"
        );
        assert!(
            h.hook_driven(),
            "a screen no rule matches released the hooks"
        );
        assert!(
            pending_input(&h).is_some(),
            "the pending prompt was dropped"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_leftover_box_after_exit_does_not_reenter_claude_mode() {
        let h = start_record("shell-leftover-box", true, hook_record(SessionStatus::Idle));
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(ms(100)).await;
        h.chunk(PROMPT_ROW.to_vec());
        advance(ms(700)).await;
        assert!(!h.hook_driven(), "the prompt ends claude mode");
        for run in 0..2 {
            h.chunk(command_run());
            settle().await;
            assert_eq!(
                h.status(),
                SessionStatus::Working,
                "run {run}: the classifier did not promote on command output"
            );
            advance(SHELL_IDLE_AFTER + ms(500)).await;
            assert_eq!(
                h.status(),
                SessionStatus::Idle,
                "run {run}: the classifier's idle tick did not end the command"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_reattached_shell_at_a_dialog_raises_no_second_attention() {
        let mut h = start_record("shell-reattach-dialog", true, restored_prompt_record());
        let mut replay = shell_output();
        replay.extend(permission_frame());
        h.chunk(replay);
        advance(Duration::from_secs(3)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the replay moved the restored prompt"
        );
        assert!(
            pending_input(&h).is_some(),
            "the replay dropped the pending prompt"
        );
        assert_eq!(
            h.attentions(),
            0,
            "the replayed dialog raised Attention again"
        );
    }

    /// A hook event already on the record but not yet broadcast, landing
    /// between the leave window's last read and its write, refuses the write.
    #[test]
    fn a_hook_landing_before_the_leave_write_keeps_hook_driven() {
        let registry = SessionRegistry::new(scratch_dirs("leave-race"));
        registry.insert(hook_record(SessionStatus::Working));
        let read = record_status(&registry, SESSION).expect("the session is registered");
        {
            let record = registry.get(SESSION).expect("session registered");
            main_stamp(&mut record.lock().expect("record lock"));
        }
        assert_eq!(
            hand_shell_back(&registry, SESSION, read),
            None,
            "the leave write landed over a newer hook event"
        );
        let record = registry.get(SESSION).expect("session registered");
        let guard = record.lock().expect("record lock");
        assert!(guard.hook_driven, "the leave write released the hooks");
        assert_eq!(guard.status, SessionStatus::Working);
    }

    /// The `AgentExited` events `events` holds now.
    fn agent_exits(events: &mut broadcast::Receiver<crate::session::SessionEvent>) -> usize {
        let mut count = 0;
        while let Ok(event) = events.try_recv() {
            if matches!(event, crate::session::SessionEvent::AgentExited { .. }) {
                count += 1;
            }
        }
        count
    }

    /// The watcher typing the return to main into a recording PTY while the
    /// test plays Claude's TUI: after each key it feeds the captured frame
    /// that key brought up.
    mod view_return {
        use super::*;
        use crate::history::test_support::{FakeExit, recording_pty};
        use crate::pty::PtyHandle;
        use crate::screen::fixture_screen::screen_from;
        use crate::session::SessionEvent;
        use crate::typing_lane::Holder;
        use crate::view_drive::{DRIVE_LIMIT, KEY_WAIT};
        use agent_api::{KeyHolder, ViewReading};
        use protocol::SessionNoticeKind;

        const V174: &str = include_str!("screen/fixtures/v174-agent-view-heads-up-builtin.txt");
        const V175: &str = include_str!("screen/fixtures/v175-main-heads-up-panel-keys.txt");
        const V176: &str = include_str!("screen/fixtures/v176-main-heads-up-box-keys.txt");
        const V179: &str =
            include_str!("screen/fixtures/v179-agent-view-chat-in-main-draft-builtin.txt");
        const V180: &str =
            include_str!("screen/fixtures/v180-main-chat-in-main-draft-carried-builtin.txt");

        const DOWN: &[u8] = b"\x1b[B";
        const UP: &[u8] = b"\x1b[A";
        const ENTER: &[u8] = b"\r";

        /// v174's mode line, under the box of Sleeper A's view.
        const V174_MODE_LINE: &str =
            "  ⏵⏵ auto mode on (shift+tab to cycle) · ← 3 agents · ↓ to manage";

        /// The session's grid: as wide and tall as `screen_from` makes it
        /// for the 152 x 72 captures.
        const GRID: (u16, u16) = (156, 74);

        struct Tui {
            output: broadcast::Sender<Vec<u8>>,
            input: mpsc::UnboundedReceiver<Vec<u8>>,
            events: broadcast::Receiver<SessionEvent>,
            registry: Arc<SessionRegistry>,
            pty: Arc<PtyHandle>,
            _attention: mpsc::UnboundedReceiver<super::super::AttentionEvent>,
            _exit: FakeExit,
        }

        /// An agent session's watcher over a recording PTY, past its
        /// starting hold.
        async fn start_tui(tag: &str) -> Tui {
            let (pty, input, exit) = recording_pty();
            let mut record = test_record(SESSION);
            record.pty = Some(Arc::clone(&pty));
            let screen: SharedScreen = Arc::new(Mutex::new(Screen::new(GRID.0, GRID.1)));
            record.screen = Some(Arc::clone(&screen));
            let rules = rules_for(&record.agent);
            let registry = SessionRegistry::new(scratch_dirs(tag));
            registry.insert(record);
            let events = registry.subscribe();
            let chat = crate::chat_mirror::ChatMirror::new(
                Arc::clone(&registry),
                crate::hook_status::LiveLines::default(),
                Arc::new(tokio::sync::watch::channel(0usize).0),
            )
            .tap(SESSION, &agent_registry::default_agent());
            let (output, output_rx) = broadcast::channel(64);
            let (attention_tx, attention) = mpsc::unbounded_channel();
            watch(
                &registry,
                SESSION.to_string(),
                AgentStreams {
                    output: output_rx,
                    screen,
                    rules,
                    chat,
                },
                attention_tx,
            );
            pass_hold().await;
            Tui {
                output,
                input,
                events,
                registry,
                pty,
                _attention: attention,
                _exit: exit,
            }
        }

        impl Tui {
            /// Paint `capture` whole and let the watcher read it.
            async fn show(&self, capture: &str) {
                let rows: Vec<&str> = capture.lines().collect();
                self.output
                    .send(frame(&rows))
                    .expect("watcher still subscribed");
                advance(ms(100)).await;
            }

            /// The keys typed since the last call, one write each.
            fn typed(&mut self) -> Vec<Vec<u8>> {
                let mut keys = Vec::new();
                while let Ok(key) = self.input.try_recv() {
                    keys.push(key);
                }
                keys
            }

            fn notices(&mut self) -> Vec<SessionNoticeKind> {
                let mut notices = Vec::new();
                while let Ok(event) = self.events.try_recv() {
                    if let SessionEvent::Notice { session_id, notice } = event {
                        assert_eq!(session_id, SESSION);
                        notices.push(notice);
                    }
                }
                notices
            }
        }

        fn view_of(capture: &str) -> ViewReading {
            crate::screen::dialogs::read_view(
                &agent_registry::default_agent(),
                &screen_from(capture),
            )
            .expect("Claude's plugin reads views")
        }

        /// `capture` with `from` replaced by `to`, which must be in it.
        fn edited(capture: &str, from: &str, to: &str) -> String {
            assert!(capture.contains(from), "the capture shows {from:?}");
            capture.replacen(from, to, 1)
        }

        /// Built from v174: the Down that left Sleeper A's box has the
        /// panel holding the keys with its cursor on `main`, the agent still
        /// in view. No capture shows this frame.
        fn v174_panel_on_main() -> String {
            let panel = edited(V174, V174_MODE_LINE, "  ↑/↓ to select · Enter to view");
            let built = edited(&panel, "  ◯ main", "❯ ◯ main");
            let view = view_of(&built);
            assert_eq!(
                (
                    view.key_holder,
                    view.panel_cursor,
                    view.agent_in_view.as_deref()
                ),
                (KeyHolder::Panel, Some(0), Some("Sleeper A")),
                "the built frame reads as meant"
            );
            built
        }

        #[tokio::test(start_paused = true)]
        async fn a_heads_up_in_an_agents_view_returns_to_main_once() {
            let mut tui = start_tui("view-heads-up").await;
            tui.show(V174).await;
            assert_eq!(tui.typed(), [DOWN], "Down to the panel");
            tui.show(&v174_panel_on_main()).await;
            assert_eq!(tui.typed(), [ENTER], "Enter on main");
            tui.show(V175).await;
            assert_eq!(tui.typed(), [UP], "Up to the box");
            tui.show(V176).await;
            assert!(tui.typed().is_empty(), "main is in view");
            tui.show(V176).await;
            advance(KEY_WAIT * 2).await;
            assert!(tui.typed().is_empty(), "nothing more for the same notice");
            assert_eq!(tui.notices(), [], "no notice");
        }

        #[tokio::test(start_paused = true)]
        async fn the_watcher_skips_its_view_return_while_another_holds_the_lane() {
            let mut tui = start_tui("view-lane-held").await;
            let turn = tui
                .pty
                .typing()
                .try_take(Holder::ChatSend)
                .expect("the lane is free");
            tui.show(V174).await;
            advance(KEY_WAIT).await;
            assert!(tui.typed().is_empty(), "no key while a send holds the lane");
            drop(turn);
            advance(ms(600)).await;
            assert_eq!(
                tui.typed(),
                [DOWN],
                "the next reading after the lane is free returns to main"
            );
            assert_eq!(tui.pty.typing().holder(), Some(Holder::ViewReturn));
        }

        #[tokio::test(start_paused = true)]
        async fn the_watcher_releases_the_lane_when_idle() {
            let mut tui = start_tui("view-lane-idle").await;
            tui.show(V174).await;
            assert_eq!(tui.typed(), [DOWN], "Down to the panel");
            assert_eq!(
                tui.pty.typing().holder(),
                Some(Holder::ViewReturn),
                "the switch holds the lane between its keys"
            );
            tui.show(&v174_panel_on_main()).await;
            assert_eq!(tui.typed(), [ENTER], "Enter on main");
            tui.show(V175).await;
            assert_eq!(tui.typed(), [UP], "Up to the box");
            tui.show(V176).await;
            assert!(tui.typed().is_empty(), "main is in view");
            assert_eq!(
                tui.pty.typing().holder(),
                None,
                "the lane is given back once main is in view"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn the_watcher_drops_a_lane_held_too_long() {
            let mut tui = start_tui("view-lane-too-long").await;
            tui.show(V174).await;
            assert_eq!(tui.typed(), [DOWN], "Down to the panel");
            // Output that never settles: the switch never reads again, and
            // keeps the lane until its ceiling.
            let ceiling = DRIVE_LIMIT * 2 + KEY_WAIT;
            let step = ms(30);
            let mut held = Duration::ZERO;
            while held < ceiling.saturating_sub(Duration::from_secs(1)) {
                tui.output
                    .send(b"\x1b[0m".to_vec())
                    .expect("watcher still subscribed");
                advance(step).await;
                held += step;
            }
            assert_eq!(
                tui.pty.typing().holder(),
                Some(Holder::ViewReturn),
                "the lane is still held short of the ceiling"
            );
            while held < ceiling + Duration::from_secs(1) {
                tui.output
                    .send(b"\x1b[0m".to_vec())
                    .expect("watcher still subscribed");
                advance(step).await;
                held += step;
            }
            assert_eq!(
                tui.pty.typing().holder(),
                None,
                "the lane is given back past the ceiling"
            );
            assert!(tui.typed().is_empty(), "no key while the output runs on");
        }

        #[tokio::test(start_paused = true)]
        async fn the_read_after_a_key_survives_an_earlier_check() {
            let mut tui = start_tui("view-key-read").await;
            tui.show(V174).await;
            assert_eq!(tui.typed(), [DOWN], "Down to the panel");
            // Another writer moves the record: the watcher holds, then checks
            // the unchanged frame before KEY_WAIT has passed.
            tui.registry.update(SESSION, super::main_stamp);
            pass_hold().await;
            assert!(
                tui.typed().is_empty(),
                "the frame the key was pressed on is not read again yet"
            );
            advance(KEY_WAIT).await;
            assert_eq!(
                tui.typed(),
                [DOWN],
                "the read at KEY_WAIT, with no output, takes the drive's next key"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn no_return_with_main_in_view() {
            let mut tui = start_tui("view-main").await;
            tui.show(V176).await;
            advance(KEY_WAIT * 2).await;
            assert!(tui.typed().is_empty(), "the notice is already in main");
        }

        #[tokio::test(start_paused = true)]
        async fn a_held_stash_with_a_draft_does_not_switch() {
            // Built from v174: no capture shows a draft in an agent's box
            // with `› stashed` held, as v181 shows it in main.
            let with_draft = edited(
                V174,
                "❯\u{a0}Message @general-purpose…",
                "❯ my draft for the sleeper",
            );
            let held = edited(
                &with_draft,
                V174_MODE_LINE,
                &format!("{V174_MODE_LINE:<143}› stashed"),
            );
            let view = view_of(&held);
            assert!(
                view.stashed
                    && view.box_first_line.as_deref() == Some("my draft for the sleeper")
                    && view.agent_in_view.as_deref() == Some("Sleeper A"),
                "the built frame reads as meant: {view:?}"
            );
            let mut tui = start_tui("view-stash-held").await;
            tui.show(&held).await;
            advance(KEY_WAIT * 2).await;
            assert!(tui.typed().is_empty(), "no key, not even the stash");
            assert_eq!(tui.notices(), [SessionNoticeKind::WaitingInMain]);
        }

        #[tokio::test(start_paused = true)]
        async fn the_notices_own_draft_is_carried_unstashed() {
            let mut tui = start_tui("view-note-draft").await;
            tui.show(V179).await;
            assert_eq!(tui.typed(), [DOWN], "Down first, with no stash before it");
            // Built from v179 and v180: the panel on `main` with the note
            // still in Sleeper A's view, and main with the box holding the
            // keys again. No capture shows either.
            let panel = edited(
                V179,
                "  ⏵⏵ auto mode on (shift+tab to cycle)\n",
                "  ↑/↓ to select · Enter to view\n",
            );
            tui.show(&edited(&panel, "  ◯ main", "❯ ◯ main")).await;
            assert_eq!(tui.typed(), [ENTER]);
            tui.show(V180).await;
            assert_eq!(tui.typed(), [UP]);
            let box_keys = edited(
                V180,
                "  ↑/↓ to select\n",
                "  ⏵⏵ auto mode on (shift+tab to cycle)\n",
            );
            tui.show(&edited(&box_keys, "❯ ● main", "  ● main")).await;
            advance(KEY_WAIT * 2).await;
            assert!(
                tui.typed().is_empty(),
                "main is in view with the note in its box"
            );
            assert_eq!(tui.notices(), [], "no notice");
        }
    }

    #[test]
    fn hand_shell_back_on_an_agent_in_shell_record_announces_once() {
        let registry = SessionRegistry::new(scratch_dirs("hand-back-announce"));
        let mut record = hook_record(SessionStatus::Working);
        record.agent_in_shell = true;
        record.shell_label = Some("pwsh".to_owned());
        record.program_name = Some("claude".to_owned());
        record.work = crate::background_work::BackgroundWork {
            tasks: vec![crate::background_work::WorkTask {
                id: "b1".to_owned(),
                kind: agent_api::hook_vocabulary::StopTaskKind::Shell { command: None },
                description: "sleep".to_owned(),
                since: chrono::DateTime::from_timestamp_nanos(1),
            }],
            wakeups: Vec::new(),
        };
        registry.insert(record);
        let mut events = registry.subscribe();
        let read = record_status(&registry, SESSION).expect("the session is registered");
        assert!(hand_shell_back(&registry, SESSION, read).is_some());
        assert_eq!(agent_exits(&mut events), 1, "the agent's end is announced");
        {
            let record = registry.get(SESSION).expect("session registered");
            let guard = record.lock().expect("record lock");
            assert_eq!(guard.program_name.as_deref(), Some("pwsh"));
            assert_eq!(
                guard.work,
                crate::background_work::BackgroundWork::default(),
                "the agent's work ended with it"
            );
        }

        // The agent runs again without a `SessionStart` (no hooks) and ends.
        registry.update(SESSION, |rec| rec.hook_driven = true);
        let read = record_status(&registry, SESSION).expect("the session is registered");
        assert!(hand_shell_back(&registry, SESSION, read).is_some());
        assert_eq!(agent_exits(&mut events), 0, "one run announces once");
    }

    #[test]
    fn hand_shell_back_clears_an_agent_in_shell_records_title() {
        let registry = SessionRegistry::new(scratch_dirs("hand-back-title"));
        let mut record = hook_record(SessionStatus::Working);
        record.agent_in_shell = true;
        record.shell_label = Some("pwsh".to_owned());
        record.program_name = Some("claude".to_owned());
        record.terminal_title = Some("✳ Claude Code".to_owned());
        registry.insert(record);
        let read = record_status(&registry, SESSION).expect("the session is registered");
        assert!(hand_shell_back(&registry, SESSION, read).is_some());
        let record = registry.get(SESSION).expect("session registered");
        let guard = record.lock().expect("record lock");
        assert_eq!(guard.terminal_title, None, "the agent's title left with it");
    }

    /// A plain shell record running `pwsh`, the way its spawn left it.
    fn pwsh_plain_shell() -> SessionRecord {
        let mut record = test_record(SESSION);
        record.mode = SessionMode::PlainShell;
        record.program_name = Some("pwsh".to_owned());
        record
    }

    /// The tag the default agent's leaf shows.
    fn agent_tag() -> String {
        agent_registry::plugin(&agent_registry::default_agent())
            .expect("the default agent is registered")
            .descriptor()
            .tag
            .clone()
    }

    /// The record's promotion fields: agent in shell, shell label, hook
    /// agent and program name.
    fn promotion(
        h: &Harness,
    ) -> (
        bool,
        Option<String>,
        Option<protocol::AgentId>,
        Option<String>,
    ) {
        let rec = h.registry.get(SESSION).expect("session registered");
        let guard = rec.lock().expect("record lock");
        (
            guard.agent_in_shell,
            guard.shell_label.clone(),
            guard.hook_agent.clone(),
            guard.program_name.clone(),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn agent_shown_promotes_a_plain_shell() {
        let h = start_record("shell-promote", true, pwsh_plain_shell());
        let mut events = h.registry.subscribe();
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            promotion(&h),
            (
                true,
                Some("pwsh".to_owned()),
                Some(agent_registry::default_agent()),
                Some(agent_tag())
            ),
            "the screen's agent promotes the shell"
        );
        let snap = h
            .registry
            .get(SESSION)
            .map(|rec| rec.lock().expect("record lock").snapshot())
            .expect("session registered");
        assert_eq!(snap.mode, SessionMode::Interactive);
        assert_eq!(snap.agent, Some(agent_registry::default_agent()));
        assert_eq!(agent_exits(&mut events), 0, "no exit at promotion");
    }

    #[tokio::test(start_paused = true)]
    async fn agent_shown_again_after_exit_restarts_without_promoting_anew() {
        let h = start_record("shell-promote-again", true, pwsh_plain_shell());
        let mut events = h.registry.subscribe();
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(1)).await;
        h.set_record(|rec| rec.conversation_id = Some("c-first".to_owned()));
        settle().await;
        h.chunk(shell_prompt_frame());
        advance(Duration::from_secs(1)).await;
        assert_eq!(agent_exits(&mut events), 1, "the agent's end is announced");
        assert_eq!(
            promotion(&h),
            (
                true,
                Some("pwsh".to_owned()),
                Some(agent_registry::default_agent()),
                Some("pwsh".to_owned())
            ),
            "the shell stays promoted with its leaf on the shell"
        );

        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            promotion(&h),
            (
                true,
                Some("pwsh".to_owned()),
                Some(agent_registry::default_agent()),
                Some(agent_tag())
            ),
            "the later run names the agent again"
        );
        {
            let rec = h.registry.get(SESSION).expect("session registered");
            let guard = rec.lock().expect("record lock");
            assert_eq!(
                guard.conversation_id, None,
                "the later run looks for its own conversation"
            );
        }
        h.chunk(shell_prompt_frame());
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            agent_exits(&mut events),
            1,
            "the later run's end is announced"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn agent_shown_on_an_agent_spawned_shell_does_not_promote() {
        let mut record = test_record(SESSION);
        record.agent_in_shell = true;
        record.shell_label = Some("pwsh".to_owned());
        record.program_name = Some("pwsh".to_owned());
        let h = start_record_launching("shell-spawned-no-promote", record, ShellLaunch::AgentTyped);
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            promotion(&h),
            (true, Some("pwsh".to_owned()), None, Some("pwsh".to_owned())),
            "an agent session's record is left to its hooks"
        );
        let rec = h.registry.get(SESSION).expect("session registered");
        assert_eq!(
            rec.lock().expect("record lock").mode,
            SessionMode::Interactive
        );
    }

    /// Whether the discovery holds a waiter for the harness's session.
    fn waiting(h: &Harness) -> bool {
        h.registry.discovery().waiter(SESSION).is_some()
    }

    #[tokio::test(start_paused = true)]
    async fn agent_exit_deregisters_the_promotion_waiter() {
        let h = start_record("shell-promote-deregister", true, pwsh_plain_shell());
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(1)).await;
        assert!(promotion(&h).0, "the shell is promoted");
        // The waiter the promotion looks for its conversation with.
        let plugin = agent_registry::plugin(&agent_registry::default_agent())
            .expect("the default agent is registered");
        h.registry
            .discovery()
            .register(crate::conversations::discovery::Waiter::new(
                SESSION.to_owned(),
                crate::conversations::discovery::promotion_baseline(
                    plugin,
                    agent_api::ResolvedRows::default(),
                    std::path::PathBuf::from(r"D:\promotion-deregister"),
                    Utc::now(),
                    None,
                ),
            ));
        assert!(waiting(&h));

        h.chunk(shell_prompt_frame());
        advance(Duration::from_secs(1)).await;
        assert!(
            !waiting(&h),
            "the agent's end takes the promoted shell's waiter out"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn recovered_shell_with_a_hook_agent_is_not_promoted() {
        let mut record = pwsh_plain_shell();
        record.hook_agent = Some(agent_registry::default_agent());
        let h = start_record("shell-recovered-no-promote", true, record);
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            promotion(&h),
            (
                false,
                None,
                Some(agent_registry::default_agent()),
                Some("pwsh".to_owned())
            ),
            "a recovered shell's hooks own its agent"
        );
        let snap = h
            .registry
            .get(SESSION)
            .map(|rec| rec.lock().expect("record lock").snapshot())
            .expect("session registered");
        assert_eq!(snap.mode, SessionMode::PlainShell);
    }

    fn after_exit_frame(prompt: &str) -> Vec<u8> {
        frame(&["[fake-claude] ready (pid: 1)", "[fake-claude] bye", prompt])
    }

    /// Wider than the harness's 140-column grid, so it soft-wraps.
    fn wrapped_pwsh_prompt() -> String {
        format!(r"PS X:\{}delve> ", r"a-long-folder-name\".repeat(8))
    }

    #[tokio::test(start_paused = true)]
    async fn promoted_shell_sees_its_agent_end_under_a_soft_wrapped_pwsh_prompt() {
        let h = start_record("shell-promote-wrapped", true, pwsh_plain_shell());
        let mut events = h.registry.subscribe();
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(1)).await;
        assert!(promotion(&h).0, "the shell is promoted");
        h.chunk(after_exit_frame(&wrapped_pwsh_prompt()));
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            agent_exits(&mut events),
            1,
            "the agent's end under a soft-wrapped prompt is announced"
        );
        assert_eq!(
            promotion(&h).3,
            Some("pwsh".to_owned()),
            "the leaf names the shell again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn promoted_shell_sees_its_agent_end_under_git_bashs_two_row_prompt() {
        let mut record = pwsh_plain_shell();
        record.program_name = Some("bash".to_owned());
        let h = start_record("shell-promote-git-bash", true, record);
        let mut events = h.registry.subscribe();
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(1)).await;
        assert!(promotion(&h).0, "the shell is promoted");
        h.chunk(frame(&[
            "[fake-claude] ready (pid: 1)",
            "[fake-claude] bye",
            "",
            "lefto@LEFTOS-PC UCRT64 /c",
            "$ ",
        ]));
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            agent_exits(&mut events),
            1,
            "the agent's end under Git Bash's `$` row is announced"
        );
    }

    /// An exit frame whose prompt is Git Bash's `$` row.
    fn git_bash_exit_frame() -> Vec<u8> {
        frame(&[
            "[fake-claude] ready (pid: 1)",
            "[fake-claude] bye",
            "",
            "lefto@LEFTOS-PC UCRT64 /c",
            "$ ",
        ])
    }

    /// The record's terminal title.
    fn terminal_title(h: &Harness) -> Option<String> {
        let rec = h.registry.get(SESSION).expect("session registered");
        let guard = rec.lock().expect("record lock");
        guard.terminal_title.clone()
    }

    #[tokio::test(start_paused = true)]
    async fn promoted_shell_hand_back_clears_the_agents_terminal_title() {
        let mut record = pwsh_plain_shell();
        record.program_name = Some("bash".to_owned());
        let h = start_record("shell-title-hand-back", true, record);
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(1)).await;
        assert!(promotion(&h).0, "the shell is promoted");
        h.set_record(|rec| rec.terminal_title = Some("✳ Claude Code".to_owned()));
        h.chunk(git_bash_exit_frame());
        advance(Duration::from_secs(1)).await;
        assert_eq!(terminal_title(&h), None, "the agent's title left with it");
    }

    #[tokio::test(start_paused = true)]
    async fn promoted_shell_keeps_the_shells_title_set_after_the_agent_ends() {
        let mut record = pwsh_plain_shell();
        record.program_name = Some("bash".to_owned());
        let h = start_record("shell-title-after-end", true, record);
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(1)).await;
        assert!(promotion(&h).0, "the shell is promoted");
        h.set_record(|rec| rec.terminal_title = Some("✳ Claude Code".to_owned()));
        h.chunk(git_bash_exit_frame());
        advance(Duration::from_secs(1)).await;
        assert_eq!(terminal_title(&h), None, "the agent's title left with it");
        crate::osc_title::watch(
            &h.registry,
            SESSION.to_string(),
            h.output.subscribe(),
            false,
        );
        h.chunk(b"\x1b]0;MINGW64:/c\x07".to_vec());
        advance(Duration::from_millis(100)).await;
        assert_eq!(
            terminal_title(&h),
            Some("MINGW64:/c".to_owned()),
            "the shell's own title is kept"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn promoted_shell_keeps_a_title_the_shell_set_before_the_hand_back() {
        let mut record = pwsh_plain_shell();
        record.program_name = Some("bash".to_owned());
        let h = start_record("shell-title-before-hand-back", true, record);
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(1)).await;
        assert!(promotion(&h).0, "the shell is promoted");
        h.set_record(|rec| rec.terminal_title = Some("✳ Claude Code".to_owned()));
        h.chunk(git_bash_exit_frame());
        // The shell's prompt title lands before the hand-back's timer fires.
        h.set_record(|rec| rec.terminal_title = Some("MINGW64:/c".to_owned()));
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            terminal_title(&h),
            Some("MINGW64:/c".to_owned()),
            "the shell's own title is kept"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cleared_title_lets_the_same_title_through_again() {
        let h = start_record("shell-title-again", true, pwsh_plain_shell());
        pass_hold().await;
        crate::osc_title::watch(
            &h.registry,
            SESSION.to_string(),
            h.output.subscribe(),
            false,
        );
        h.chunk(b"\x1b]0;MINGW64:/c\x07".to_vec());
        advance(Duration::from_millis(100)).await;
        assert_eq!(terminal_title(&h), Some("MINGW64:/c".to_owned()));
        h.set_record(|rec| rec.terminal_title = None);
        h.chunk(b"\x1b]0;MINGW64:/c\x07".to_vec());
        advance(Duration::from_millis(100)).await;
        assert_eq!(
            terminal_title(&h),
            Some("MINGW64:/c".to_owned()),
            "a title cleared from the record comes back"
        );
    }

    /// The typed-agent case runs the same watcher and predicate.
    #[tokio::test(start_paused = true)]
    async fn typed_agent_sees_its_end_under_a_soft_wrapped_pwsh_prompt() {
        let mut record = test_record(SESSION);
        record.agent_in_shell = true;
        record.shell_label = Some("pwsh".to_owned());
        record.program_name = Some(agent_tag());
        let h = start_record_launching("shell-typed-wrapped", record, ShellLaunch::AgentTyped);
        let mut events = h.registry.subscribe();
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(1)).await;
        h.chunk(after_exit_frame(&wrapped_pwsh_prompt()));
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            agent_exits(&mut events),
            1,
            "the typed agent's end under a soft-wrapped prompt is announced"
        );
    }

    /// Nushell's prompt on Windows as measured through `ConPTY` (0.116.1,
    /// default config): marks ST-terminated, a right prompt after
    /// `133;P;k=r`. No pattern reads it.
    const NU_PROMPT: &str = "\x1b]133;A;k=i;click_events=1\x1b\\X:\\work> \x1b]133;P;k=r\x1b\\\x1b[41C 11/06/2026 11:01:02 AM\x1b]133;B\x1b\\";

    /// fish 4's prompt with its own marks.
    const FISH_PROMPT: &str = "\x1b]133;A\x07me@box ~> \x1b]133;B\x07";

    /// The shell at `prompt`, the user's `claude` typed, the command's start
    /// marked, and the agent's idle frame painted.
    fn marked_agent_start(prompt: &str) -> Vec<u8> {
        let mut bytes = format!("{prompt}claude\r\n\x1b]133;C\x07").into_bytes();
        bytes.extend(idle_frame(&[]));
        bytes
    }

    /// The agent's exit lines, the command's end marked, and `prompt`.
    fn marked_agent_end(prompt: &str) -> Vec<u8> {
        let mut bytes = frame(&["[fake-claude] ready (pid: 1)", "[fake-claude] bye", ""]);
        bytes.extend(format!("\x1b]133;D;0\x07{prompt}").into_bytes());
        bytes
    }

    #[tokio::test(start_paused = true)]
    async fn promoted_shell_sees_its_agent_end_at_a_nushell_prompt_by_its_prompt_mark() {
        let mut record = pwsh_plain_shell();
        record.program_name = Some("nu".to_owned());
        let h = start_record("shell-promote-nu-mark", true, record);
        let mut events = h.registry.subscribe();
        pass_hold().await;
        h.chunk(marked_agent_start(NU_PROMPT));
        advance(Duration::from_secs(1)).await;
        assert!(promotion(&h).0, "the shell is promoted");
        h.chunk(marked_agent_end(NU_PROMPT));
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            agent_exits(&mut events),
            1,
            "the agent's end at Nushell's marked prompt is announced"
        );
        assert_eq!(
            promotion(&h).3,
            Some("nu".to_owned()),
            "the leaf names the shell again"
        );
    }

    /// The typed-agent case runs the same watcher and predicate.
    #[tokio::test(start_paused = true)]
    async fn typed_agent_sees_its_end_at_a_fish_prompt_by_its_prompt_mark() {
        let mut record = test_record(SESSION);
        record.agent_in_shell = true;
        record.shell_label = Some("fish".to_owned());
        record.program_name = Some(agent_tag());
        let h = start_record_launching("shell-typed-fish-mark", record, ShellLaunch::AgentTyped);
        let mut events = h.registry.subscribe();
        pass_hold().await;
        h.chunk(marked_agent_start(FISH_PROMPT));
        advance(Duration::from_secs(1)).await;
        h.chunk(marked_agent_end(FISH_PROMPT));
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            agent_exits(&mut events),
            1,
            "the typed agent's end at fish's marked prompt is announced"
        );
    }

    /// The agent's last frame stays painted above the marked prompt: the end
    /// is seen, and the leftover frame does not promote the shell again.
    #[tokio::test(start_paused = true)]
    async fn an_agent_frame_left_above_a_marked_prompt_ends_the_run_and_is_not_promoted_again() {
        let mut record = pwsh_plain_shell();
        record.program_name = Some("fish".to_owned());
        let h = start_record("shell-promote-fish-leftover", true, record);
        let mut events = h.registry.subscribe();
        pass_hold().await;
        h.chunk(marked_agent_start(FISH_PROMPT));
        advance(Duration::from_secs(1)).await;
        assert!(promotion(&h).0, "the shell is promoted");
        let mut leftover = idle_frame(&[]);
        leftover.extend(format!("\r\n\x1b]133;D;0\x07{FISH_PROMPT}").into_bytes());
        h.chunk(leftover);
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            agent_exits(&mut events),
            1,
            "the agent's end under its leftover frame is announced"
        );
        advance(Duration::from_secs(3)).await;
        assert_eq!(
            promotion(&h).3,
            Some("fish".to_owned()),
            "the leftover frame restarted the agent"
        );
    }

    /// The prompt mark the shell painted before the agent started is not the
    /// agent's end.
    #[tokio::test(start_paused = true)]
    async fn a_prompt_mark_before_the_agents_command_start_does_not_end_it() {
        let mut record = pwsh_plain_shell();
        record.program_name = Some("nu".to_owned());
        let h = start_record("shell-nu-mark-before", true, record);
        let mut events = h.registry.subscribe();
        pass_hold().await;
        h.chunk(marked_agent_start(NU_PROMPT));
        advance(Duration::from_secs(1)).await;
        assert!(promotion(&h).0, "the shell is promoted");
        h.chunk(idle_frame(&["working on it"]));
        advance(Duration::from_secs(3)).await;
        assert_eq!(agent_exits(&mut events), 0, "the running agent was ended");
        assert_eq!(promotion(&h).3, Some(agent_tag()));
    }

    /// A shell that marks its prompt but no command (pwsh with no
    /// `PSReadLine`) keeps today's reading: its prompt mark alone ends nothing.
    #[tokio::test(start_paused = true)]
    async fn a_shell_marking_only_its_prompt_keeps_its_agent_running() {
        let h = start_record("shell-prompt-mark-only", true, pwsh_plain_shell());
        let mut events = h.registry.subscribe();
        pass_hold().await;
        let mut start = b"\x1b]133;A\x07PS C:\\work> \x1b]133;B\x07claude\r\n".to_vec();
        start.extend(idle_frame(&[]));
        h.chunk(start);
        advance(Duration::from_secs(1)).await;
        assert!(promotion(&h).0, "the shell is promoted");
        h.chunk(idle_frame(&["working on it"]));
        advance(Duration::from_secs(3)).await;
        assert_eq!(agent_exits(&mut events), 0, "the running agent was ended");
    }

    #[test]
    fn agent_up_reads_the_prompt_mark() {
        let registry = SessionRegistry::new(scratch_dirs("agent-up-mark"));
        let screen: SharedScreen = Arc::new(Mutex::new(Screen::new(COLS, ROWS)));
        let mut record = test_record(SESSION);
        record.screen = Some(Arc::clone(&screen));
        registry.insert(record);
        screen
            .lock()
            .expect("screen lock")
            .feed(&marked_agent_start(FISH_PROMPT));
        assert_eq!(
            crate::pty_state::agent_reading(&registry, SESSION),
            crate::pty_state::AgentReading::Ready,
            "Claude's idle prompt box after a marked command start did not count as the agent up"
        );
        screen
            .lock()
            .expect("screen lock")
            .feed(format!("\r\n\x1b]133;D;0\x07{FISH_PROMPT}").as_bytes());
        assert_eq!(
            crate::pty_state::agent_reading(&registry, SESSION),
            crate::pty_state::AgentReading::Starting { shell_prompt: true },
            "fish's marked prompt under the agent's leftover frame counted as the agent up"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn promoted_shell_without_a_program_name_still_sees_its_agent_end() {
        let mut record = pwsh_plain_shell();
        record.program_name = None;
        let h = start_record("shell-promote-unnamed", true, record);
        let mut events = h.registry.subscribe();
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(1)).await;
        h.set_record(|rec| rec.conversation_id = Some("c-first".to_owned()));
        settle().await;
        h.chunk(shell_prompt_frame());
        advance(Duration::from_secs(1)).await;
        assert_eq!(agent_exits(&mut events), 1, "the agent's end is announced");
        let (in_shell, shell_label, _, program_name) = promotion(&h);
        assert!(in_shell);
        assert!(shell_label.is_some(), "the shell has a label to go back to");
        assert_eq!(program_name, shell_label, "the leaf names the shell again");

        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(1)).await;
        let rec = h.registry.get(SESSION).expect("session registered");
        assert_eq!(
            rec.lock().expect("record lock").conversation_id,
            None,
            "the later run is a restart, which looks for its own conversation"
        );
    }

    #[test]
    fn redetected_sighting_with_no_change_does_not_broadcast() {
        let mut record = pwsh_plain_shell();
        record.agent_in_shell = true;
        record.shell_label = Some("pwsh".to_owned());
        record.hook_agent = Some(agent_registry::default_agent());
        record.program_name = Some(agent_tag());
        let registry = SessionRegistry::new(scratch_dirs("shell-redetected-quiet"));
        registry.insert(record);
        let mut events = registry.subscribe();

        super::agent_shown_in_shell(&registry, SESSION);

        assert!(
            events.try_recv().is_err(),
            "a sighting that changes nothing is not broadcast"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn leaving_claude_mode_clears_background() {
        let mut record = hook_record(SessionStatus::Idle);
        record.background = true;
        let h = start_record("shell-leave-background", true, record);
        pass_hold().await;
        h.chunk(shell_prompt_frame());
        advance(ms(700)).await;
        assert!(!h.hook_driven(), "the prompt ends claude mode");
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(!h.background(), "claude's background tasks outlived it");
    }

    #[tokio::test(start_paused = true)]
    async fn a_keystroke_echo_just_after_leaving_claude_mode_stays_idle() {
        let h = start_record(
            "shell-leave-keystroke",
            true,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(ms(50)).await;
        h.chunk(PROMPT_ROW.to_vec());
        advance(ms(400)).await;
        assert!(!h.hook_driven(), "the prompt ends claude mode");
        assert_eq!(h.status(), SessionStatus::Idle);
        h.input.send(1).expect("watcher still subscribed");
        h.chunk(b"d".to_vec());
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "a keystroke echo read claude's last frames as command output"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_shell_spots_claude_only_once_its_output_settles() {
        let mut h = start("shell-settled-entry", true);
        pass_hold().await;
        for _ in 0..40 {
            h.chunk(permission_frame());
            advance(ms(20)).await;
        }
        assert_ne!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a check inside a burst spotted Claude"
        );
        assert_eq!(h.attentions(), 0);
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the settled dialog did not raise the prompt"
        );
        assert_eq!(h.attentions(), 1, "one Attention on entry");
    }

    /// The line a spawn types into the hosting shell to start the agent, as
    /// pwsh echoes it: each token coloured, then the line break. The injector
    /// types it straight into the PTY, so no input pulse comes with it.
    fn launch_echo() -> Vec<u8> {
        let tokens = [
            "Set-Location",
            "-LiteralPath",
            "'C:\\work';",
            "&",
            "'C:\\Users\\me\\.local\\bin\\claude.exe'",
            "'--settings'",
            "'C:\\Users\\me\\AppData\\Roaming\\leftos\\rustling-tulip\\config\\sessions\\9f3831cc-bd1d-4674-b069-09ab9a68d2c2\\hook-settings.json'",
            "'--session-id'",
            "'bcae3a5b-f792-4e72-b15d-0654793af913'",
        ];
        let line: Vec<String> = tokens
            .iter()
            .map(|t| format!("\x1b[93m{t}\x1b[0m"))
            .collect();
        format!("{}\r\n", line.join(" ")).into_bytes()
    }

    /// A shell the spawn typed an agent's launch line into.
    fn agent_typed(tag: &str) -> Harness {
        let mut record = test_record(SESSION);
        record.agent_in_shell = true;
        start_record_launching(tag, record, ShellLaunch::AgentTyped)
    }

    #[tokio::test(start_paused = true)]
    async fn an_agent_launch_typed_into_the_shell_is_not_a_turn() {
        let h = agent_typed("shell-launch-not-a-turn");
        pass_hold().await;
        h.chunk(launch_echo());
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the launch line's echo read as a command running"
        );
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(2)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert_eq!(h.status_seq(), 0, "the launch wrote a status");
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_command_typed_into_a_plain_shell_still_works() {
        let h = start("shell-long-command", true);
        pass_hold().await;
        h.chunk(launch_echo());
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a plain shell's long echo no longer reads as work"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_launched_agent_on_screen_reports_its_own_status() {
        let mut h = agent_typed("shell-launch-agent-shown");
        pass_hold().await;
        h.chunk(launch_echo());
        settle().await;
        for _ in 0..40 {
            h.chunk(permission_frame());
            advance(ms(20)).await;
        }
        assert_ne!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a check inside a burst spotted Claude"
        );
        assert_eq!(h.attentions(), 0);
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the settled dialog did not raise the prompt"
        );
        assert_eq!(h.attentions(), 1, "one Attention on entry");
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the agent's spinner did not set the status"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_launch_that_never_shows_an_agent_returns_to_the_classifier() {
        let h = agent_typed("shell-launch-lapses");
        pass_hold().await;
        h.chunk(launch_echo());
        settle().await;
        advance(LAUNCH_HOLD + Duration::from_secs(1)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        h.chunk(vec![b'x'; 400]);
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the classifier did not take the shell back after the launch hold"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn hooks_taking_over_a_launch_report_hook_status() {
        let h = agent_typed("shell-launch-hooks");
        pass_hold().await;
        h.chunk(launch_echo());
        settle().await;
        set_hook_working(&h, 1);
        settle().await;
        assert_eq!(h.status(), SessionStatus::Working, "the hooks' status");
        h.set_record(|record| {
            record.hook_driven = false;
            record.status = SessionStatus::Idle;
            record.last_hook_time = Some(2);
        });
        settle().await;
        assert_eq!(h.status(), SessionStatus::Idle, "the hooks' status");
        h.chunk(vec![b'x'; 400]);
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the launch still held the classifier off after the hooks took over"
        );
    }

    /// A hook-reported prompt dismissed without a hook (from another client)
    /// leaves the idle box on screen: it demotes once the window closes.
    #[tokio::test(start_paused = true)]
    async fn hook_awaiting_input_demotes_on_the_idle_box_after_the_window() {
        let h = start_record(
            "hook-prompt-gone",
            false,
            hook_record(SessionStatus::AwaitingInput),
        );
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(ms(299)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the idle box demoted before the window closed"
        );
        advance(ms(401)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the idle box demotes after the window"
        );
    }

    /// A hook-reported prompt holds while its permission dialog shows.
    #[tokio::test(start_paused = true)]
    async fn hook_awaiting_input_holds_while_the_permission_dialog_shows() {
        let h = start_record(
            "hook-prompt-holds",
            false,
            hook_record(SessionStatus::AwaitingInput),
        );
        pass_hold().await;
        h.chunk(permission_frame());
        advance(Duration::from_secs(10)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the permission dialog on screen did not hold the prompt"
        );
    }

    /// A hook-reported prompt holds while its `AskUserQuestion` dialog shows.
    #[tokio::test(start_paused = true)]
    async fn hook_awaiting_input_holds_while_the_question_dialog_shows() {
        let h = start_record(
            "hook-chevron-holds",
            false,
            hook_record(SessionStatus::AwaitingInput),
        );
        pass_hold().await;
        h.chunk(question_frame());
        advance(Duration::from_secs(10)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the question dialog on screen did not hold the prompt"
        );
    }

    /// A record reattached `AwaitingInput` with its pending prompt, which
    /// hooks no longer drive until their next event.
    fn restored_prompt_record() -> SessionRecord {
        let mut record = test_record(SESSION);
        record.status = SessionStatus::AwaitingInput;
        record.pending_input = Some(protocol::PendingInput::Permission {
            tool_name: "Bash".to_owned(),
            summary: "cargo build".to_owned(),
        });
        record
    }

    fn pending_input(h: &Harness) -> Option<protocol::PendingInput> {
        let rec = h.registry.get(SESSION).expect("session registered");
        rec.lock().expect("record lock").pending_input.clone()
    }

    #[tokio::test(start_paused = true)]
    async fn a_restored_prompt_is_cleared_when_the_screen_leaves_awaiting_input() {
        let h = start_record("restored-prompt-cleared", false, restored_prompt_record());
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(ms(700)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the idle box ends the wait"
        );
        assert_eq!(pending_input(&h), None, "the prompt left with the wait");
    }

    /// A question (`question_frame`, no quote on screen) holds the wait; Esc
    /// cancels it and the idle box draws under a reply that quotes the
    /// question's footer. No hook fires for Esc, so that idle frame must end
    /// the wait, the quoted footer above the box notwithstanding.
    #[tokio::test(start_paused = true)]
    async fn esc_over_a_dialog_with_a_quoted_footer_ends_awaiting_input() {
        let h = start_record("esc-quoted-footer", false, restored_prompt_record());
        pass_hold().await;
        h.chunk(question_frame());
        advance(ms(800)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the question holds the wait"
        );
        h.chunk(idle_frame(&[
            "● The footer under a question reads:",
            "",
            "  Enter to select · ↑/↓ to navigate · Esc to cancel",
            "",
        ]));
        advance(ms(800)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the idle box under a quoted footer ends the wait"
        );
        assert_eq!(pending_input(&h), None, "the prompt left with the wait");
    }

    #[tokio::test(start_paused = true)]
    async fn a_restored_prompt_is_cleared_when_the_plain_shell_classifier_leaves_awaiting_input() {
        let h = start_record(
            "shell-restored-prompt-cleared",
            true,
            restored_prompt_record(),
        );
        pass_hold().await;
        h.heartbeat(0);
        settle().await;
        assert_eq!(h.status(), SessionStatus::Working, "shell output promotes");
        assert_eq!(pending_input(&h), None, "the prompt left with the wait");
    }

    /// A session with no hook events follows its screen alone: a heartbeat
    /// title moves nothing, a spinner leaves its prompt after the window, and
    /// the idle box then ends the turn after its own.
    #[tokio::test(start_paused = true)]
    async fn a_session_without_hooks_follows_its_screen() {
        let h = start_from("screen-only", false, SessionStatus::AwaitingInput);
        pass_hold().await;
        h.heartbeat(0);
        advance(Duration::from_secs(10)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a title heartbeat moved a session without hooks"
        );
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(ms(700)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the spinner leaves the prompt"
        );
        h.chunk(idle_frame(&[]));
        advance(ms(700)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the idle box ends the turn"
        );
    }

    /// The status record `id`'s registry holds, failing when it is gone.
    fn status_in(registry: &SessionRegistry) -> SessionStatus {
        let record = registry.get(SESSION).expect("session registered");
        record.lock().expect("record lock").status
    }

    /// A hook event that lands between the screen check's read of the record
    /// and its write must keep the hook's status: the write is refused because
    /// the hook-driven flag or the hook stamp moved since that read.
    #[test]
    fn a_hook_landing_after_the_read_keeps_its_awaiting_input() {
        let registry = SessionRegistry::new(scratch_dirs("hook-race-agent"));
        registry.insert(test_record(SESSION));
        let read = record_status(&registry, SESSION).expect("the session is registered");
        assert!(!read.hook_driven, "the read saw no hooks");
        // A hook event lands after the read and before the screen writes.
        registry.update(SESSION, |record| {
            record.status = SessionStatus::AwaitingInput;
            record.hook_driven = true;
            record.last_hook_time = Some(7);
        });
        let (attention, mut receiver) = mpsc::unbounded_channel();
        let wrote = apply_screen_status(
            &registry,
            SESSION,
            read,
            ScreenWrite {
                next: SessionStatus::Working,
                shown: false,
            },
            &attention,
        );
        assert!(!wrote, "the guard let a stale read write");
        assert_eq!(
            status_in(&registry),
            SessionStatus::AwaitingInput,
            "the screen overwrote a hook prompt that landed after its read"
        );
        assert!(
            receiver.try_recv().is_err(),
            "a refused write raised Attention"
        );
    }

    /// A status another writer moved since the read (an exit, a chat answer)
    /// is never overwritten, and the refused write is not broadcast.
    #[test]
    fn a_status_moved_since_the_read_refuses_the_screen_write() {
        let registry = SessionRegistry::new(scratch_dirs("status-moved"));
        let mut record = test_record(SESSION);
        record.status = SessionStatus::Working;
        registry.insert(record);
        let read = record_status(&registry, SESSION).expect("the session is registered");
        for moved in [SessionStatus::Stopped, SessionStatus::AwaitingInput] {
            registry.update(SESSION, |record| record.status = moved);
            let mut events = registry.subscribe();
            let (attention, _receiver) = mpsc::unbounded_channel();
            let wrote = apply_screen_status(
                &registry,
                SESSION,
                read,
                ScreenWrite {
                    next: SessionStatus::Idle,
                    shown: false,
                },
                &attention,
            );
            assert!(!wrote, "the guard let a write over {moved:?} land");
            assert_eq!(
                status_in(&registry),
                moved,
                "the screen overwrote {moved:?}"
            );
            assert!(
                events.try_recv().is_err(),
                "a refused write over {moved:?} was broadcast"
            );
        }
    }

    /// The plain shell's write carries no stamp check at all: a hook event
    /// landing after the classifier's read must survive that write too.
    #[test]
    fn a_hook_landing_after_the_plain_shell_read_keeps_its_awaiting_input() {
        let registry = SessionRegistry::new(scratch_dirs("hook-race-shell"));
        registry.insert(test_record(SESSION));
        let read = record_status(&registry, SESSION).expect("the session is registered");
        assert!(!read.hook_driven, "the read saw no hooks");
        registry.update(SESSION, |record| {
            record.status = SessionStatus::AwaitingInput;
            record.hook_driven = true;
            record.last_hook_time = Some(7);
        });
        let mut state = State::Idle;
        on_plain_shell_transition(&registry, SESSION, &mut state, State::Working, read);
        assert_eq!(
            status_in(&registry),
            SessionStatus::AwaitingInput,
            "the shell classifier's unguarded write overwrote a hook prompt"
        );
    }

    /// The guard refuses only a stale read: a write taken with the record
    /// unchanged since the read still lands.
    #[test]
    fn a_write_after_the_read_still_applies_without_a_hook() {
        let registry = SessionRegistry::new(scratch_dirs("classifier-write"));
        registry.insert(test_record(SESSION));
        let read = record_status(&registry, SESSION).expect("the session is registered");
        let (attention, _receiver) = mpsc::unbounded_channel();
        let wrote = apply_screen_status(
            &registry,
            SESSION,
            read,
            ScreenWrite {
                next: SessionStatus::Working,
                shown: false,
            },
            &attention,
        );
        assert!(wrote);
        assert_eq!(status_in(&registry), SessionStatus::Working);
    }

    #[tokio::test(start_paused = true)]
    async fn a_quiet_screen_with_the_spinner_keeps_hook_working() {
        let h = start_record("quiet-spinner", false, hook_record(SessionStatus::Working));
        pass_hold().await;
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(Duration::from_secs(10)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a quiet screen still showing the spinner demoted hook-reported work"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_interrupt_row_demotes_working_after_the_window() {
        let h = start_record("interrupt-row", false, hook_record(SessionStatus::Working));
        pass_hold().await;
        h.chunk(idle_frame(&[INTERRUPT_ROW]));
        advance(ms(299)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the interrupt row demoted before the window closed"
        );
        advance(ms(401)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the interrupt row demotes once the window closes"
        );
    }

    /// The first half of a repaint still shows the dialog the turn already
    /// answered; the second half paints the spinner over it.
    #[tokio::test(start_paused = true)]
    async fn a_half_drawn_frame_raises_no_attention() {
        let mut h = start_from("half-drawn", false, SessionStatus::Working);
        pass_hold().await;
        h.chunk(permission_frame());
        advance(CHECK_DEBOUNCE / 2).await;
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(Duration::from_secs(1)).await;
        assert_eq!(h.attentions(), 0, "a half-drawn frame raised Attention");
        assert_eq!(h.status(), SessionStatus::Working);
    }

    /// A chat answer clears the prompt at once; the dialog still on the
    /// grid, repainted before Claude redraws, must not raise it again.
    #[tokio::test(start_paused = true)]
    async fn a_foreign_clear_holds_off_a_stale_dialog() {
        let mut h = start_record(
            "foreign-clear-hold",
            false,
            hook_record(SessionStatus::AwaitingInput),
        );
        h.chunk(permission_frame());
        pass_hold().await;
        h.registry.update_if_changed(SESSION, |record| {
            record.status = SessionStatus::Idle;
            record.pending_input = None;
            true
        });
        settle().await;
        advance(CHECK_DEBOUNCE + ms(10)).await;
        // Arrow keys move the cursor in the dialog Claude still paints, past
        // the hold the clear began.
        for _ in 0..60 {
            h.chunk(permission_frame());
            advance(ms(50)).await;
        }
        assert_eq!(h.attentions(), 0, "a stale dialog raised Attention");
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "a stale dialog undid the clear"
        );
    }

    /// After a clear, a dialog that goes away (the idle box) and
    /// comes back (a new prompt) raises `AwaitingInput` again, once.
    #[tokio::test(start_paused = true)]
    async fn a_dialog_after_the_box_returns_raises_again() {
        let mut h = start_record(
            "dialog-returns",
            false,
            hook_record(SessionStatus::AwaitingInput),
        );
        h.chunk(permission_frame());
        pass_hold().await;
        h.registry.update_if_changed(SESSION, |record| {
            record.status = SessionStatus::Idle;
            record.pending_input = None;
            true
        });
        settle().await;
        for _ in 0..20 {
            h.chunk(permission_frame());
            advance(ms(50)).await;
        }
        assert_eq!(h.attentions(), 0, "the stale dialog raised Attention");
        h.chunk(idle_frame(&[]));
        advance(ms(100)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        h.chunk(permission_frame());
        advance(ms(100)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a new dialog after the box did not raise the prompt"
        );
        assert_eq!(h.attentions(), 1, "one Attention for the new dialog");
    }

    /// A newer hook stamp inside a window refuses its publish; the idle box
    /// still on screen after the hold opens a fresh window that publishes.
    #[tokio::test(start_paused = true)]
    async fn a_newer_hook_stamp_mid_window_cancels_the_publish() {
        let h = start_record(
            "stamp-mid-window",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(ms(2500)).await;
        h.set_record(main_stamp);
        advance(ms(690)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a window opened before a newer hook event still published"
        );
        advance(FOREIGN_WRITE_HOLD + IDLE_WITHOUT_STOP + CONFIRM_SPACING).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the idle box after the hold did not demote"
        );
    }

    /// A foreign write with the idle box already on screen and no output
    /// after it: the screen is read once the hold ends.
    #[tokio::test(start_paused = true)]
    async fn a_quiet_idle_box_after_a_foreign_write_still_demotes() {
        let h = start_record(
            "quiet-after-hold",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(ms(100)).await;
        h.chunk(idle_frame(&[]));
        advance(ms(2500)).await;
        h.set_record(main_stamp);
        advance(ms(690)).await;
        assert_eq!(h.status(), SessionStatus::Working, "read inside the hold");
        advance(FOREIGN_WRITE_HOLD + IDLE_WITHOUT_STOP + CONFIRM_SPACING).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "a quiet idle box was never read after the hold"
        );
    }

    /// A max-wait check under continuous output catches a half-drawn frame
    /// whose bottom still holds the answered dialog's footer; the next chunk
    /// paints the spinner over it. That one read raises nothing.
    #[tokio::test(start_paused = true)]
    async fn a_half_frame_under_continuous_output_raises_no_attention() {
        let mut h = start_from("half-frame-burst", false, SessionStatus::Working);
        pass_hold().await;
        for tick in 0..20 {
            // The chunk just before the 150 ms max-wait check.
            if tick == 7 {
                h.chunk(permission_frame());
            } else {
                h.chunk(idle_frame(&[SPINNER_ROW]));
            }
            advance(ms(20)).await;
        }
        advance(ms(200)).await;
        assert_eq!(h.attentions(), 0, "a half frame raised Attention");
        assert_eq!(h.status(), SessionStatus::Working);
    }

    /// The grid resized inside a window: the frame under the new size is
    /// only read after a hold, so nothing publishes until hold and window.
    #[tokio::test(start_paused = true)]
    async fn a_resize_holds_before_the_next_read() {
        let h = start_record("resize-hold", false, hook_record(SessionStatus::Working));
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(ms(100)).await;
        crate::sync::lock(&h.screen).resize(160, ROWS);
        // The window's next check, at 190 ms, finds the new size and holds
        // until 890 ms.
        advance(ms(780)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a check read the grid across a resize"
        );
        advance(FOREIGN_WRITE_HOLD + IDLE_WITHOUT_STOP + CONFIRM_SPACING).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the idle box after the hold did not demote"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_exit_mid_window_writes_nothing() {
        let h = start_record(
            "exit-mid-window",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(ms(100)).await;
        h.set_record(|record| record.status = SessionStatus::Stopped);
        let mut events = h.registry.subscribe();
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(3)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Stopped,
            "the screen revived a stopped session"
        );
        assert!(
            events.try_recv().is_err(),
            "the watcher broadcast an update for a stopped session"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_detailed_transcript_overlay_holds_working_and_restarts_the_count() {
        let h = start_record(
            "transcript-overlay",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(transcript_overlay_frame());
        advance(Duration::from_secs(10)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the overlay demoted hook-reported work"
        );
        // A window the overlay breaks counts again from its next check.
        h.chunk(idle_frame(&[]));
        advance(ms(200)).await;
        h.chunk(transcript_overlay_frame());
        advance(ms(60)).await;
        h.chunk(idle_frame(&[]));
        advance(IDLE_WITHOUT_STOP).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the window kept its count across the overlay"
        );
        advance(ms(240)).await;
        assert_eq!(h.status(), SessionStatus::Idle, "the new window publishes");
    }

    #[tokio::test(start_paused = true)]
    async fn a_non_hook_dialog_raises_one_attention() {
        let mut h = start("dialog-attention", false);
        pass_hold().await;
        h.chunk(permission_frame());
        advance(ms(50)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the dialog raises the prompt at once"
        );
        assert_eq!(h.attentions(), 1, "one Attention on entry");
        for _ in 0..20 {
            h.chunk(permission_frame());
            advance(ms(20)).await;
        }
        assert_eq!(
            h.attentions(),
            0,
            "repaints of the same dialog raised Attention"
        );
        // The `PermissionRequest` hook for the same prompt: hook_status
        // raises Attention only when `apply_status` enters the prompt.
        h.set_record(|record| {
            record.hook_driven = true;
            record.last_hook_time = Some(1);
            record.pending_input = Some(protocol::PendingInput::Permission {
                tool_name: "Bash".to_owned(),
                summary: "python -c \"print(6*7)\"".to_owned(),
            });
            assert!(
                !super::apply_status(record, SessionStatus::AwaitingInput),
                "the hook found the prompt already raised"
            );
        });
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            h.attentions(),
            0,
            "the hook's prompt raised a second Attention"
        );
        assert_eq!(h.status(), SessionStatus::AwaitingInput);
        assert!(pending_input(&h).is_some(), "the hook's prompt is pending");
    }

    /// A main-agent `Stop` reporting a running shell when `background`.
    fn main_stop(background: bool) -> serde_json::Value {
        let tasks = if background {
            serde_json::json!([{"id": "b1", "type": "shell", "status": "running"}])
        } else {
            serde_json::json!([])
        };
        serde_json::json!({
            "hook_event_name": "Stop",
            "last_assistant_message": "done",
            "background_tasks": tasks,
            "session_crons": [],
        })
    }

    /// A subagent's tool call.
    fn subagent_tool() -> serde_json::Value {
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "agent_id": "a0123456789abcdef",
            "agent_type": "Explore",
            "tool_name": "Read",
        })
    }

    impl Harness {
        /// Apply the hook `body` at hook time `time`, as the hook route does.
        fn hook(&self, time: u128, body: &serde_json::Value) {
            let report = agent_registry::plugin(&agent_registry::default_agent())
                .expect("Claude is registered")
                .map_hook_event(body, None)
                .expect("the payload maps to a report");
            self.set_record(|record| {
                crate::hook_status::apply_to_record(record, Some(time), &report);
            });
        }

        fn holds_turn_end(&self) -> bool {
            let rec = self.registry.get(SESSION).expect("session registered");
            rec.lock().expect("record lock").turn_end.is_some()
        }

        /// How many status changes the record has seen.
        fn status_seq(&self) -> u64 {
            let rec = self.registry.get(SESSION).expect("session registered");
            rec.lock().expect("record lock").status_seq
        }
    }

    /// The statuses and background flags `events` carried for the session,
    /// in order.
    fn updates(
        events: &mut broadcast::Receiver<crate::session::SessionEvent>,
    ) -> Vec<(SessionStatus, bool)> {
        let mut seen = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let crate::session::SessionEvent::Updated(snapshot, _) = event {
                seen.push((snapshot.status, snapshot.background));
            }
        }
        seen
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_stop_never_leaves_working() {
        let h = start_record("blocked-stop", false, hook_record(SessionStatus::Working));
        pass_hold().await;
        let seq = h.status_seq();
        h.hook(2, &main_stop(false));
        h.chunk(idle_frame(&[STOP_HOOKS_ROW]));
        pass_hold().await;
        for tick in 0..3 {
            let row = format!("✢ Simmering… (running Stop hooks… {tick}/2 · 5s · ↓ 621 tokens)");
            h.chunk(idle_frame(&[row.as_str()]));
            advance(ms(300)).await;
            assert_eq!(h.status(), SessionStatus::Working, "hooks row {tick}");
            assert!(h.holds_turn_end(), "the hooks row resolves nothing");
        }
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(Duration::from_secs(2)).await;
        assert_eq!(h.status(), SessionStatus::Working);
        let tool = serde_json::json!({"hook_event_name": "PostToolUse", "tool_name": "Read"});
        h.hook(3, &tool);
        settle().await;
        assert!(!h.holds_turn_end(), "the next main tool event drops it");
        assert_eq!(h.status(), SessionStatus::Working);
        assert_eq!(h.status_seq(), seq, "a status was written");
    }

    #[tokio::test(start_paused = true)]
    async fn a_spinner_between_the_hooks_row_and_the_idle_box_keeps_the_held_background() {
        let h = start_record(
            "spinner-between",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.hook(2, &main_stop(true));
        h.chunk(idle_frame(&[STOP_HOOKS_ROW]));
        pass_hold().await;
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(Duration::from_secs(1)).await;
        assert!(h.holds_turn_end(), "a spinner resolves nothing");
        h.chunk(background_shell_frame());
        advance(Duration::from_secs(1)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(h.background(), "the held flag lands");
    }

    #[tokio::test(start_paused = true)]
    async fn the_ceiling_waits_for_a_settled_frame() {
        let h = start_record(
            "ceiling-settled",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        let seq = h.status_seq();
        h.hook(2, &main_stop(false));
        h.chunk(idle_frame(&[STOP_HOOKS_ROW]));
        advance(ms(59_900)).await;
        // Chunks closer than the debounce, on past the ceiling and past
        // `CHECK_MAX_WAIT` beyond it, so checks run on unsettled frames.
        for chunk in 0..12 {
            h.chunk(frame(&["half a frame"]));
            advance(ms(30)).await;
            assert!(h.holds_turn_end(), "chunk {chunk}: read an unsettled frame");
        }
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(ms(100)).await;
        assert!(!h.holds_turn_end(), "the settled frame decides");
        assert_eq!(h.status(), SessionStatus::Working);
        assert_eq!(h.status_seq(), seq, "a status was written");
    }

    #[tokio::test(start_paused = true)]
    async fn a_working_title_is_not_no_evidence() {
        let h = start_record("working-title", false, hook_record(SessionStatus::Working));
        let mut titled = "\x1b]0;◐ Fixing tests\x07".as_bytes().to_vec();
        titled.extend(frame(&["something no rule reads"]));
        h.chunk(titled);
        pass_hold().await;
        h.hook(2, &main_stop(false));
        pass_hold().await;
        advance(Duration::from_secs(1)).await;
        assert_eq!(h.status(), SessionStatus::Working);
        assert!(h.holds_turn_end(), "the title says the turn runs");
        advance(Duration::from_secs(60)).await;
        assert_eq!(h.status(), SessionStatus::Working, "the ceiling landed it");
        assert!(!h.holds_turn_end(), "the ceiling drops it");
    }

    #[tokio::test(start_paused = true)]
    async fn a_title_that_stops_working_lands_no_evidence() {
        let h = start_record("title-stops", false, hook_record(SessionStatus::Working));
        let mut titled = "\x1b]0;◐ Fixing tests\x07".as_bytes().to_vec();
        titled.extend(frame(&["something no rule reads"]));
        h.chunk(titled);
        pass_hold().await;
        h.hook(2, &main_stop(true));
        pass_hold().await;
        assert!(h.holds_turn_end(), "the title says the turn runs");
        let mut titled = "\x1b]0;✳ Fixing tests\x07".as_bytes().to_vec();
        titled.extend(frame(&["something no rule reads"]));
        h.chunk(titled);
        advance(ms(100)).await;
        assert_eq!(h.status(), SessionStatus::Idle, "waited for the ceiling");
        assert!(h.background(), "the held flag lands");
        assert!(!h.holds_turn_end());
    }

    #[tokio::test(start_paused = true)]
    async fn an_overlay_at_the_ceiling_writes_no_idle() {
        let h = start_record(
            "ceiling-overlay",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        let seq = h.status_seq();
        h.hook(2, &main_stop(true));
        h.chunk(idle_frame(&[STOP_HOOKS_ROW]));
        pass_hold().await;
        h.chunk(transcript_overlay_frame());
        advance(Duration::from_secs(61)).await;
        assert_eq!(h.status(), SessionStatus::Working);
        assert_eq!(h.status_seq(), seq, "a status was written");
        assert!(h.holds_turn_end(), "the overlay keeps it");
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(2)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the frame after the overlay lands it"
        );
        assert!(!h.holds_turn_end());
    }

    #[tokio::test(start_paused = true)]
    async fn an_overlay_at_the_ceiling_keeps_the_held_flag_for_its_idle() {
        let h = start_record(
            "ceiling-overlay-flag",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.hook(2, &main_stop(true));
        h.chunk(idle_frame(&[STOP_HOOKS_ROW]));
        pass_hold().await;
        h.chunk(transcript_overlay_frame());
        advance(Duration::from_secs(61)).await;
        assert_eq!(h.status(), SessionStatus::Working);
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(5)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(h.background(), "the Stop reported running work");
    }

    /// A frame no state rule reads, with a background shell on its footer.
    fn unread_frame_with_a_shell() -> Vec<u8> {
        let rule = rule();
        frame(&[
            "something no rule reads",
            rule.as_str(),
            "  ⏵⏵ bypass permissions on · 1 shell · ← for agents · ↓ to manage",
        ])
    }

    #[tokio::test(start_paused = true)]
    async fn a_no_evidence_landing_keeps_the_footers_background_flag() {
        let h = start_record(
            "no-evidence-footer",
            false,
            hook_record(SessionStatus::Working),
        );
        h.chunk(unread_frame_with_a_shell());
        pass_hold().await;
        h.hook(2, &main_stop(false));
        settle().await;
        advance(ms(800)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(h.background(), "the footer shows a shell");
    }

    #[tokio::test(start_paused = true)]
    async fn a_ceiling_landing_keeps_the_footers_background_flag() {
        let h = start_record("ceiling-footer", false, hook_record(SessionStatus::Working));
        pass_hold().await;
        h.hook(2, &main_stop(false));
        h.chunk(idle_frame(&[STOP_HOOKS_ROW]));
        pass_hold().await;
        h.chunk(unread_frame_with_a_shell());
        advance(Duration::from_secs(61)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(h.background(), "the footer shows a shell");
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_turn_end_under_an_overlay_still_hands_a_shell_back() {
        let h = start_record(
            "ceiling-overlay-hand-back",
            true,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.hook(2, &main_stop(true));
        h.chunk(idle_frame(&[STOP_HOOKS_ROW]));
        pass_hold().await;
        h.chunk(transcript_overlay_frame());
        advance(Duration::from_secs(61)).await;
        assert!(h.holds_turn_end(), "the overlay keeps it");
        assert!(h.hook_driven());
        let mut with_prompt = transcript_overlay_frame();
        with_prompt.extend_from_slice("\r\nPS C:\\Users\\me> ".as_bytes());
        h.chunk(with_prompt);
        advance(Duration::from_secs(2)).await;
        assert!(!h.hook_driven(), "the agent's end hands the shell back");
    }

    #[tokio::test(start_paused = true)]
    async fn an_interrupt_row_ends_a_hook_driven_turn_without_the_long_window() {
        let h = start_record(
            "interrupt-short-window",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(idle_frame(&[INTERRUPT_ROW]));
        advance(ms(850)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
    }

    #[tokio::test(start_paused = true)]
    async fn a_bare_task_subject_row_does_not_end_a_hook_driven_turn() {
        for (tag, row) in [
            ("bare-subject", "✽ Build the thing and check it by hand…"),
            (
                "bare-phase",
                "✽ Build unsigned installer and ask owner for iPhone hand check (RT-369)… · responding",
            ),
        ] {
            let h = start_record(tag, false, hook_record(SessionStatus::Working));
            pass_hold().await;
            h.chunk(idle_frame(&["❯ go", "", row]));
            advance(ms(1100)).await;
            assert_eq!(
                h.status(),
                SessionStatus::Working,
                "{row}: the row ended the turn a second in"
            );
        }
    }

    /// A lost `Stop` cannot leave a session stuck: an idle box that holds
    /// through the window still ends the turn.
    #[tokio::test(start_paused = true)]
    async fn a_bare_idle_box_still_ends_a_hook_driven_turn_with_no_stop() {
        let h = start_record("lost-stop", false, hook_record(SessionStatus::Working));
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(5)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
    }

    #[tokio::test(start_paused = true)]
    async fn a_compact_over_a_background_shell_never_reads_idle_without_the_flag() {
        let mut record = hook_record(SessionStatus::Idle);
        record.background = true;
        let h = start_record("compact-shell", false, record);
        let mut events = h.registry.subscribe();
        pass_hold().await;
        h.chunk(box_frame(
            &["✶ Compacting conversation… (2s)"],
            &["  ⏵⏵ bypass permissions on · 1 shell · esc to interrupt · ↓ to manage"],
        ));
        advance(ms(200)).await;
        assert_eq!(h.status(), SessionStatus::Working, "the spinner promotes");
        h.chunk(background_shell_frame());
        advance(Duration::from_secs(10)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(h.background(), "the screen shows the shell");
        let mut idle_without_flag = 0;
        while let Ok(event) = events.try_recv() {
            if let crate::session::SessionEvent::Updated(snapshot, _) = event
                && snapshot.status == SessionStatus::Idle
                && !snapshot.background
            {
                idle_without_flag += 1;
            }
        }
        assert_eq!(
            idle_without_flag, 0,
            "a broadcast read Idle with no background work"
        );
    }

    #[test]
    fn handing_the_shell_back_keeps_the_held_alert() {
        let registry = SessionRegistry::new(scratch_dirs("hand-back-alert"));
        let mut record = hook_record(SessionStatus::Working);
        record.agent_in_shell = true;
        record.turn_end = Some(crate::session::TurnEnd {
            event_time: 1,
            background: true,
            alert: Some(crate::session::TurnAlert {
                kind: None,
                notification: "held".to_owned(),
                source: protocol::SummarySource::Model,
            }),
            held_at: tokio::time::Instant::now(),
        });
        registry.insert(record);
        let mut events = registry.subscribe();
        let read = record_status(&registry, SESSION).expect("the session is registered");
        assert!(hand_shell_back(&registry, SESSION, read).is_some());
        let mut alerts = 0;
        let mut landed = None;
        while let Ok(event) = events.try_recv() {
            match event {
                crate::session::SessionEvent::WaitingAlert { .. } => alerts += 1,
                crate::session::SessionEvent::Updated(snapshot, _) => {
                    landed = Some((snapshot.status, snapshot.background));
                }
                _ => {}
            }
        }
        assert_eq!(alerts, 1, "the alert goes out");
        assert_eq!(
            landed,
            Some((SessionStatus::Idle, false)),
            "the agent's work ended with it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unblocked_stop_lands_idle_when_the_hooks_row_clears() {
        let h = start_record("unblocked-stop", false, hook_record(SessionStatus::Working));
        pass_hold().await;
        let mut events = h.registry.subscribe();
        h.hook(2, &main_stop(true));
        h.chunk(idle_frame(&[STOP_HOOKS_ROW]));
        pass_hold().await;
        h.chunk(idle_frame(&[STOP_HOOKS_ROW]));
        advance(ms(300)).await;
        assert_eq!(h.status(), SessionStatus::Working);
        assert!(!h.background(), "the flag waits for Idle");
        h.chunk(background_shell_frame());
        advance(Duration::from_secs(1)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(h.background(), "the held flag lands");
        assert!(!h.holds_turn_end());
        let landed = updates(&mut events)
            .into_iter()
            .find(|(status, _)| *status == SessionStatus::Idle);
        assert_eq!(
            landed,
            Some((SessionStatus::Idle, true)),
            "Idle and its flag land in one update"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_flag_a_held_turn_end_landed_is_not_the_screens_to_clear() {
        let h = start_record("landed-flag", false, hook_record(SessionStatus::Working));
        pass_hold().await;
        h.hook(2, &main_stop(true));
        h.chunk(idle_frame(&[STOP_HOOKS_ROW]));
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(1)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        advance(Duration::from_secs(5)).await;
        assert!(h.background(), "the screen cleared a flag it never set");
    }

    #[tokio::test(start_paused = true)]
    async fn no_evidence_of_hooks_lands_idle_after_the_hold() {
        let overlay = transcript_overlay_frame();
        let unmatched = frame(&["something no rule reads"]);
        for (tag, screen) in [("overlay", overlay), ("no-match", unmatched)] {
            let mut h = start_record(
                &format!("no-evidence-{tag}"),
                false,
                hook_record(SessionStatus::Working),
            );
            h.chunk(screen);
            pass_hold().await;
            h.hook(2, &main_stop(true));
            settle().await;
            assert_eq!(h.status(), SessionStatus::Working, "{tag}: held");
            advance(ms(600)).await;
            assert_eq!(h.status(), SessionStatus::Working, "{tag}: inside the hold");
            advance(ms(200)).await;
            assert_eq!(h.status(), SessionStatus::Idle, "{tag}");
            assert!(h.background(), "{tag}: the held flag lands");
            assert!(!h.holds_turn_end(), "{tag}");
            assert_eq!(h.attentions(), 0, "{tag}: resolution raises no Attention");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_spinner_before_the_hooks_row_keeps_the_hold() {
        let h = start_record("stale-spinner", false, hook_record(SessionStatus::Working));
        h.chunk(idle_frame(&[SPINNER_ROW]));
        pass_hold().await;
        h.hook(2, &main_stop(true));
        pass_hold().await;
        advance(Duration::from_secs(1)).await;
        assert!(h.holds_turn_end(), "a spinner alone drops nothing");
        h.chunk(idle_frame(&[STOP_HOOKS_ROW]));
        advance(ms(300)).await;
        assert!(h.holds_turn_end());
        h.chunk(background_shell_frame());
        advance(Duration::from_secs(1)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(h.background(), "the held flag lands");
    }

    #[tokio::test(start_paused = true)]
    async fn the_ceiling_never_lands_on_a_spinner() {
        let h = start_record(
            "ceiling-hooks-spinner",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        let seq = h.status_seq();
        h.hook(2, &main_stop(false));
        h.chunk(idle_frame(&[STOP_HOOKS_ROW]));
        pass_hold().await;
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(Duration::from_secs(61)).await;
        assert_eq!(h.status(), SessionStatus::Working);
        assert_eq!(h.status_seq(), seq, "a status was written");

        let h = start_record(
            "ceiling-spinner",
            false,
            hook_record(SessionStatus::Working),
        );
        h.chunk(idle_frame(&[SPINNER_ROW]));
        pass_hold().await;
        let seq = h.status_seq();
        h.hook(2, &main_stop(false));
        advance(Duration::from_secs(59)).await;
        assert!(h.holds_turn_end(), "only the ceiling decides this one");
        advance(Duration::from_secs(2)).await;
        assert!(!h.holds_turn_end(), "the ceiling drops the held turn end");
        assert_eq!(h.status(), SessionStatus::Working);
        assert_eq!(h.status_seq(), seq, "a status was written");
    }

    #[tokio::test(start_paused = true)]
    async fn the_ceiling_lands_idle_otherwise() {
        let mut h = start_record("ceiling-hooks", false, hook_record(SessionStatus::Working));
        pass_hold().await;
        h.hook(2, &main_stop(true));
        h.chunk(idle_frame(&[STOP_HOOKS_ROW]));
        advance(Duration::from_secs(59)).await;
        assert_eq!(h.status(), SessionStatus::Working, "before the ceiling");
        advance(Duration::from_secs(2)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(h.background(), "the held flag lands");
        assert!(!h.holds_turn_end());
        assert_eq!(h.attentions(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn subagent_stamps_do_not_delay_the_landing() {
        let h = start_record(
            "subagent-stamps",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.hook(2, &main_stop(false));
        h.chunk(idle_frame(&[STOP_HOOKS_ROW]));
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        settle().await;
        for time in 3..12 {
            h.hook(time, &subagent_tool());
            advance(ms(100)).await;
        }
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "subagent stamps starved the window"
        );
    }

    /// A main agent's tool call.
    fn main_tool() -> serde_json::Value {
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Read",
        })
    }

    /// A prompt answered by key, with subagents firing hooks all along: the
    /// idle box ends it as if they were quiet.
    #[tokio::test(start_paused = true)]
    async fn subagent_stamps_do_not_delay_an_answered_prompt_ending() {
        let mut h = start_record("subagent-stamps-prompt", false, pending_record());
        pass_hold().await;
        h.chunk(question_frame());
        advance(ms(100)).await;
        h.chunk(idle_frame(&[]));
        settle().await;
        for time in 2..32 {
            h.hook(time, &subagent_tool());
            advance(ms(100)).await;
        }
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "subagent stamps held the answered prompt"
        );
        assert!(!has_pending(&h), "the answered prompt stayed pending");
        assert_eq!(h.attentions(), 0, "no new Attention");
    }

    /// An Esc with no `Stop`, with subagents firing hooks all along: the idle
    /// box still ends the turn.
    #[tokio::test(start_paused = true)]
    async fn subagent_stamps_do_not_starve_working_to_idle_after_esc() {
        let h = start_record(
            "subagent-stamps-esc",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        settle().await;
        for time in 2..52 {
            h.hook(time, &subagent_tool());
            advance(ms(100)).await;
        }
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "subagent stamps starved the turn's end"
        );
    }

    /// A subagent's permission request.
    fn subagent_permission_hook() -> serde_json::Value {
        serde_json::json!({
            "hook_event_name": "PermissionRequest",
            "agent_id": "a0123456789abcdef",
            "tool_name": "Write",
            "tool_input": {"file_path": "notes.txt"},
        })
    }

    /// A wait the screen raised with no prompt pending, answered by key; a
    /// subagent's prompt lands inside the leave window, before its dialog
    /// paints. The window's read predates that prompt, so it must not clear
    /// it.
    #[tokio::test(start_paused = true)]
    async fn a_subagent_prompt_on_a_screen_raised_wait_is_not_dropped_by_a_stale_read() {
        let mut h = start_record(
            "subagent-prompt-stale-read",
            false,
            hook_record(SessionStatus::Idle),
        );
        pass_hold().await;
        h.chunk(permission_frame());
        advance(ms(100)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the screen raised it"
        );
        assert!(!has_pending(&h), "no hook named the prompt");
        h.chunk(idle_frame(&[]));
        advance(ms(100)).await;
        h.hook(2, &subagent_permission_hook());
        advance(ms(400)).await;
        h.chunk(permission_frame());
        advance(Duration::from_secs(2)).await;
        assert_eq!(h.status(), SessionStatus::AwaitingInput);
        let rec = h.registry.get(SESSION).expect("session registered");
        let pending = rec.lock().expect("record lock").pending_input.clone();
        assert!(
            matches!(pending, Some(protocol::PendingInput::Permission { .. })),
            "the subagent's prompt was dropped: {pending:?}"
        );
        assert_eq!(h.attentions(), 1, "one Attention for the wait");
    }

    /// A main agent's hook inside a window still refuses its publish; the
    /// idle box still on screen after the hold opens a fresh window.
    #[tokio::test(start_paused = true)]
    async fn a_main_hook_mid_window_still_cancels_the_publish() {
        let h = start_record(
            "main-hook-mid-window",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(ms(2500)).await;
        h.hook(2, &main_tool());
        advance(ms(690)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a window opened before a main hook event still published"
        );
        advance(FOREIGN_WRITE_HOLD + IDLE_WITHOUT_STOP + CONFIRM_SPACING).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the idle box after the hold did not demote"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stop_hooks_running_keep_idle_and_background() {
        let mut record = hook_record(SessionStatus::Idle);
        record.background = true;
        let h = start_record("stop-hooks", false, record);
        pass_hold().await;
        h.chunk(idle_frame(&[STOP_HOOKS_ROW]));
        advance(Duration::from_secs(10)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the Stop hooks' spinner promoted"
        );
        assert!(h.background(), "the Stop hooks' spinner cleared background");
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(ms(50)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a turn's spinner promotes at once"
        );
        assert!(!h.background(), "the promotion clears background");
    }

    #[tokio::test(start_paused = true)]
    async fn subagent_rows_and_a_working_title_keep_idle() {
        let h = start("subagent-rows", false);
        pass_hold().await;
        h.chunk(subagent_panel_frame());
        advance(Duration::from_secs(2)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "subagent rows read as a turn"
        );
        assert!(h.background(), "running subagent rows set no background");
        let mut titled = "\x1b]0;◐ Fixing tests\x07".as_bytes().to_vec();
        titled.extend(idle_frame(&[]));
        h.chunk(titled);
        advance(Duration::from_secs(2)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "a working title over the idle box promoted"
        );
    }

    fn status_since(h: &Harness) -> Option<chrono::DateTime<Utc>> {
        let rec = h.registry.get(SESSION).expect("session registered");
        rec.lock().expect("record lock").status_since
    }

    #[tokio::test(start_paused = true)]
    async fn a_screen_sets_background_beside_idle_without_hooks() {
        let mut h = start("screen-background", false);
        let since = status_since(&h);
        pass_hold().await;
        h.chunk(background_shell_frame());
        advance(Duration::from_secs(2)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(h.background(), "the footer's shell set no background");
        assert_eq!(h.attentions(), 0, "background raised Attention");
        assert_eq!(status_since(&h), since, "background moved status_since");
        assert!(!h.hook_driven(), "background made the session hook-driven");
    }

    #[tokio::test(start_paused = true)]
    async fn a_hook_driven_idle_gets_background_from_the_screen_after_a_restart() {
        let h = start_record(
            "hook-background-restart",
            false,
            hook_record(SessionStatus::Idle),
        );
        pass_hold().await;
        h.chunk(background_shell_frame());
        advance(Duration::from_secs(2)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(h.background(), "the footer's shell set no background");
        let read = record_status(&h.registry, SESSION).expect("session registered");
        assert!(read.hook_driven, "the screen write cleared hook_driven");
        let stamp = h
            .registry
            .get(SESSION)
            .expect("session registered")
            .lock()
            .expect("record lock")
            .last_hook_time;
        assert_eq!(stamp, Some(1), "the screen write moved the stamp");
    }

    const SUBAGENT_IN_VIEW: &str =
        include_str!("screen/fixtures/v2318-agent-view-subagent-running.txt");
    const FINISHED_SUBAGENT_IN_VIEW: &str =
        include_str!("screen/fixtures/v2318-agent-view-subagent-finished-main-woken.txt");

    fn capture_frame(capture: &str) -> Vec<u8> {
        let rows: Vec<&str> = capture.lines().collect();
        frame(&rows)
    }

    #[tokio::test(start_paused = true)]
    async fn a_subagent_in_view_keeps_the_main_agents_idle_and_background() {
        // The main agent's Stop listed the running subagent.
        let mut record = hook_record(SessionStatus::Idle);
        record.background = true;
        let h = start_record("agent-view-spinner", false, record);
        pass_hold().await;
        h.chunk(capture_frame(SUBAGENT_IN_VIEW));
        advance(Duration::from_secs(2)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the subagent's spinner was read as the main agent's turn"
        );
        assert!(
            h.background(),
            "the subagent's view cleared the main Stop's flag"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_subagent_in_view_never_ends_the_main_agents_turn() {
        // UserPromptSubmit: the subagent's report woke the main agent.
        let h = start_record(
            "agent-view-finished",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(capture_frame(FINISHED_SUBAGENT_IN_VIEW));
        advance(Duration::from_secs(2)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the subagent's idle box ended the main agent's turn"
        );
    }

    /// `SUBAGENT_IN_VIEW` with an `AskUserQuestion` dialog in the box: the
    /// dialog block from `c31-ask-q1.txt` (its rule, the question tab, the
    /// question and its options, its rule and key row), then the rule the
    /// agent in view rides on.
    fn subagent_view_with_a_question() -> Vec<u8> {
        let dialog: Vec<&str> = include_str!("screen/fixtures/c31-ask-q1.txt")
            .lines()
            .collect();
        let tab = dialog
            .iter()
            .position(|row| row.trim_start().starts_with("←  ☐"))
            .expect("the capture draws the question's tabs");
        let keys = dialog
            .iter()
            .position(|row| row.trim_start().starts_with("Enter to select"))
            .expect("the capture draws the question's keys");
        let mut rows: Vec<&str> = SUBAGENT_IN_VIEW.lines().take(17).collect();
        rows.extend(&dialog[tab - 2..=keys]);
        rows.push(
            SUBAGENT_IN_VIEW
                .lines()
                .find(|row| row.trim_matches('─').trim() == "Agent C sleep")
                .expect("the view labels the rule with the agent in view"),
        );
        frame(&rows)
    }

    /// Either agent's dialog needs the user: a question drawn in a subagent's
    /// view still raises the session to `AwaitingInput`.
    #[tokio::test(start_paused = true)]
    async fn a_dialog_in_a_subagents_view_still_awaits_input() {
        let h = start_from("agent-view-dialog", false, SessionStatus::Working);
        pass_hold().await;
        h.chunk(subagent_view_with_a_question());
        advance(Duration::from_secs(2)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a dialog in a subagent's view left the session working"
        );
    }

    /// `SUBAGENT_IN_VIEW` with another agent's name on its rule.
    fn another_subagent_in_view() -> Vec<u8> {
        capture_frame(&SUBAGENT_IN_VIEW.replace("Agent C sleep", "Agent D sleep"))
    }

    /// A session asking for the owner, with `SUBAGENT_IN_VIEW`'s dialog read
    /// once the watcher's first hold has passed.
    async fn start_on_a_subagents_dialog(tag: &str) -> Harness {
        let h = start_record(tag, false, pending_record());
        pass_hold().await;
        h.chunk(subagent_view_with_a_question());
        advance(Duration::from_secs(2)).await;
        h
    }

    /// A hook-driven record in `AwaitingInput` with its prompt pending.
    fn pending_record() -> SessionRecord {
        let mut record = hook_record(SessionStatus::AwaitingInput);
        record.pending_input = Some(protocol::PendingInput::Other {
            message: "approve?".to_owned(),
        });
        record
    }

    /// Whether the record still holds a pending prompt.
    fn has_pending(h: &Harness) -> bool {
        let rec = h.registry.get(SESSION).expect("session registered");
        rec.lock().expect("record lock").pending_input.is_some()
    }

    /// No subagent hook ends a subagent's prompt, so the screen does: the
    /// reading after its dialog, in the same view, shows it answered.
    #[tokio::test(start_paused = true)]
    async fn an_answered_subagent_dialog_ends_the_status() {
        let h = start_on_a_subagents_dialog("agent-view-answered").await;
        h.chunk(capture_frame(SUBAGENT_IN_VIEW));
        advance(Duration::from_secs(2)).await;
        assert_eq!(h.status(), SessionStatus::Idle, "the answer ended nothing");
        assert!(!has_pending(&h), "the answered prompt stayed pending");
    }

    /// A subagent's view over main's pending question hides the question
    /// without answering it.
    #[tokio::test(start_paused = true)]
    async fn peeking_at_a_subagent_keeps_mains_question() {
        let h = start_record("agent-view-peek", false, pending_record());
        pass_hold().await;
        h.chunk(question_frame());
        advance(Duration::from_secs(2)).await;
        h.chunk(capture_frame(SUBAGENT_IN_VIEW));
        advance(Duration::from_secs(2)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "looking at a subagent ended main's question"
        );
        assert!(has_pending(&h), "looking at a subagent dropped the prompt");
    }

    /// Moving from one subagent's dialog to another subagent's view leaves
    /// the dialog up, out of sight.
    #[tokio::test(start_paused = true)]
    async fn leaving_a_subagents_dialog_for_another_view_does_not_end_it() {
        let h = start_on_a_subagents_dialog("agent-view-left").await;
        h.chunk(another_subagent_in_view());
        advance(Duration::from_secs(2)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "leaving the subagent's view ended its dialog"
        );
        assert!(has_pending(&h), "leaving the view dropped the prompt");
    }

    /// A dialog read again in the same view is still up.
    #[tokio::test(start_paused = true)]
    async fn a_subagent_dialog_still_up_keeps_counting() {
        let h = start_on_a_subagents_dialog("agent-view-still-up").await;
        h.chunk(subagent_view_with_a_question());
        advance(Duration::from_secs(2)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a dialog still up stopped counting"
        );
        assert!(has_pending(&h), "a dialog still up dropped its prompt");
    }

    /// A permission dialog in a subagent's view with `footer` under its
    /// options: the options block of `v23-amend-open.txt`, then the rule the
    /// agent in view rides on.
    fn subagent_permission(footer: &str) -> Vec<u8> {
        let capture: Vec<&str> = include_str!("screen/fixtures/v23-amend-open.txt")
            .lines()
            .collect();
        let kind = capture
            .iter()
            .position(|row| row.trim() == "Bash command")
            .expect("the capture draws the dialog's kind");
        let label = SUBAGENT_IN_VIEW
            .lines()
            .find(|row| row.trim_matches('─').trim() == "Agent C sleep")
            .expect("the view labels the rule with the agent in view");
        let mut rows = capture[kind - 1..capture.len() - 1].to_vec();
        rows.push(footer);
        rows.push(label);
        frame(&rows)
    }

    /// The rules' and the plugin's readings of `bytes` on a fresh grid.
    fn read_frame(
        bytes: &[u8],
    ) -> (
        crate::screen::rules::Reading,
        Option<agent_api::ViewReading>,
    ) {
        let mut screen = Screen::new(COLS, ROWS);
        screen.feed(bytes);
        let agent = agent_registry::default_agent();
        (
            rules_for(&agent).read(&screen),
            crate::screen::dialogs::read_view(&agent, &screen),
        )
    }

    /// Tab and Down take the hint off a permission dialog's footer, and its
    /// row then changes while the dialog stays up: nothing has answered it.
    #[tokio::test(start_paused = true)]
    async fn a_permission_dialog_without_its_hint_keeps_counting_under_a_subagent() {
        for footer in [" Esc to cancel", " Esc to cancel · ctrl+e to explain"] {
            let hinted = subagent_permission(" Esc to cancel · Tab to amend");
            let unhinted = subagent_permission(footer);
            let (_, view) = read_frame(&unhinted);
            let view = view.expect("the plugin reads the view");
            assert!(view.agent_in_view.is_some(), "{footer}: no agent in view");
            assert!(view.dialog, "{footer}: the view reads no dialog");
            let h = start_record("agent-view-no-hint", false, pending_record());
            pass_hold().await;
            h.chunk(hinted);
            advance(Duration::from_secs(2)).await;
            h.chunk(unhinted);
            advance(Duration::from_secs(2)).await;
            assert_eq!(
                h.status(),
                SessionStatus::AwaitingInput,
                "{footer}: the dialog is still up"
            );
            assert!(has_pending(&h), "{footer}: the prompt must stay pending");
        }
    }

    /// A main hook event starts a hold, which no screen write
    /// passes: a dialog painted around it and answered inside it was still
    /// up, so the first reading after the hold ends the status.
    #[tokio::test(start_paused = true)]
    async fn a_dialog_seen_during_the_hold_still_releases() {
        // Painted once the hold has begun.
        let h = start_record("agent-view-in-hold", false, pending_record());
        pass_hold().await;
        h.set_record(main_stamp);
        settle().await;
        h.chunk(subagent_view_with_a_question());
        advance(CHECK_DEBOUNCE + ms(10)).await;
        h.chunk(capture_frame(SUBAGENT_IN_VIEW));
        advance(Duration::from_secs(2)).await;
        assert_eq!(h.status(), SessionStatus::Idle, "painted in the hold");
        assert!(
            !has_pending(&h),
            "painted in the hold: prompt still pending"
        );
        // Painted just before the hold, whose start drops the debounced check.
        let h = start_record("agent-view-before-hold", false, pending_record());
        pass_hold().await;
        h.chunk(subagent_view_with_a_question());
        settle().await;
        h.set_record(main_stamp);
        settle().await;
        h.chunk(capture_frame(SUBAGENT_IN_VIEW));
        advance(Duration::from_secs(2)).await;
        assert_eq!(h.status(), SessionStatus::Idle, "painted before the hold");
        assert!(!has_pending(&h), "painted before the hold: still pending");
    }

    /// A hook landing between a reading and its clear refuses the clear, and
    /// the next reading in that view tries it again.
    #[tokio::test]
    async fn a_refused_clear_is_tried_again_on_the_next_reading() {
        let registry = SessionRegistry::new(scratch_dirs("agent-view-refused"));
        registry.insert(pending_record());
        let (reading, view) = read_frame(&capture_frame(SUBAGENT_IN_VIEW));
        let agent = agent_registry::default_agent();
        let chat = crate::chat_mirror::ChatMirror::new(
            Arc::clone(&registry),
            crate::hook_status::LiveLines::default(),
            Arc::new(tokio::sync::watch::channel(0usize).0),
        )
        .tap(SESSION, &agent);
        let sight = super::Sight {
            screen: Arc::new(Mutex::new(Screen::new(COLS, ROWS))),
            rules: rules_for(&agent),
            chat,
        };
        let (attention, _attention_rx) = mpsc::unbounded_channel();
        let mut watch = super::ScreenWatch::new(&registry, SESSION.to_string(), sight, attention);
        watch.subagent_dialog = view.as_ref().and_then(|view| view.agent_in_view.clone());
        assert!(watch.subagent_dialog.is_some(), "the view names its agent");

        let stale = record_status(&registry, SESSION).expect("session registered");
        registry.update(SESSION, main_stamp);
        watch.follow_subagent_dialog(stale, &reading, view.as_ref(), true);
        assert_eq!(
            status_in(&registry),
            SessionStatus::AwaitingInput,
            "a clear must be refused over a record moved since the read"
        );

        let fresh = record_status(&registry, SESSION).expect("session registered");
        watch.follow_subagent_dialog(fresh, &reading, view.as_ref(), true);
        assert_eq!(
            status_in(&registry),
            SessionStatus::Idle,
            "the next reading must try the refused clear again"
        );
    }

    /// Back in main's view the screen writes again: an idle box leaves
    /// `AwaitingInput` after its window, as it does with no subagent involved.
    #[tokio::test(start_paused = true)]
    async fn returning_to_main_follows_the_main_rules() {
        let h = start_on_a_subagents_dialog("agent-view-return").await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(2)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "main's idle box did not end the status"
        );
        assert!(!has_pending(&h), "main's idle box kept the prompt pending");
    }

    /// A held `Stop` lands on its first settled frame even with another
    /// agent's conversation in view, through the turn end's `no_evidence`
    /// path.
    #[tokio::test(start_paused = true)]
    async fn a_held_stop_lands_while_a_subagent_is_in_view() {
        let h = start_record(
            "agent-view-held-stop",
            false,
            hook_record(SessionStatus::Working),
        );
        pass_hold().await;
        h.chunk(capture_frame(SUBAGENT_IN_VIEW));
        h.hook(2, &main_stop(false));
        advance(Duration::from_secs(2)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the held Stop was dropped while a subagent was in view"
        );
        assert!(!h.holds_turn_end(), "the held turn end was not landed");
    }

    #[tokio::test(start_paused = true)]
    async fn a_reattached_idle_restores_background_from_the_replayed_screen() {
        let h = start_from("reattach-background", false, SessionStatus::Idle);
        settle().await;
        h.chunk(background_shell_frame());
        advance(FOREIGN_WRITE_HOLD.saturating_sub(ms(10))).await;
        assert!(!h.background(), "the replay was read during the hold");
        advance(ms(20)).await;
        assert!(
            !h.background(),
            "the replay set background before the window closed"
        );
        advance(ms(800)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(h.background(), "the replayed footer restored no background");
    }

    #[tokio::test(start_paused = true)]
    async fn the_screen_clears_only_the_background_it_set() {
        let h = start("screen-clears-own", false);
        pass_hold().await;
        h.chunk(background_shell_frame());
        advance(Duration::from_secs(2)).await;
        assert!(h.background(), "the footer's shell set no background");
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(2)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(
            !h.background(),
            "the footer's shell went and the screen kept its flag"
        );

        let mut record = hook_record(SessionStatus::Idle);
        record.background = true;
        let h = start_record("hook-background-survives", false, record);
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(10)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(
            h.background(),
            "a footer-less frame cleared the hooks' background"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_flag_flicker_below_the_window_does_not_publish() {
        let h = start("background-flicker", false);
        pass_hold().await;
        for _ in 0..10 {
            h.chunk(background_shell_frame());
            advance(ms(50)).await;
            assert!(!h.background(), "one footer frame published background");
            h.chunk(idle_frame(&[]));
            advance(ms(50)).await;
            assert!(!h.background(), "a flickering footer published background");
        }
        h.chunk(background_shell_frame());
        advance(Duration::from_secs(2)).await;
        assert!(h.background(), "a steady footer set no background");
    }

    #[tokio::test(start_paused = true)]
    async fn a_status_change_clears_a_screen_set_flag() {
        let h = start("background-status-change", false);
        pass_hold().await;
        h.chunk(background_shell_frame());
        advance(Duration::from_secs(2)).await;
        assert!(h.background(), "the footer's shell set no background");
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(ms(50)).await;
        assert_eq!(h.status(), SessionStatus::Working);
        assert!(!h.background(), "the promotion kept background");
        h.chunk(background_shell_frame());
        advance(Duration::from_secs(3)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(h.background(), "the window restored no background");
    }

    #[tokio::test(start_paused = true)]
    async fn a_dialog_over_the_footer_leaves_the_flag_alone() {
        let h = start("background-overlay", false);
        pass_hold().await;
        h.chunk(background_shell_frame());
        advance(Duration::from_secs(2)).await;
        assert!(h.background(), "the footer's shell set no background");
        h.chunk(transcript_overlay_frame());
        advance(Duration::from_secs(10)).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        assert!(
            h.background(),
            "an overlay over the footer cleared background"
        );

        let h = start_record(
            "background-dialog",
            false,
            hook_record(SessionStatus::AwaitingInput),
        );
        pass_hold().await;
        let mut dialog = permission_frame();
        dialog.extend_from_slice("\r\n  ⏵⏵ auto mode on · 1 shell".as_bytes());
        h.chunk(dialog);
        advance(Duration::from_secs(10)).await;
        assert_eq!(h.status(), SessionStatus::AwaitingInput);
        assert!(!h.background(), "a dialog's screen set background");
    }

    #[tokio::test(start_paused = true)]
    async fn a_screen_demotion_updates_the_sidecar() {
        let dirs = scratch_dirs("screen-sidecar");
        crate::history::test_support::write_meta_for(&dirs, SESSION);
        let h = start_in(
            dirs.clone(),
            false,
            hook_record(SessionStatus::Working),
            rules_for(&agent_registry::default_agent()),
        );
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(IDLE_WITHOUT_STOP + CONFIRM_SPACING).await;
        assert_eq!(h.status(), SessionStatus::Idle);
        let since = {
            let rec = h.registry.get(SESSION).expect("session registered");
            rec.lock().expect("record lock").status_since
        };
        let meta = crate::orphan::load_meta(&dirs, SESSION).expect("load meta");
        assert_eq!(
            meta.status,
            Some(SessionStatus::Idle),
            "the sidecar kept the old status"
        );
        assert_eq!(meta.status_since, since, "the sidecar kept the old stamp");
    }

    #[tokio::test(start_paused = true)]
    async fn a_dialog_repainted_every_20ms_is_seen_within_two_max_waits() {
        let mut h = start("dialog-burst", false);
        pass_hold().await;
        // Checks at one and two max-waits agree: 15 repaints reach 300 ms.
        for _ in 0..15 {
            h.chunk(permission_frame());
            advance(ms(20)).await;
        }
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "continuous repaints held the check past its longest wait"
        );
        assert_eq!(h.attentions(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_overflowing_broadcast_restarts_the_window() {
        let h = start_record("lagged", false, hook_record(SessionStatus::Working));
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(ms(200)).await;
        // More chunks than the broadcast holds, with no yield: the watcher lags.
        for _ in 0..70 {
            h.chunk(idle_frame(&[]));
        }
        // The lag at 200 ms holds the screen until 900 ms.
        advance(ms(690)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a write landed inside the lag's hold"
        );
        advance(FOREIGN_WRITE_HOLD + IDLE_WITHOUT_STOP + CONFIRM_SPACING).await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "the window after the lag publishes"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_restored_prompt_does_not_leave_on_its_first_spinner_chunk() {
        let h = start_record("restored-spinner", false, restored_prompt_record());
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(ms(100)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the replayed spinner left the restored prompt at once"
        );
        pass_hold().await;
        h.chunk(idle_frame(&[SPINNER_ROW]));
        advance(ms(100)).await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "the spinner left the prompt without the window"
        );
        advance(ms(600)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "the spinner leaves after the window"
        );
        assert_eq!(pending_input(&h), None, "the prompt left with the wait");
    }
}
