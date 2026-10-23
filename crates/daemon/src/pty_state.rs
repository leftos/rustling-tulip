//! Status detection for PTY-driven sessions.
//!
//! An interactive agent session's status comes from its hooks and from its
//! screen. [`watch`] feeds the session's grid and, once output settles, reads
//! it with [`crate::screen::rules::read_claude`]. What the screen shows is
//! published behind hysteresis, a hold after any other writer's change, and a
//! guard that refuses the write once the record has moved since the read. A
//! quiet screen is never read as idle on its own: a spinner still on screen
//! keeps a turn `Working` however long the PTY is silent.
//!
//! A plain shell ([`watch_plain_shell`]) runs the same screen check while
//! `claude` runs in it: while hooks drive it, or once its screen has shown
//! Claude for a whole confirmation window. Once the shell's prompt has been
//! back on the grid's last row for a whole window, the shell is handed back
//! to its heuristic classifier:
//! - an output-vs-input byte-volume comparison: command output outpaces
//!   keystroke echo and promotes to `working`
//! - a terminal-title heartbeat: a title change counts as activity (see
//!   [`crate::osc_title::TitleActivity`])
//! - an idle timeout with hysteresis: promotion to `working` happens on the
//!   first qualifying chunk, but demotion back to `idle` waits for
//!   [`SHELL_IDLE_AFTER`] of silence

use crate::osc_title::TitleActivity;
use crate::screen::SharedScreen;
use crate::screen::rules::{self, Reading, ScreenState, Verdict};
use crate::session::{SessionEvent, SessionRecord, SessionRegistry, push_recent_action};
use protocol::{AttentionReason, SessionStatus};
use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};
use tracing::{debug, warn};

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

/// How long a watcher leaves the screen unread after another writer changes
/// the status or the hook stamp, and after it starts: the grid may still show
/// the frame from before the change.
const FOREIGN_WRITE_HOLD: Duration = Duration::from_millis(700);

pub struct AttentionEvent {
    pub session_id: String,
    pub reason: AttentionReason,
}

/// What a plain shell's status watcher reads: the session's output broadcast,
/// the byte counts of the user input forwarded to it, and the grid it feeds
/// that output into and reads Claude's screen from.
pub struct ShellStreams {
    pub output: broadcast::Receiver<Vec<u8>>,
    pub input_pulses: mpsc::UnboundedReceiver<usize>,
    pub screen: SharedScreen,
}

/// What an agent session's status watcher reads: the session's output
/// broadcast, and the grid it feeds that output into and reads the status
/// from.
pub struct AgentStreams {
    pub output: broadcast::Receiver<Vec<u8>>,
    pub screen: SharedScreen,
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
/// something else (the owner's key leaves the answered dialog painted until
/// Claude redraws). Every other change (leaving
/// `AwaitingInput`, `Working` to `Idle`) publishes only after
/// [`CONFIRM_CHECKS`] agreeing checks at least [`CONFIRM_SPACING`] apart, or on
/// the first agreeing check [`CONFIRM_CAP`] after the window opened, all read
/// against the same record. A reading taken from the title alone, an overlay,
/// or a screen no rule matches never writes and breaks any open window.
///
/// When another writer (a hook event, the owner's key, an exit) changes the
/// record's status, hook stamp or hook-driven flag, the screen is left unread
/// for [`FOREIGN_WRITE_HOLD`], and so it is when the watcher starts (while a
/// reattached session's replay lands), after the output stream lagged, and
/// when a check finds the grid resized; the screen is read again once the
/// hold ends, output or not. The write itself is refused unless the
/// record still holds the status, hook stamp and hook-driven flag the window
/// read (see [`apply_screen_status`]).
///
/// `attention_tx` receives one event each time the screen moves the session
/// into `AwaitingInput`.
pub fn watch(
    registry: &Arc<SessionRegistry>,
    session_id: String,
    streams: AgentStreams,
    attention_tx: mpsc::UnboundedSender<AttentionEvent>,
) {
    let AgentStreams { mut output, screen } = streams;
    let mut watcher = ScreenWatch::new(registry, session_id, screen, attention_tx);
    tokio::spawn(async move { watcher.run_agent(&mut output).await });
}

/// What a check reads the screen for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// An agent session: the screen sets its status.
    Agent,
    /// A plain shell running `claude`: the screen sets its status, until
    /// Claude's screen is gone.
    ShellClaude,
    /// A plain shell not running `claude`: the screen is read only to spot
    /// Claude.
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
    /// A shell's screen showed Claude for a whole window.
    ClaudeShown,
    /// Claude's screen was gone from a shell for a whole window, and the
    /// shell was handed back to its classifier.
    ClaudeEnded,
}

