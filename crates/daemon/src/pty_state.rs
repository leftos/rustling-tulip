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
//! A plain shell ([`watch_plain_shell`]) keeps a heuristic classifier:
//! - an output-vs-input byte-volume comparison: command output outpaces
//!   keystroke echo and promotes to `working`
//! - a terminal-title heartbeat: a title change counts as activity (see
//!   [`crate::osc_title::TitleActivity`])
//! - an idle timeout with hysteresis: promotion to `working` happens on the
//!   first qualifying chunk, but demotion back to `idle` waits for
//!   [`IDLE_AFTER`] of silence
//! - while hooks drive it, a prompt regex over its byte tail decides whether
//!   a hook-reported `AwaitingInput` is still on screen

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

/// Silence required before a plain shell's `Working` / `AwaitingInput`
/// settles back to `Idle`. Deliberately asymmetric with promotion (which is
/// immediate): a TUI element repainting once a second (an elapsed-time
/// counter) must hold `working` instead of flapping. 2.5 s clears a 1 Hz
/// cadence with room for scheduler jitter while still reading as "idle"
/// promptly once the command really stops.
const IDLE_AFTER: Duration = Duration::from_millis(2500);
const SCROLLBACK_BYTES: usize = 8 * 1024;

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

/// Quiet time after a chunk of output before the agent watcher reads the
/// screen, so a repaint split across chunks is read whole.
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

/// How long the agent watcher leaves the screen unread after another writer
/// changes the status or the hook stamp, and after it starts: the grid may
/// still show the frame from before the change.
const FOREIGN_WRITE_HOLD: Duration = Duration::from_millis(700);

pub struct AttentionEvent {
    pub session_id: String,
    pub reason: AttentionReason,
}

/// The PTY streams a plain shell's status watcher reads: the session's output
/// broadcast and the byte counts of the user input forwarded to it.
pub struct WatchStreams {
    pub output: broadcast::Receiver<Vec<u8>>,
    pub input_pulses: mpsc::UnboundedReceiver<usize>,
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
/// record's status or hook stamp, the screen is left unread for
/// [`FOREIGN_WRITE_HOLD`], and so it is when the watcher starts (while a
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
    let wake = RecordWake::new(registry, &session_id);
    let mut watcher = AgentWatcher {
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
    };
    tokio::spawn(async move { watcher.run(&mut output).await });
}