/// What a record change seen by the wake asks of the watcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Moved {
    /// A shell's hook-driven flag flipped; the shell watcher acts on it.
    HooksFlipped,
    /// The screen is held off.
    Held,
    /// Nothing: a shell not running `claude` writes nothing from its screen.
    Ignored,
}

/// One session's screen check: its inputs, and the state of the check it has
/// scheduled and the change it is confirming. The agent watcher runs it alone;
/// the plain-shell watcher runs it beside its classifier.
struct ScreenWatch {
    registry: Arc<SessionRegistry>,
    session_id: String,
    screen: SharedScreen,
    attention_tx: mpsc::UnboundedSender<AttentionEvent>,
    wake: RecordWake,
    confirm: Confirm,
    /// When the last chunk of output arrived; a check at least
    /// [`CHECK_DEBOUNCE`] after it reads a settled frame.
    last_output: Option<Instant>,
    /// The grid's size at the last executed check.
    size: Option<(u16, u16)>,
    /// The verdict of the last executed check.
    last_verdict: Option<Verdict>,
    /// Whether a dialog on screen may raise `AwaitingInput`: false from the
    /// moment the session enters or leaves a prompt that the screen still
    /// shows, until a check reads anything other than a dialog. A dialog left
    /// painted after the owner's key cleared it raises nothing.
    dialog_edge_armed: bool,
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
    /// Claude's screen, in a shell not yet running `claude`.
    ClaudeShown,
    /// Claude's screen gone, in a shell running `claude`.
    ClaudeGone,
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