/// One agent session's status watcher: its inputs, and the state of the
/// check it has scheduled and the change it is confirming.
struct AgentWatcher {
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

/// A gated change being confirmed: the status it would publish, the record
/// it was read against, and its agreeing checks so far.
struct Window {
    target: SessionStatus,
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
    fn agree(&mut self, read: StatusRead, target: SessionStatus, now: Instant) -> bool {
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

impl AgentWatcher {
    async fn run(&mut self, output: &mut broadcast::Receiver<Vec<u8>>) {
        loop {
            let next_check = self.confirm.next_check();
            // In arm order: output is fed before a due check reads the grid,
            // and a check already due runs inline, since this biased select
            // would otherwise starve the timer arm under continuous output.
            tokio::select! {
                biased;
                msg = output.recv() => match msg {
                    Ok(bytes) => {
                        crate::sync::lock(&self.screen).feed(&bytes);
                        let now = Instant::now();
                        self.last_output = Some(now);
                        if self.confirm.schedule(now) && self.check(now).is_break() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(session_id = %self.session_id, lagged = n, "pty state lagged");
                        crate::sync::lock(&self.screen).reset_parser();
                        self.confirm.hold(Instant::now());
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                event = self.wake.events.recv(), if self.wake.open => {
                    let before = self.wake.seen;
                    if self.wake.changed(&self.registry, &self.session_id, event) {
                        self.foreign_write(before, Instant::now());
                    }
                },
                () = sleep_until(next_check.unwrap_or_else(Instant::now)), if next_check.is_some() => {
                    if self.check(Instant::now()).is_break() {
                        return;
                    }
                }
            }
        }
    }

    /// Read the record, then the screen, and publish what the screen shows
    /// when the rules allow it. Breaks once the record is gone.
    fn check(&mut self, now: Instant) -> ControlFlow<()> {
        self.confirm.check_at = None;
        // Output since the last chunk's debounce: the frame may be half drawn,
        // and the burst goes on, so the next max-wait counts from here.
        let settled_at = self.last_output.map(|at| at + CHECK_DEBOUNCE);
        let quiet = settled_at.is_none_or(|at| now >= at);
        self.confirm.burst_from = (!quiet).then_some(now);
        if now < self.confirm.hold_until {
            self.confirm.check_at = Some(self.confirm.hold_until);
            return ControlFlow::Continue(());
        }
        let Some(read) = record_status(&self.registry, &self.session_id) else {
            return ControlFlow::Break(());
        };
        let before = self.wake.seen;
        if self.wake.note(read) {
            // Another writer changed the record and its broadcast has not
            // arrived yet: the grid predates that change too.
            self.foreign_write(before, now);
            return ControlFlow::Continue(());
        }
        if matches!(read.status, SessionStatus::Stopped | SessionStatus::Error) {
            self.confirm.window = None;
            self.confirm.candidate = None;
            return ControlFlow::Continue(());
        }
        if !quiet {
            // Read the frame again once it settles, should no output follow.
            self.confirm.check_at = settled_at;
        }
        let Some(reading) = self.read_screen() else {
            self.confirm.hold(now);
            return ControlFlow::Continue(());
        };
        debug!(
            session_id = %self.session_id,
            rule = ?reading.rule,
            verdict = ?reading.verdict,
            from_title = reading.from_title,
            status = ?read.status,
            quiet,
            "pty_state screen check"
        );
        self.last_verdict = Some(reading.verdict);
        if reading.verdict != Verdict::State(ScreenState::AwaitingInput) {
            self.dialog_edge_armed = true;
        }
        let Some(target) = screen_target(reading).filter(|&target| target != read.status) else {
            self.confirm.window = None;
            self.confirm.candidate = None;
            return ControlFlow::Continue(());
        };
        if publishes_at_once(read.status, target) {
            self.publish_at_once(read, target, quiet);
        } else {
            self.confirm.candidate = None;
            if self.confirm.agree(read, target, now) {
                self.publish(read, target);
            }
        }
        ControlFlow::Continue(())
    }

    /// The screen's reading, or `None` when the grid's size changed since the
    /// last executed check: the frame under the new size may be half drawn.
    fn read_screen(&mut self) -> Option<Reading> {
        let (size, reading) = {
            let screen = crate::sync::lock(&self.screen);
            (screen.size(), rules::read_claude(&screen))
        };
        let resized = self.size.is_some_and(|last| last != size);
        self.size = Some(size);
        (!resized).then_some(reading)
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
            self.wake.seen = Some((next, from.last_hook_time));
            if next == SessionStatus::AwaitingInput {
                self.dialog_edge_armed = false;
            }
        }
    }

    /// Another writer moved the record from `before` to what the wake last
    /// saw: hold off the screen. A move off `AwaitingInput` while the screen
    /// showed the dialog (the owner's key) disarms the dialog edge, since the
    /// dialog stays painted until Claude redraws.
    fn foreign_write(&mut self, before: Option<(SessionStatus, Option<u128>)>, now: Instant) {
        let was_awaiting = before.is_some_and(|(status, _)| status == SessionStatus::AwaitingInput);
        let still_awaiting = self
            .wake
            .seen
            .is_some_and(|(status, _)| status == SessionStatus::AwaitingInput);
        let dialog_shown = self.last_verdict == Some(Verdict::State(ScreenState::AwaitingInput));
        if was_awaiting && !still_awaiting && dialog_shown {
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
        let moved = rec.status != from.status
            || rec.last_hook_time != from.last_hook_time
            || rec.hook_driven != from.hook_driven;
        if moved || matches!(rec.status, SessionStatus::Stopped | SessionStatus::Error) {
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

/// Spawn a background task that infers `idle / working` for a plain shell.
///
/// Plain shells deliberately avoid the Claude prompt classifier, but they can
/// still expose useful activity: command output should set the status to
/// `Working`, while echoed input and prompt redraws should leave it `Idle`.
/// `initial` is the status the session's record already carries. The shell
/// classifier only moves between `Idle` and `Working`, so any other stored
/// status seeds `Idle` (see [`State::seed_plain_shell`]). Hooks injected into
/// an agent the shell runs can report `AwaitingInput`; the idle timer demotes
/// it once its prompt has left the screen (see [`on_plain_shell_idle`]).
pub fn watch_plain_shell(
    registry: &Arc<SessionRegistry>,
    session_id: String,
    streams: WatchStreams,
    initial: SessionStatus,
) {
    let registry = Arc::clone(registry);
    let WatchStreams {
        mut output,
        mut input_pulses,
    } = streams;
    let mut wake = RecordWake::new(&registry, &session_id);
    tokio::spawn(async move {
        let mut state = State::seed_plain_shell(initial);
        let mut last_output = Instant::now();
        // The idle timer fires once per stretch of silence; the next output
        // or hook-reported status starts a new stretch.
        let mut idle_checked = false;
        // Read only to tell whether a hook-reported prompt is still on screen.
        let mut scrollback: Vec<u8> = Vec::with_capacity(SCROLLBACK_BYTES);
        let mut in_window: VecDeque<(Instant, u32)> = VecDeque::new();
        let mut out_window: VecDeque<(Instant, u32)> = VecDeque::new();
        let mut title_activity = TitleActivity::new();

        loop {
            let next_idle_check = last_output + IDLE_AFTER;
            // In arm order: a pending hook status re-arms the deadline before
            // a stale deadline can fire.
            tokio::select! {
                biased;
                msg = output.recv() => match msg {
                    Ok(bytes) => {
                        let now = Instant::now();
                        last_output = now;
                        idle_checked = false;
                        // Drain any input pulses that landed in our channel
                        // before (or alongside) this output chunk so the echo
                        // budget is computed against the right input volume:
                        // a fast typist's keystroke must not race ahead of its
                        // own echo into the Working state.
                        while let Ok(n) = input_pulses.try_recv() {
                            push_window(&mut in_window, now, n);
                        }
                        append_scrollback(&mut scrollback, &bytes);
                        push_window(&mut out_window, now, bytes.len());
                        trim_window(&mut in_window, now);
                        trim_window(&mut out_window, now);

                        let in_sum = window_sum(&in_window);
                        let out_sum = window_sum(&out_window);
                        // A title change (e.g. a `cd` that repaints the cwd in
                        // the prompt) counts as activity alongside command output.
                        let title_changed = title_activity.feed(&bytes);
                        match record_status(&registry, &session_id) {
                            Some(read) if read.hook_driven => {
                                // Hooks own the status while they are live: a
                                // shell classifier promotion would overwrite a
                                // hook-reported `AwaitingInput` with `Working`.
                                // Keep the local state in step with the record
                                // so the idle tick reads the same status.
                                state = State::seed_plain_shell(read.status);
                            }
                            Some(read) => {
                                let next =
                                    if shell_output_is_working(in_sum, out_sum) || title_changed {
                                        State::Working
                                    } else {
                                        state
                                    };
                                on_plain_shell_transition(
                                    &registry,
                                    &session_id,
                                    &mut state,
                                    next,
                                    read,
                                );
                            }
                            // The record is gone; there is nothing to write.
                            None => {}
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(session_id = %session_id, lagged = n, "plain shell state lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                pulse = input_pulses.recv() => {
                    if let Some(n) = pulse {
                        let now = Instant::now();
                        push_window(&mut in_window, now, n);
                        trim_window(&mut in_window, now);
                    }
                },
                event = wake.events.recv(), if wake.open => {
                    if wake.changed(&registry, &session_id, event) {
                        last_output = Instant::now();
                        idle_checked = false;
                    }
                },
                () = sleep_until(next_idle_check),
                    if !idle_checked && plain_shell_idle_due(&registry, &session_id, state) => {
                    // A hook status on the record whose broadcast is still on
                    // its way starts a new stretch instead of meeting the old
                    // deadline.
                    if wake.reread(&registry, &session_id) {
                        last_output = Instant::now();
                    } else {
                        idle_checked = true;
                        on_plain_shell_idle(&registry, &session_id, &scrollback, &mut state);
                    }
                }
            }
        }
    });
}

/// Run a plain shell's idle transition once a silence has lasted
/// [`IDLE_AFTER`].
///
/// While hooks drive the shell the record's status is the truth: a
/// hook-reported `Working` ends (an interrupted turn, or an agent killed
/// without a closing hook), and so does an `AwaitingInput` whose prompt has
/// left the screen. Either demotion also hands the shell back to its own
/// classifier by clearing `hook_driven`; the next mapped hook event sets it
/// again. The demotion re-checks the status and hook stamp under the lock, so
/// a promotion that landed since the read wins.
fn on_plain_shell_idle(
    registry: &Arc<SessionRegistry>,
    session_id: &str,
    scrollback: &[u8],
    state: &mut State,
) {
    match record_status(registry, session_id) {
        Some(read) if read.hook_driven => {
            *state = State::seed_plain_shell(read.status);
            let demote = match read.status {
                SessionStatus::Working => true,
                SessionStatus::AwaitingInput => !prompt_on_screen(scrollback),
                _ => false,
            };
            if demote {
                *state = State::Idle;
                demote_plain_shell_after_silence(
                    registry,
                    session_id,
                    read.status,
                    read.last_hook_time,
                );
            }
        }
        Some(read) => {
            // Hooks that have ended leave the record `Working`; adopt it so
            // the demotion writes even when no PTY chunk synced the local
            // state since the hook event.
            *state = State::seed_plain_shell(read.status);
            on_plain_shell_transition(registry, session_id, state, State::Idle, read);
        }
        None => {}
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
    /// The plain-shell watcher's starting point for a record already carrying
    /// `status`. The shell classifier only moves between `Idle` and `Working`,
    /// so every other stored status starts at `Idle`; a hook-reported
    /// `AwaitingInput` stays on the record, which the idle tick reads.
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

fn append_scrollback(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(bytes);
    if buf.len() > SCROLLBACK_BYTES {
        let drop = buf.len() - SCROLLBACK_BYTES;
        buf.drain(0..drop);
    }
}

/// Whether a prompt is on screen: the prompt regexes match the last ~2 KB of
/// stripped output. ANSI escape sequences are stripped first, since Claude's
/// TUI emits a lot of color codes that would otherwise break naive substring
/// matches; multi-byte characters are kept whole. The byte offset is widened
/// to a character boundary so a glyph straddling it cannot panic the slice.
fn prompt_on_screen(scrollback: &[u8]) -> bool {
    let recent = strip_ansi(scrollback);
    let mut tail_start = recent.len().saturating_sub(2048);
    while !recent.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    matches_prompt(&recent[tail_start..])
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
/// another writer changes the record's status or hook stamp without any PTY
/// byte, and the watcher has to react to that change (a plain shell restarts
/// its idle deadline; an agent session holds off reading its screen).
struct RecordWake {
    events: broadcast::Receiver<SessionEvent>,
    /// False once the registry's event stream has closed.
    open: bool,
    /// The status and hook stamp last seen; `None` once the record is gone.
    seen: Option<(SessionStatus, Option<u128>)>,
}

impl RecordWake {
    /// Subscribe before the watcher task starts, so an event that lands
    /// before its first poll is not missed.
    fn new(registry: &Arc<SessionRegistry>, session_id: &str) -> Self {
        Self {
            events: registry.subscribe(),
            open: true,
            seen: Self::mark(registry, session_id),
        }
    }

    fn mark(
        registry: &Arc<SessionRegistry>,
        session_id: &str,
    ) -> Option<(SessionStatus, Option<u128>)> {
        record_status(registry, session_id).map(|read| (read.status, read.last_hook_time))
    }

    /// Take one registry event; answers whether this session's status or
    /// hook stamp changed since the last one seen. A lagged stream re-reads
    /// the record, since the missed events may have changed it.
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

    /// Re-read the record's status and hook stamp; answers whether they moved
    /// since the last ones seen. The idle tick calls it too, since a registry
    /// update writes the record before it broadcasts the change.
    fn reread(&mut self, registry: &Arc<SessionRegistry>, session_id: &str) -> bool {
        let mark = Self::mark(registry, session_id);
        let changed = mark != self.seen;
        self.seen = mark;
        changed
    }

    /// [`Self::reread`] over a read the caller has already taken.
    fn note(&mut self, read: StatusRead) -> bool {
        let mark = Some((read.status, read.last_hook_time));
        let changed = mark != self.seen;
        self.seen = mark;
        changed
    }
}

/// Whether a plain shell's idle tick is due: the classifier's own `Working`,
/// a record still reporting `Working` when the PTY goes quiet (a hook-driven
/// screen, or one whose hooks have since ended), or a hook-reported
/// `AwaitingInput`, which the tick demotes once its prompt has left the screen.
fn plain_shell_idle_due(registry: &Arc<SessionRegistry>, session_id: &str, state: State) -> bool {
    match record_status(registry, session_id) {
        Some(read) if read.hook_driven => matches!(
            read.status,
            SessionStatus::Working | SessionStatus::AwaitingInput
        ),
        Some(read) => state == State::Working || read.status == SessionStatus::Working,
        None => state == State::Working,
    }
}

fn matches_prompt(tail: &str) -> bool {
    // Permission-prompt heuristics; any one of them is a prompt.
    //
    // 1. The `❯` selection cursor on an option line: the chevron, then on the
    //    same line an option number such as `1.` and a space or tab. Numbered
    //    text elsewhere (a numbered answer, the banner's `v2.1.307`) beside the
    //    idle input box's lone `❯` is not a prompt.
    // 2. "Do you want to" wording (Claude's edit/bash permission prompts).
    // 3. "Choose an option", "Press 1" or "(y/n)" wording.
    // 4. AskUserQuestion blocks contain "│" framing plus the tool name.
    let has_do_you_want = tail.contains("Do you want to");
    let has_choose_option =
        tail.contains("Choose an option") || tail.contains("Press 1") || tail.contains("(y/n)");
    let has_ask_user = tail.contains("│") && tail.contains("AskUserQuestion");

    has_do_you_want || has_choose_option || has_ask_user || has_option_chevron(tail)
}

/// Whether some `❯` is followed on its own line by an option number: spaces or
/// tabs, then one or two digits starting 1-9, a dot, and a space or tab.
fn has_option_chevron(tail: &str) -> bool {
    tail.split('❯')
        .skip(1)
        .any(|after| starts_with_option_number(after.trim_start_matches([' ', '\t'])))
}

/// Whether `text` opens with an option number such as `1. ` or `12.\t`.
fn starts_with_option_number(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digits = bytes
        .iter()
        .take(2)
        .take_while(|b| b.is_ascii_digit())
        .count();
    let first_nonzero = bytes.first().is_some_and(|b| (b'1'..=b'9').contains(b));
    first_nonzero
        && bytes.get(digits) == Some(&b'.')
        && matches!(bytes.get(digits + 1), Some(b' ' | b'\t'))
}

fn strip_ansi(input: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == 0x1B {
            // ESC
            if i + 1 < input.len() && input[i + 1] == b'[' {
                // CSI: ESC [ ... <terminator in 0x40..=0x7E>
                i += 2;
                while i < input.len() && !(0x40..=0x7E).contains(&input[i]) {
                    i += 1;
                }
                i += 1;
                continue;
            }
            if i + 1 < input.len() && input[i + 1] == b']' {
                // OSC: ESC ] ... BEL or ESC \
                i += 2;
                while i < input.len() && input[i] != 0x07 {
                    if input[i] == 0x1B && i + 1 < input.len() && input[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                if i < input.len() && input[i] == 0x07 {
                    i += 1;
                }
                continue;
            }
            i += 1;
            continue;
        }
        if b == b'\r' {
            i += 1;
            continue;
        }
        out.push(b);
        i += 1;
    }
    // Keep multi-byte characters whole and decode once: the prompt markers
    // `❯` and `│` are UTF-8 beyond ASCII, and dropping those bytes would hide
    // a real question prompt from `matches_prompt`.
    String::from_utf8_lossy(&out).into_owned()
}

/// Demote a plain shell from `expected` to `Idle` after a silence, clearing
/// the prompt it was waiting on, and clear `hook_driven` so the shell's own
/// classifier runs again.
///
/// The update checks `expected` and the hook ordering stamp again under the
/// record lock: a promotion that landed since the caller read the record wins,
/// and a status that left and returned through a newer hook event (an ABA) is
/// not demoted at the old deadline.
fn demote_plain_shell_after_silence(
    registry: &Arc<SessionRegistry>,
    session_id: &str,
    expected: SessionStatus,
    epoch: Option<u128>,
) {
    registry.update(session_id, |rec| {
        if demote_if_unchanged(rec, expected, epoch) {
            rec.hook_driven = false;
        }
    });
}

/// Demote `rec` from `expected` to `Idle` when neither its status nor its hook
/// stamp has moved, clearing the prompt it was waiting on; answers whether it
/// did.
fn demote_if_unchanged(
    rec: &mut SessionRecord,
    expected: SessionStatus,
    epoch: Option<u128>,
) -> bool {
    if rec.status != expected || rec.last_hook_time != epoch {
        return false;
    }
    apply_status(rec, SessionStatus::Idle);
    if expected == SessionStatus::AwaitingInput {
        rec.pending_input = None;
    }
    true
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
    use super::{matches_prompt, prompt_on_screen, shell_output_is_working};

    #[test]
    fn prompt_tail_starts_on_a_char_boundary() {
        // The 2 KB tail begins inside the three-byte `❯`. Slicing at the raw
        // byte offset would panic, and a panicking watcher task stops
        // reporting status for the rest of the session.
        let mut screen = vec![b'a'; 2047];
        screen.extend_from_slice("❯".as_bytes());
        screen.resize(4096, b'a');
        assert_eq!(screen.len(), 4096);
        assert!(!prompt_on_screen(&screen));
    }

    #[test]
    fn plain_numbered_output_does_not_match() {
        let output = "Build output:\n1. compiling crate\n2. linking crate\n";
        assert!(!matches_prompt(output));
    }

    /// Claude Code's idle input box: a lone `❯` between two solid rules, then
    /// the user's status lines.
    fn idle_box() -> String {
        let rule = "─".repeat(135);
        [
            rule.as_str(),
            "❯",
            rule.as_str(),
            concat!(
                "  ctx:   0%  |  sub: 5h  38% (3h40m) · 7d  86% / F   0% (5d16h)",
                "  |  ds: $  17.40  |  X: 252G free  |  Opus 5.5  |  master*  |  C:\\Us…"
            ),
            "  ⏵⏵ auto mode on (shift+tab to cycle) · ← 2 agents",
        ]
        .join("\n")
    }

    #[test]
    fn idle_box_after_a_numbered_answer_is_not_a_prompt() {
        let answer = [
            "● 1. Botanically, bananas count as berries but strawberries don't.",
            "  2. A strawberry carries about 200 seeds on its outside.",
            "  3. Apples float because about a quarter of their volume is air.",
            "✻ Cooked for 26s · done 5:36 PM",
            "❯ C:/Program Files/Git/compact",
            "❯ /compact",
            "  ⎿  Compacted (ctrl+o to see full summary)",
        ]
        .join("\n");
        let tail = format!("{answer}\n{}", idle_box());
        assert!(!matches_prompt(&tail));
    }

    #[test]
    fn banner_version_and_idle_box_are_not_a_prompt() {
        let tail = format!(" ▐▛███▛█   Claude Code v2.1.307\n{}", idle_box());
        assert!(!matches_prompt(&tail));
    }

    #[test]
    fn chevron_on_an_option_line_is_a_prompt() {
        let prompts = [
            "❯ 1. Yes  2. No ",
            "❯ 1. Tea 2. Coffee",
            "Which function should be added to calc.py next?\n❯ 1. sub\n  2. div\n  3. pow",
            "Ready to submit your answers?\n❯ 1. Submit answers\n  2. Cancel",
            concat!(
                "Claude has written up a plan and is ready to execute. Would you like to proceed?\n",
                "❯ 1. Yes, and use auto mode\n  2. Yes, manually approve edits\n",
                "  3. Tell Claude what to change"
            ),
            "Do you want to proceed? 1. Yes 2. No ",
        ];
        for prompt in prompts {
            assert!(matches_prompt(prompt), "not read as a prompt: {prompt:?}");
        }
    }

    #[test]
    fn chevron_then_newline_number_is_not_a_prompt() {
        assert!(!matches_prompt("❯\n1. a\n2. b"));
    }

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
        AgentStreams, CHECK_DEBOUNCE, FOREIGN_WRITE_HOLD, IDLE_AFTER, State, WatchStreams,
        apply_screen_status, on_plain_shell_transition, record_status, watch, watch_plain_shell,
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
        }
    }

    struct Harness {
        registry: Arc<SessionRegistry>,
        output: broadcast::Sender<Vec<u8>>,
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
    /// sidecar there first. An agent session's record carries the grid its
    /// watcher feeds.
    fn start_in(dirs: Dirs, plain_shell: bool, mut record: SessionRecord) -> Harness {
        let initial = record.status;
        let screen: SharedScreen = Arc::new(Mutex::new(Screen::new(COLS, ROWS)));
        if !plain_shell {
            record.screen = Some(Arc::clone(&screen));
        }
        let registry = SessionRegistry::new(dirs);
        registry.insert(record);
        let (output, output_rx) = broadcast::channel(64);
        let (input, input_rx) = mpsc::unbounded_channel();
        let (attention_tx, attention_rx) = mpsc::unbounded_channel();
        let unread_input = if plain_shell {
            watch_plain_shell(
                &registry,
                SESSION.to_string(),
                WatchStreams {
                    output: output_rx,
                    input_pulses: input_rx,
                },
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
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(h.status(), SessionStatus::Idle, "silence past IDLE_AFTER");
    }

    #[tokio::test(start_paused = true)]
    async fn once_a_second_repaint_does_not_flicker_plain_shell() {
        let h = start("shell", true);
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
        let h = start("shell-hook-idle", true);
        h.set_record(|record| record.hook_driven = true);
        for tick in 0..3 {
            h.heartbeat(tick);
            settle().await;
            assert_eq!(
                h.status(),
                SessionStatus::Idle,
                "tick {tick}: shell output promoted a hook-reported idle"
            );
            tokio::time::sleep(HEARTBEAT / 4).await;
        }
    }

    /// Hook-reported work in a plain shell with no further PTY output is an
    /// interrupted turn: the silence demotion still ends it.
    #[tokio::test(start_paused = true)]
    async fn hook_plain_shell_working_demotes_to_idle_after_silence() {
        let h = start("shell-hook-working", true);
        h.set_record(|record| {
            record.hook_driven = true;
            record.status = SessionStatus::Working;
        });
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "silence demotes hook-reported work"
        );
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

    /// A hook-reported `Working` that lands while the watcher is parked must
    /// arm the idle deadline itself: no PTY byte follows an interrupted turn.
    async fn assert_hook_working_set_after_start_demotes(h: &Harness) {
        settle().await;
        set_hook_working(h, 1);
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "silence after a hook-reported Working demotes it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn hook_working_set_after_start_demotes_after_silence_plain_shell() {
        let h = start("shell-hook-working-late", true);
        assert_hook_working_set_after_start_demotes(&h).await;
    }

    /// The idle deadline counts from the hook-reported `Working`, not from
    /// the last PTY output long before it: a wake right after the hook event
    /// must not demote the fresh turn at once.
    async fn assert_hook_working_after_long_idle_waits(h: &Harness) {
        settle().await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        settle().await;
        assert_eq!(h.status(), SessionStatus::Idle, "silent from the start");
        set_hook_working(h, 1);
        h.input.send(1).expect("watcher still subscribed");
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Working,
            "a stale deadline demoted the fresh hook-reported turn"
        );
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "silence past IDLE_AFTER from the hook event demotes it"
        );
    }

    /// A hook-reported prompt already on the record but not yet broadcast
    /// must not meet a stale deadline: the watcher woken by anything else
    /// restarts the deadline from the change it reads instead of demoting.
    async fn assert_unbroadcast_hook_status_waits(h: &Harness) {
        settle().await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        settle().await;
        h.set_record_unbroadcast(|record| {
            record.hook_driven = true;
            record.status = SessionStatus::AwaitingInput;
            record.last_hook_time = Some(1);
        });
        h.input.send(1).expect("watcher still subscribed");
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a stale deadline demoted a hook prompt its broadcast had not reached"
        );
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "silence past IDLE_AFTER with no prompt on screen demotes it"
        );
    }

    /// A hook event already on the record but not yet broadcast, landing
    /// inside a window, refuses that window's publish: the next check reads
    /// the moved record and holds off the screen as for a broadcast write.
    /// The idle box still on screen after the hold opens a fresh window.
    #[tokio::test(start_paused = true)]
    async fn an_unbroadcast_hook_write_during_a_window_refuses_the_publish() {
        let h = start_record(
            "hook-unbroadcast",
            false,
            hook_record(SessionStatus::Working),
        );
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
    async fn a_hook_status_written_before_its_broadcast_is_not_demoted_at_once_plain_shell() {
        let h = start("shell-hook-unbroadcast", true);
        assert_unbroadcast_hook_status_waits(&h).await;
    }

    /// A hook-reported turn after a long idle, while the grid still shows the
    /// idle box: that stale frame does not demote before the window, and the
    /// spinner the turn paints inside the window cancels it.
    #[tokio::test(start_paused = true)]
    async fn a_stale_idle_box_waits_for_the_window_and_a_spinner_cancels_it() {
        let h = start("hook-working-long-idle", false);
        h.chunk(idle_frame(&[]));
        advance(Duration::from_secs(10)).await;
        assert_eq!(h.status(), SessionStatus::Idle, "idle from the start");
        set_hook_working(&h, 1);
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
    async fn hook_working_after_a_long_idle_is_not_demoted_at_once_plain_shell() {
        let h = start("shell-hook-working-long-idle", true);
        assert_hook_working_after_long_idle_waits(&h).await;
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
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "silence demotes a Working left by ended hooks"
        );
    }

    /// The shell classifier never overwrites a hook-reported prompt, and the
    /// silence demotion holds while the prompt is still on screen.
    #[tokio::test(start_paused = true)]
    async fn hook_plain_shell_awaiting_input_holds_while_the_prompt_shows() {
        let h = start("shell-hook-awaiting", true);
        h.set_record(|record| {
            record.hook_driven = true;
            record.status = SessionStatus::AwaitingInput;
        });
        h.output
            .send("\x1b[2K\r❯ 1. Yes  2. No ".as_bytes().to_vec())
            .expect("watcher still subscribed");
        for tick in 0..3 {
            h.heartbeat(tick);
            settle().await;
            assert_eq!(
                h.status(),
                SessionStatus::AwaitingInput,
                "tick {tick}: shell output overwrote a hook-reported prompt"
            );
            tokio::time::sleep(HEARTBEAT / 4).await;
        }
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a prompt on screen holds the demotion"
        );
    }

    /// A hook-reported prompt in a plain shell whose text has left the screen
    /// is over (the agent may have died without a closing hook): silence
    /// demotes it and hands the shell back to its own classifier.
    #[tokio::test(start_paused = true)]
    async fn hook_plain_shell_awaiting_input_demotes_once_the_prompt_is_gone() {
        let h = start("shell-hook-prompt-gone", true);
        h.set_record(|record| {
            record.hook_driven = true;
            record.status = SessionStatus::AwaitingInput;
            record.last_hook_time = Some(1);
        });
        h.output
            .send(b"\x1b[2K\r$ ".to_vec())
            .expect("watcher still subscribed");
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::AwaitingInput,
            "a small redraw leaves it waiting"
        );
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(
            h.status(),
            SessionStatus::Idle,
            "silence with no prompt demotes it"
        );
        assert!(!h.hook_driven(), "the demotion leaves hooks in charge");
    }

    /// Silence that ends a hook-reported turn in a plain shell also ends the
    /// hooks' hold on it, so a killed agent does not mute the shell for good.
    #[tokio::test(start_paused = true)]
    async fn plain_shell_silence_demotion_clears_hook_driven() {
        let h = start("shell-hook-release", true);
        settle().await;
        set_hook_working(&h, 1);
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(h.status(), SessionStatus::Idle, "silence demotes it");
        assert!(!h.hook_driven(), "the demotion leaves hooks in charge");
    }

    /// Once the silence demotion has released a plain shell from its hooks,
    /// the shell's own output promotes it again.
    #[tokio::test(start_paused = true)]
    async fn plain_shell_classifier_runs_again_after_hook_driven_clears() {
        let h = start("shell-hook-release-promote", true);
        settle().await;
        set_hook_working(&h, 1);
        tokio::time::sleep(IDLE_AFTER + Duration::from_millis(50)).await;
        settle().await;
        assert_eq!(h.status(), SessionStatus::Idle, "silence demotes it");
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
        assert_eq!(h.status(), SessionStatus::Idle);
        h.heartbeat(0);
        settle().await;
        assert_eq!(h.status(), SessionStatus::Working);
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