    /// The next check due: the debounced one, or the window's next
    /// confirmation, whichever comes first.
    fn next_check(&self) -> Option<Instant> {
        let confirm = self
            .window
            .as_ref()
            .map(|window| window.counted + CONFIRM_SPACING);
        match (self.check_at, confirm) {
            (Some(at), Some(confirm)) => Some(at.min(confirm)),
            (at, confirm) => at.or(confirm),
        }
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
        let due = match self.window.as_mut() {
            Some(window) if window.target == target && window.from == read => {
                if now >= window.counted + CONFIRM_SPACING {
                    window.agreeing += 1;
                    window.counted = now;
                }
                window.agreeing >= CONFIRM_CHECKS || now >= window.opened + CONFIRM_CAP
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
    /// Subscribe to the registry before the watcher's task starts, and hold
    /// the screen unread for [`FOREIGN_WRITE_HOLD`] from now.
    fn new(
        registry: &Arc<SessionRegistry>,
        session_id: String,
        screen: SharedScreen,
        attention_tx: mpsc::UnboundedSender<AttentionEvent>,
    ) -> Self {
        let wake = RecordWake::new(registry, &session_id);
        Self {
            registry: Arc::clone(registry),
            session_id,
            screen,
            attention_tx,
            wake,
            confirm: Confirm::new(Instant::now()),
            last_output: None,
            size: None,
            last_verdict: None,
            dialog_edge_armed: true,
        }
    }

    async fn run_agent(&mut self, output: &mut broadcast::Receiver<Vec<u8>>) {
        loop {
            let next_check = self.confirm.next_check();
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

    /// Read the record, then the screen, and act on what the screen shows
    /// when the rules allow it.
    fn check(&mut self, now: Instant, mode: Mode) -> Checked {
        let (read, quiet) = match self.read_record(now, mode) {
            Ok(found) => found,
            Err(checked) => return checked,
        };
        let Some((reading, prompt)) = self.read_screen() else {
            self.confirm.hold(now);
            return Checked::Done;
        };
        debug!(
            session_id = %self.session_id,
            rule = ?reading.rule,
            verdict = ?reading.verdict,
            from_title = reading.from_title,
            prompt,
            status = ?read.status,
            ?mode,
            quiet,
            "pty_state screen check"
        );
        self.last_verdict = Some(reading.verdict);
        if reading.verdict != Verdict::State(ScreenState::AwaitingInput) {
            self.dialog_edge_armed = true;
        }
        match mode {
            Mode::Shell => {
                self.watch_for_claude(read, shows_claude(reading) && !prompt, quiet, now)
            }
            Mode::ShellClaude if prompt => self.watch_for_claude_end(read, now),
            Mode::ShellClaude if !shows_claude(reading) => {
                // A Claude screen no rule knows, or Claude's title alone, with
                // the shell's prompt not back: claude may still run. Nothing
                // writes, and any open window waits for the next check.
                self.confirm.candidate = None;
                Checked::Done
            }
            Mode::Agent | Mode::ShellClaude => {
                self.follow_screen(read, reading, quiet, now);
                Checked::Done
            }
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
            return Err(Checked::Done);
        }
        let Some(read) = record_status(&self.registry, &self.session_id) else {
            return Err(Checked::Gone);
        };
        let before = self.wake.seen;
        // Another writer changed the record and its broadcast has not arrived
        // yet: the grid predates that change too.
        if self.wake.note(read) {
            match self.record_moved(before, mode, now) {
                Moved::HooksFlipped => return Err(Checked::HooksFlipped),
                Moved::Held => return Err(Checked::Done),
                Moved::Ignored => {}
            }
        }
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

    /// The screen's reading, and whether its last non-empty row is a shell's
    /// prompt; `None` when the grid's size changed since the last executed
    /// check: the frame under the new size may be half drawn.
    fn read_screen(&mut self) -> Option<(Reading, bool)> {
        let (size, reading, prompt) = {
            let screen = crate::sync::lock(&self.screen);
            (
                screen.size(),
                rules::read_claude(&screen),
                rules::last_row_is_shell_prompt(&screen),
            )
        };
        let resized = self.size.is_some_and(|last| last != size);
        self.size = Some(size);
        (!resized).then_some((reading, prompt))
    }

    /// Publish the status the screen shows, at once or after its window.
    fn follow_screen(&mut self, read: StatusRead, reading: Reading, quiet: bool, now: Instant) {
        let Some(target) = screen_target(reading).filter(|&target| target != read.status) else {
            self.confirm.window = None;
            self.confirm.candidate = None;
            return;
        };
        if publishes_at_once(read.status, target) {
            self.publish_at_once(read, target, quiet);
        } else {
            self.confirm.candidate = None;
            if self.confirm.agree(read, Target::Status(target), now) {
                self.publish(read, target);
            }
        }
    }

    /// In a shell not running `claude`, count a settled check whose screen
    /// shows Claude with no shell prompt under it (`shown`); answers
    /// [`Checked::ClaudeShown`] once a whole window has. A check inside a
    /// burst of output neither counts nor breaks the window.
    fn watch_for_claude(
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
        if self.confirm.agree(read, Target::ClaudeShown, now) {
            Checked::ClaudeShown
        } else {
            Checked::Done
        }
    }

    /// In a shell running `claude`, count a check whose last row is the
    /// shell's prompt; once a whole window has, hand the shell back to its classifier
    /// (see [`hand_shell_back`]) and answer [`Checked::ClaudeEnded`]. A write
    /// that lands is this watcher's own, so its broadcast starts no hold.
    fn watch_for_claude_end(&mut self, read: StatusRead, now: Instant) -> Checked {
        self.confirm.candidate = None;
        if !self.confirm.agree(read, Target::ClaudeGone, now) {
            return Checked::Done;
        }
        match hand_shell_back(&self.registry, &self.session_id, read) {
            Some(after) => {
                self.wake.seen = Some(after);
                Checked::ClaudeEnded
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
            self.publish(read, target);
        } else {
            self.confirm.candidate = Some((read, target));
        }
    }

    /// Write `next` over `from`; a write that lands is this watcher's own, so
    /// its broadcast starts no hold.
    fn publish(&mut self, from: StatusRead, next: SessionStatus) {
        if apply_screen_status(
            &self.registry,
            &self.session_id,
            from,
            next,
            &self.attention_tx,
        ) {
            self.wake.seen = Some(StatusRead {
                status: next,
                ..from
            });
            if next == SessionStatus::AwaitingInput {
                self.dialog_edge_armed = false;
            }
        }
    }

    /// The record moved from `before` to what the wake now holds. A shell's
    /// hook-driven flip is left to the shell watcher; a shell not running
    /// `claude` ignores any other move; otherwise the screen is held off.
    fn record_moved(&mut self, before: Option<StatusRead>, mode: Mode, now: Instant) -> Moved {
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
    /// saw: hold off the screen. A move off `AwaitingInput` while the screen
    /// showed the dialog (the owner's key) disarms the dialog edge, since the
    /// dialog stays painted until Claude redraws.
    fn foreign_write(&mut self, before: Option<StatusRead>, now: Instant) {
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

/// The status a screen reading asks for, or `None` when it asks for none: an
/// overlay, an unmatched screen, or a state read from the title alone, which
/// never writes.
fn screen_target(reading: Reading) -> Option<SessionStatus> {
    match reading.verdict {
        Verdict::State(_) if reading.from_title => None,
        Verdict::State(ScreenState::Working) => Some(SessionStatus::Working),
        Verdict::State(ScreenState::AwaitingInput) => Some(SessionStatus::AwaitingInput),
        Verdict::State(ScreenState::Idle) => Some(SessionStatus::Idle),
        Verdict::Keep | Verdict::NoMatch => None,
    }
}

/// Whether a reading comes from Claude's screen: a rule over the screen body
/// matched, a state or an overlay. A title alone, which outlives the CLI that
/// set it, does not count.
fn shows_claude(reading: Reading) -> bool {
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

/// Whether `record` moved since `from` was read (its status, hook stamp or
/// hook-driven flag), or is stopped or failed: either refuses a screen write.
fn moved_since(record: &SessionRecord, from: StatusRead) -> bool {
    record.status != from.status
        || record.last_hook_time != from.last_hook_time
        || record.hook_driven != from.hook_driven
        || matches!(record.status, SessionStatus::Stopped | SessionStatus::Error)
}

/// Write the status the screen shows, refused unless the record still holds
/// the status, hook stamp and hook-driven flag of `from` and is not stopped
/// or failed; answers whether it wrote.
///
/// A hook event, the owner's key or an exit that landed after `from` was read
/// wins. A refused write neither broadcasts nor touches the sidecar. A write
/// that lands goes through [`apply_watcher_status`], so it clears
/// `background` and, leaving `AwaitingInput`, the pending prompt; it never
/// writes `hook_driven` or the hook stamp. On entry to `AwaitingInput` the
/// one [`AttentionEvent`] is sent after the registry update returns.
fn apply_screen_status(
    registry: &Arc<SessionRegistry>,
    session_id: &str,
    from: StatusRead,
    next: SessionStatus,
    attention_tx: &mpsc::UnboundedSender<AttentionEvent>,
) -> bool {
    let mut wrote = false;
    let mut entered_awaiting = false;
    registry.update_if_changed(session_id, |rec| {
        if moved_since(rec, from) {
            return false;
        }
        entered_awaiting = apply_watcher_status(rec, next);
        wrote = rec.status == next;
        wrote
    });
    if entered_awaiting {
        announce_awaiting_input(session_id, attention_tx);
    }
    wrote
}

/// Hand a shell whose `claude` has ended back to its own classifier: clear
/// `hook_driven` and `background` (claude's background tasks ended with it),
/// and end a `Working` or `AwaitingInput` turn at `Idle` through
/// [`apply_watcher_status`] (an `Idle` stays as it is). Refused, like
/// [`apply_screen_status`], unless the record still holds what `from` read;
/// answers the record as written, or `None` when refused. It is the only
/// screen write of `hook_driven`, and it never writes the hook stamp. An agent
/// typed into its shell at spawn whose `SessionEnd` never came ends here, and
/// its end is announced once the update is broadcast.
fn hand_shell_back(
    registry: &Arc<SessionRegistry>,
    session_id: &str,
    from: StatusRead,
) -> Option<StatusRead> {
    let mut after = None;
    let mut agent_exited = false;
    registry.update_if_changed(session_id, |rec| {
        if moved_since(rec, from) {
            return false;
        }
        let in_turn = matches!(
            rec.status,
            SessionStatus::Working | SessionStatus::AwaitingInput
        );
        agent_exited = rec.end_agent_in_shell();
        let changed = rec.hook_driven || rec.background || in_turn || agent_exited;
        rec.hook_driven = false;
        rec.background = false;
        if in_turn {
            apply_watcher_status(rec, SessionStatus::Idle);
        }
        after = Some(StatusRead {
            status: rec.status,
            hook_driven: false,
            last_hook_time: rec.last_hook_time,
        });
        changed
    });
    if agent_exited {
        registry.announce_agent_exited(session_id);
    }
    after
}

/// Whether the agent typed into session `session_id`'s shell is up: its hooks
/// drive the session, or its screen shows the agent with no shell prompt
/// under it, as the shell's watcher reads it.
pub fn agent_up(registry: &SessionRegistry, session_id: &str) -> bool {
    let Some(arc) = registry.get(session_id) else {
        return false;
    };
    let (hooks, screen) = {
        let record = crate::sync::lock(&arc);
        (record.hook_driven, record.screen.clone())
    };
    hooks
        || screen.is_some_and(|screen| {
            let screen = crate::sync::lock(&screen);
            shows_claude(rules::read_claude(&screen)) && !rules::last_row_is_shell_prompt(&screen)
        })
}

/// Spawn a background task that sets a plain shell's status.
///
/// The task feeds the shell's grid from its PTY output, in every mode. While
/// `claude` runs in the shell (hooks drive it, or its screen showed Claude for
/// a whole window without them), it runs the agent watcher's screen check
/// (see [`watch`]), and `attention_tx` receives one event each time the screen
/// moves the session into `AwaitingInput`. Once the grid's last non-empty row
/// has been the shell's prompt for a whole window, `claude` has ended (its
/// last frame stays painted above the prompt): the shell is handed back to
/// its classifier (see [`hand_shell_back`]). A screen no Claude rule matches,
/// or Claude's title alone, with no prompt back, keeps `claude` running.
/// Without hooks, the shell enters that mode once settled output (read after
/// [`CHECK_DEBOUNCE`] of quiet, never inside a burst) has shown Claude's
/// screen, with no shell prompt under it, for a whole window.
///
/// A shell not running `claude` keeps its classifier: command output sets the
/// status to `Working`, while echoed input and prompt redraws leave it
/// `Idle`, and [`SHELL_IDLE_AFTER`] of silence ends a `Working`. `initial` is
/// the status the session's record already carries; the classifier only
/// moves between `Idle` and `Working`, so any other stored status seeds
/// `Idle` (see [`State::seed_plain_shell`]). Output during the watcher's
/// starting [`FOREIGN_WRITE_HOLD`] (a reattached session's replay) only feeds
/// the grid. When hooks start or stop driving the shell, the classifier's
/// state is seeded again from the record and the screen is held off for
/// [`FOREIGN_WRITE_HOLD`]; on every return to the classifier, its volume
/// windows, title and silence start afresh.
pub fn watch_plain_shell(
    registry: &Arc<SessionRegistry>,
    session_id: String,
    streams: ShellStreams,
    attention_tx: mpsc::UnboundedSender<AttentionEvent>,
    initial: SessionStatus,
) {
    let ShellStreams {
        mut output,
        input_pulses,
        screen,
    } = streams;
    let watch = ScreenWatch::new(registry, session_id, screen, attention_tx);
    let mut watcher = ShellWatcher {
        replay_until: watch.confirm.hold_until,
        watch,
        input_pulses,
        state: State::seed_plain_shell(initial),
        quiet_from: Instant::now(),
        idle_checked: false,
        in_window: VecDeque::new(),
        out_window: VecDeque::new(),
        title_activity: TitleActivity::new(),
        claude_on_screen: false,
    };
    tokio::spawn(async move { watcher.run(&mut output).await });
}

/// One plain shell's status watcher: the screen check it runs while `claude`
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
    /// Whether the screen showed Claude for a whole window while hooks were
    /// not driving the shell: `claude` started without hooks.
    claude_on_screen: bool,
}

impl ShellWatcher {
    /// Whether `claude` runs in the shell: hooks drive it, or its screen
    /// showed Claude.
    fn mode(&self) -> Mode {
        let hooks = self.watch.wake.seen.is_some_and(|read| read.hook_driven);
        if hooks || self.claude_on_screen {
            Mode::ShellClaude
        } else {
            Mode::Shell
        }
    }

    async fn run(&mut self, output: &mut broadcast::Receiver<Vec<u8>>) {
        loop {
            let mode = self.mode();
            let next_check = self.watch.confirm.next_check();
            let next_idle_check = self.quiet_from + SHELL_IDLE_AFTER;
            let idle_due = mode == Mode::Shell
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
    /// on it while `claude` is not running, and run the screen check if it is
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
        if mode == Mode::Shell {
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
            Checked::ClaudeShown => {
                // Claude mode reads the screen at once, from a fresh window.
                self.claude_on_screen = true;
                self.watch.confirm.check_at = Some(now);
            }
            Checked::ClaudeEnded => {
                self.claude_on_screen = false;
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
    /// classifier from the record, and hold off the screen.
    fn hooks_flipped(&mut self, now: Instant) {
        self.watch.confirm.hold(now);
        self.reseed();
        if self.mode() == Mode::Shell {
            self.start_shell_mode(now);
        }
    }

    /// The classifier takes the shell back: the volume, the title and the
    /// silence it saw while `claude` ran are not the shell's activity.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StatusRead {
    status: SessionStatus,
    hook_driven: bool,
    last_hook_time: Option<u128>,
}

/// The record's current status, whether hooks drive its promotions, and the
/// hook ordering stamp, as one snapshot the caller carries into its write.
fn record_status(registry: &Arc<SessionRegistry>, session_id: &str) -> Option<StatusRead> {
    let arc = registry.get(session_id)?;
    let guard = crate::sync::lock(&arc);
    Some(StatusRead {
        status: guard.status,
        hook_driven: guard.hook_driven,
        last_hook_time: guard.last_hook_time,
    })
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
/// a `Stop` writes after this) and records the `"awaiting input"` action on
/// entry. Answers whether the record entered `AwaitingInput`, so a caller
/// holding a registry update can raise the one `Attention` after that update
/// returns.
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
        if rec.hook_driven != read.hook_driven || rec.last_hook_time != read.last_hook_time {
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
        AgentStreams, CHECK_DEBOUNCE, FOREIGN_WRITE_HOLD, SHELL_IDLE_AFTER, ShellStreams, State,
        apply_screen_status, hand_shell_back, on_plain_shell_transition, record_status, watch,
        watch_plain_shell,
    };
    use crate::paths::Dirs;
    use crate::screen::{Screen, SharedScreen};
    use crate::session::{SessionRecord, SessionRegistry};
    use chrono::Utc;
    use protocol::{
        Agent, AppearanceOverrides, SessionKind, SessionMetrics, SessionMode, SessionStatus,
    };
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::{broadcast, mpsc};

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
            kind: SessionKind::Standalone,
            members: Vec::new(),
            mode: SessionMode::Interactive,
            started_at: Utc::now(),
            status: SessionStatus::Idle,
            status_since: Some(Utc::now()),
            exit_code: None,
            metrics: SessionMetrics::default(),
            recent_actions: Vec::new(),
            pty: None,
            headless: None,
            workspace_id: None,
            agent: Agent::default(),
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
            claude_session_id: None,
            pending_input: None,
            last_hook_time: None,
            hook_driven: false,
            background: false,
            last_assistant_message: None,
            busy_excluded: false,
            agent_in_shell: false,
            shell_label: None,
            agent_exit_announced: false,
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
        start_in(scratch_dirs(tag), plain_shell, record)
    }

    /// [`start_record`] over a registry in `dirs`, so a test can put a
    /// sidecar there first. The record carries the grid its watcher feeds.
    fn start_in(dirs: Dirs, plain_shell: bool, mut record: SessionRecord) -> Harness {
        let initial = record.status;
        let screen: SharedScreen = Arc::new(Mutex::new(Screen::new(COLS, ROWS)));
        record.screen = Some(Arc::clone(&screen));
        let registry = SessionRegistry::new(dirs);
        registry.insert(record);
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
                },
                attention_tx,
                initial,
            );
            None
        } else {
            watch(
                &registry,
                SESSION.to_string(),
                AgentStreams {
                    output: output_rx,
                    screen: Arc::clone(&screen),
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
        advance(ms(100)).await;
        h.set_record_unbroadcast(|record| record.last_hook_time = Some(2));
        // The window's next check, at 190 ms, reads the moved record and holds
        // until 890 ms.
        advance(ms(780)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a window published over a hook event its broadcast had not reached"
        );
        advance(FOREIGN_WRITE_HOLD + ms(60)).await;
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
        h.set_record(|record| record.last_hook_time = Some(2));
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
            record.lock().expect("record lock").last_hook_time = Some(2);
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

    #[test]
    fn hand_shell_back_on_an_agent_in_shell_record_announces_once() {
        let registry = SessionRegistry::new(scratch_dirs("hand-back-announce"));
        let mut record = hook_record(SessionStatus::Working);
        record.agent_in_shell = true;
        record.shell_label = Some("pwsh".to_owned());
        record.program_name = Some("claude".to_owned());
        registry.insert(record);
        let mut events = registry.subscribe();
        let read = record_status(&registry, SESSION).expect("the session is registered");
        assert!(hand_shell_back(&registry, SESSION, read).is_some());
        assert_eq!(agent_exits(&mut events), 1, "the agent's end is announced");
        {
            let record = registry.get(SESSION).expect("session registered");
            let guard = record.lock().expect("record lock");
            assert_eq!(guard.program_name.as_deref(), Some("pwsh"));
        }

        // The agent runs again without a `SessionStart` (no hooks) and ends.
        registry.update(SESSION, |rec| rec.hook_driven = true);
        let read = record_status(&registry, SESSION).expect("the session is registered");
        assert!(hand_shell_back(&registry, SESSION, read).is_some());
        assert_eq!(agent_exits(&mut events), 0, "one run announces once");
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
        let wrote =
            apply_screen_status(&registry, SESSION, read, SessionStatus::Working, &attention);
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

    /// A status another writer moved since the read (an exit, the owner's
    /// key) is never overwritten, and the refused write is not broadcast.
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
            let wrote =
                apply_screen_status(&registry, SESSION, read, SessionStatus::Idle, &attention);
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
        let wrote =
            apply_screen_status(&registry, SESSION, read, SessionStatus::Working, &attention);
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

    /// The owner's key clears the prompt at once; the dialog still on the
    /// grid, repainted before Claude redraws, must not raise it again.
    #[tokio::test(start_paused = true)]
    async fn an_owner_key_clear_holds_off_a_stale_dialog() {
        let mut h = start_record(
            "owner-key-hold",
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
            "a stale dialog undid the owner's clear"
        );
    }

    /// After the owner's clear, a dialog that goes away (the idle box) and
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
        advance(ms(100)).await;
        h.set_record(|record| record.last_hook_time = Some(2));
        advance(ms(690)).await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a window opened before a newer hook event still published"
        );
        advance(FOREIGN_WRITE_HOLD + ms(60)).await;
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
        h.set_record(|record| record.last_hook_time = Some(2));
        advance(ms(690)).await;
        assert_eq!(h.status(), SessionStatus::Working, "read inside the hold");
        advance(FOREIGN_WRITE_HOLD + ms(60)).await;
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
        advance(FOREIGN_WRITE_HOLD + ms(60)).await;
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
        advance(ms(200)).await;
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

    #[tokio::test(start_paused = true)]
    async fn a_screen_demotion_updates_the_sidecar() {
        let dirs = scratch_dirs("screen-sidecar");
        crate::history::test_support::write_meta_for(&dirs, SESSION);
        let h = start_in(dirs.clone(), false, hook_record(SessionStatus::Working));
        pass_hold().await;
        h.chunk(idle_frame(&[]));
        advance(ms(700)).await;
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
        advance(FOREIGN_WRITE_HOLD + ms(60)).await;
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
