//! Scripted PTY input runner — drives a [`protocol::PromptInjector`] against
//! a live [`PtyHandle`] after spawn.
//!
//! Used by the preset launcher to enter the agent's plan mode and submit a
//! prompt without relying on the `-p` CLI flag (which auto-executes). The
//! runner has two pieces of intelligence layered onto a mostly-dumb
//! sleep/write loop:
//!
//! 1. The **first** step, when it's a `Delay`, is treated as a startup-wait
//!    with PTY-output-quiescence-based early exit. We proceed as soon as
//!    the agent's TUI has finished its initial paint rather than always
//!    waiting the full ceiling, but only after we've seen enough output to
//!    be confident the TUI has actually started painting. How much output
//!    that takes and how long the stream must then stay quiet comes from
//!    the agent's plugin ([`agent_api::ReadySignal`]): by default a banner of
//!    1024 bytes, then 1.5 s of silence. An injector whose `startup` is
//!    [`InjectorStartup::ShellPrompt`] waits for a shell prompt instead:
//!    a short quiet period once the shell has printed anything, capped at
//!    the step's own delay (see [`StartupRule`]).
//! 2. If the injector declares a `verify_mode_marker`, the runner scans
//!    PTY output for that marker after the pre-input steps and re-sends
//!    them up to a few times if the marker doesn't appear. This self-heals
//!    the common Plan-Mode case where `Shift+Tab` bytes get dropped
//!    because the agent wasn't yet listening on its keystroke handler.
//!
//! Subsequent delays in the script (typically inter-keystroke pacing) are
//! literal sleeps.
//!
//! It also types a chat message into a session ([`type_message`]): one
//! bracketed paste, then Enter once the screen shows the paste in the input
//! box, after returning the terminal to the main conversation when an agent
//! is in view. [`check_typed_text`] decides which text may be typed that way.

use std::sync::Arc;
use std::time::Duration;

use agent_api::{Key, KeyHolder, ReadySignal, ViewGoal, ViewStep};
use base64::Engine as _;
use protocol::error_codes::{BUSY, CHAT_REFUSED};
use protocol::{AgentId, ComposerState, InjectorStartup, InjectorStep, PromptInjector};
use tokio::sync::broadcast;
use tokio::time::{Instant, sleep, timeout};
use tracing::{debug, info, warn};

use crate::chat_mirror::ChatMirror;
use crate::pty::PtyHandle;
use crate::pty_state::AgentReading;
use crate::screen::SharedScreen;
use crate::screen::dialogs::next_view_key;
use crate::session::{SessionRecord, SessionRegistry};
use crate::sync::lock;
use crate::typing_lane::{Holder, TypingTurn};
use crate::view_drive::{DriveAction, KEY_WAIT, Reading, ScreenReader, ViewDrive};

/// How long a chain waits for the typing lane when another writer holds it
/// at the spawn.
const LANE_WAIT: Duration = Duration::from_secs(20);

/// Minimum wait before declaring the TUI ready, even if no output has been
/// observed. Raised from the original 1500 ms after observing that under
/// burst spawn load (9-prompt preset) the agent's banner-paint sometimes has
/// silence gaps in the 500–1500 ms range that would otherwise trip early
/// exit on a TUI whose keystroke handler isn't yet bound.
const STARTUP_MIN_WAIT: Duration = Duration::from_secs(3);

/// Lower bound on the caller-provided startup cap. The cap is a safety
/// ceiling, not a target — the early-exit heuristic still wins on a
/// healthy spawn — so a generous floor here costs nothing on the happy
/// path but protects burst launches whose preset declares a tight cap
/// (the bundled `smoke-inline` preset historically used 6000 ms, which
/// is too tight under load).
pub(crate) const MIN_STARTUP_CAP: Duration = Duration::from_secs(15);

/// How long a shell's output must be silent before its prompt counts as
/// printed, under [`InjectorStartup::ShellPrompt`].
const SHELL_PROMPT_QUIET_FOR: Duration = Duration::from_millis(300);

/// Output a shell must print before quiescence counts, under
/// [`InjectorStartup::ShellPrompt`]: more than a lone terminal query.
const SHELL_PROMPT_MIN_OUTPUT_BYTES: usize = 32;

/// How long to wait for `verify_mode_marker` to appear in output before
/// declaring this attempt a miss and re-sending `pre_input`.
const VERIFY_WINDOW: Duration = Duration::from_secs(2);

/// Total attempts to wait for `verify_mode_marker` (1 initial + N-1
/// retries). After this we log a warning and proceed with the prompt
/// anyway — best-effort matches the existing inject contract.
const VERIFY_MAX_ATTEMPTS: u32 = 3;

/// How much PTY output (raw bytes, pre-strip) to keep buffered when
/// scanning for `verify_mode_marker`. Sized generously above the agent's
/// idle-frame size so we don't miss a marker that's only visible in
/// the bottom-of-screen footer.
const VERIFY_MARKER_SCROLLBACK_BYTES: usize = 4096;

/// How many trailing characters of stripped output to include in the
/// "marker not seen" warning log line. Bounded so noisy renders don't
/// blow up the log.
const VERIFY_TAIL_BYTES_FOR_LOG: usize = 200;

/// Spawn a background task that walks `injector.steps` in order. Returns
/// immediately. If the PTY is dropped or the child exits mid-script the
/// task will keep writing into a closed channel — the writes are silently
/// discarded by `PtyHandle::write_input`, so we just log and move on.
pub fn run(session_id: String, pty: Arc<PtyHandle>, injector: PromptInjector, ready: ReadySignal) {
    run_chain(session_id, pty, vec![injector], None, ready);
}

/// Reads how far the agent a launch typed into its shell has come.
pub type AgentUp = Box<dyn Fn() -> AgentReading + Send + Sync>;

/// An [`AgentUp`], borrowed.
type ReadAgent = dyn Fn() -> AgentReading + Send + Sync;

/// How often [`wait_for_agent`] reads how far the agent has come.
const AGENT_UP_POLL: Duration = Duration::from_millis(100);

/// The longest a first prompt is held once the agent has shown a dialog or
/// its box, before it is dropped.
const HOLD_CEILING: Duration = Duration::from_secs(600);

/// The refusal of a chat send while the chain holds the first prompt.
const FIRST_PROMPT_HELD: &str = "This session's first prompt is still waiting to be typed.";

/// [`run`] for several injectors, one after another in one background task:
/// each one's startup wait reads only the output printed after the one
/// before it finished (a shell's prompt, then the agent's TUI it launched).
/// With `agent_up`, every injector after the first is held until the agent's
/// box reads ready (see [`hold_then_type`]), so nothing meant for the agent
/// is typed into its shell or into a dialog. `ready` is the spawned agent's
/// plugin ready signal, which decides when its TUI counts as up.
pub fn run_chain(
    session_id: String,
    pty: Arc<PtyHandle>,
    injectors: Vec<PromptInjector>,
    agent_up: Option<AgentUp>,
    ready: ReadySignal,
) {
    // Subscribe BEFORE spawning the runner task so the receiver is hooked
    // up synchronously with the caller's view of the PTY — otherwise a
    // fast-booting child could emit its banner before the spawned task
    // subscribes and we'd miss the startup output we want to track
    // quiescence on. The typing lane is taken here too, so no other writer
    // types between the spawn and the chain's first key.
    let mut output_rx = Some(pty.output.subscribe());
    let turn = pty.typing().try_take(Holder::Injector);
    tokio::spawn(async move {
        let Some(turn) = first_turn(&session_id, &pty, turn).await else {
            return;
        };
        let mut turn = Some(turn);
        let chain = Chain {
            session_id: &session_id,
            pty: &pty,
            ready,
        };
        for (index, injector) in injectors.into_iter().enumerate() {
            if let Some(agent_up) = agent_up.as_deref().filter(|_| index > 0) {
                if !hold_then_type(&chain, agent_up, &mut turn, &injector).await {
                    return;
                }
                continue;
            }
            // Only a hold gives the lane up, and it ends holding it.
            let Some(held) = turn.as_ref() else {
                return;
            };
            let rx = output_rx.take().unwrap_or_else(|| pty.output.subscribe());
            let typist = Typist {
                pty: &pty,
                turn: held,
            };
            run_one(&session_id, typist, rx, injector, ready).await;
        }
    });
}

/// The typing lane for a chain's first key: `turn`, taken at the spawn, or
/// once its holder gives it back within [`LANE_WAIT`]; `None`, logged, when
/// it stays taken.
async fn first_turn(
    session_id: &str,
    pty: &Arc<PtyHandle>,
    turn: Option<TypingTurn>,
) -> Option<TypingTurn> {
    if turn.is_some() {
        return turn;
    }
    warn!(
        session_id = %session_id,
        holder = ?pty.typing().holder(),
        "another writer holds the typing lane; the injector waits"
    );
    match pty.typing().take(Holder::Injector, LANE_WAIT).await {
        Ok(turn) => Some(turn),
        Err(holder) => {
            warn!(
                session_id = %session_id,
                ?holder,
                "the typing lane stayed taken; the injector's chain is dropped"
            );
            None
        }
    }
}

/// What every injector of one chain shares: its session, its PTY and the
/// agent's ready signal.
struct Chain<'a> {
    session_id: &'a str,
    pty: &'a Arc<PtyHandle>,
    ready: ReadySignal,
}

/// The session's PTY and the typing lane held for it: everything a chain
/// types goes through the turn.
#[derive(Clone, Copy)]
struct Typist<'a> {
    pty: &'a PtyHandle,
    turn: &'a TypingTurn,
}

impl Typist<'_> {
    fn press(self, bytes: Vec<u8>) {
        self.turn.press(self.pty, bytes);
    }
}

/// Marks a session's first prompt held for as long as it lives.
struct HeldPrompt<'a>(&'a PtyHandle);

impl<'a> HeldPrompt<'a> {
    fn new(pty: &'a PtyHandle) -> Self {
        pty.set_first_prompt_held(true);
        Self(pty)
    }
}

impl Drop for HeldPrompt<'_> {
    fn drop(&mut self) {
        self.0.set_first_prompt_held(false);
    }
}

/// Hold `injector`, the agent's first prompt, until the agent's box reads
/// ready, then type it under the chain's lane. While a dialog holds the keys
/// the lane is given up, so a chat answer reaches the dialog, and a chat send
/// is refused until the prompt is typed or dropped. On ready the lane is
/// taken back and the injector's startup wait runs on fresh output; a reading
/// no longer ready after it goes back to holding. Answers whether the prompt
/// was typed; when it was not, the hold's end is logged.
async fn hold_then_type(
    chain: &Chain<'_>,
    agent_up: &ReadAgent,
    turn: &mut Option<TypingTurn>,
    injector: &PromptInjector,
) -> bool {
    let _held = HeldPrompt::new(chain.pty);
    let mut clock = HoldClock::new(MIN_STARTUP_CAP);
    loop {
        let waited = wait_for_agent(agent_up, &mut clock, &mut || {
            release_lane(chain.session_id, turn);
        })
        .await;
        if let Err(dropped) = waited {
            dropped.warn(chain.session_id);
            return false;
        }
        let mut rx = chain.pty.output.subscribe();
        let cursor = startup_wait(chain.session_id, &mut rx, injector, chain.ready).await;
        if turn.is_none() {
            *turn = chain
                .pty
                .typing()
                .take(Holder::Injector, LANE_WAIT)
                .await
                .ok();
        }
        let Some(held) = turn.as_ref() else {
            continue;
        };
        if agent_up() != AgentReading::Ready {
            *turn = None;
            continue;
        }
        info!(session_id = %chain.session_id, "the first prompt is typed");
        let typist = Typist {
            pty: chain.pty,
            turn: held,
        };
        type_steps(chain.session_id, typist, rx, injector, cursor).await;
        return true;
    }
}

/// Give up the chain's lane while a dialog holds the keys, so a chat answer
/// reaches it.
fn release_lane(session_id: &str, turn: &mut Option<TypingTurn>) {
    if turn.take().is_some() {
        info!(
            session_id = %session_id,
            "the first prompt waits behind a dialog"
        );
    }
}

/// Why a held first prompt was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dropped {
    /// The agent showed nothing within the startup cap.
    NotStarted,
    /// The shell's prompt came back after the agent was seen.
    Exited,
    /// The agent was seen but its box did not read ready within
    /// [`HOLD_CEILING`].
    Ceiling,
}

impl Dropped {
    fn warn(self, session_id: &str) {
        match self {
            Self::NotStarted | Self::Exited => warn!(
                session_id = %session_id,
                agent_exited = self == Self::Exited,
                "the agent did not start; the initial prompt was not typed"
            ),
            Self::Ceiling => warn!(
                session_id = %session_id,
                "the first prompt waited 10 minutes behind a dialog; it was not typed"
            ),
        }
    }
}

/// The time a first prompt has been held: before the agent showed a dialog
/// or its box, against `cap`; after, against [`HOLD_CEILING`].
#[derive(Debug)]
struct HoldClock {
    cap: Duration,
    starting: Duration,
    held: Duration,
    seen: bool,
}

impl HoldClock {
    fn new(cap: Duration) -> Self {
        Self {
            cap,
            starting: Duration::ZERO,
            held: Duration::ZERO,
            seen: false,
        }
    }

    /// Charge `spent` to the budget `reading` falls under.
    ///
    /// # Errors
    ///
    /// The prompt is to be dropped: the budget is spent, or the shell's
    /// prompt is back after the agent was seen.
    fn charge(&mut self, reading: AgentReading, spent: Duration) -> Result<(), Dropped> {
        match reading {
            AgentReading::Starting { shell_prompt } if self.seen => {
                if shell_prompt {
                    return Err(Dropped::Exited);
                }
                self.charge_held(spent)
            }
            AgentReading::Starting { .. } => {
                self.starting += spent;
                if self.starting >= self.cap {
                    return Err(Dropped::NotStarted);
                }
                Ok(())
            }
            AgentReading::Dialog | AgentReading::Ready => {
                self.seen = true;
                self.charge_held(spent)
            }
        }
    }

    fn charge_held(&mut self, spent: Duration) -> Result<(), Dropped> {
        self.held += spent;
        if self.held >= HOLD_CEILING {
            return Err(Dropped::Ceiling);
        }
        Ok(())
    }
}

/// Wait until `agent_up` reads ready, reading it every [`AGENT_UP_POLL`] and
/// charging the time between readings to `clock`; `on_dialog` runs at each
/// reading of a dialog.
///
/// # Errors
///
/// Why the prompt is to be dropped (see [`HoldClock::charge`]).
async fn wait_for_agent(
    agent_up: &ReadAgent,
    clock: &mut HoldClock,
    on_dialog: &mut (dyn FnMut() + Send),
) -> Result<(), Dropped> {
    let mut last = Instant::now();
    loop {
        let reading = agent_up();
        let now = Instant::now();
        clock.charge(reading, now.saturating_duration_since(last))?;
        last = now;
        match reading {
            AgentReading::Ready => return Ok(()),
            AgentReading::Dialog => on_dialog(),
            AgentReading::Starting { .. } => {}
        }
        sleep(AGENT_UP_POLL).await;
    }
}

/// Walk one injector's steps against `pty`, reading its startup from
/// `output_rx`.
async fn run_one(
    session_id: &str,
    typist: Typist<'_>,
    output_rx: broadcast::Receiver<Vec<u8>>,
    injector: PromptInjector,
    ready: ReadySignal,
) {
    let mut output_rx = output_rx;
    debug!(
        session_id = %session_id,
        steps = injector.steps.len(),
        verify_marker = ?injector.verify_mode_marker.as_deref(),
        "injector starting"
    );
    let cursor = startup_wait(session_id, &mut output_rx, &injector, ready).await;
    type_steps(session_id, typist, output_rx, &injector, cursor).await;
}

/// Phase 1 of an injector: its leading `Delay`, if any, waited as a
/// quiescence-gated startup wait on `output_rx`, not a literal sleep. The
/// index of the first step after it.
async fn startup_wait(
    session_id: &str,
    output_rx: &mut broadcast::Receiver<Vec<u8>>,
    injector: &PromptInjector,
    ready: ReadySignal,
) -> usize {
    let Some(InjectorStep::Delay { ms }) = injector.steps.first() else {
        return 0;
    };
    let rule = StartupRule::of(injector.startup, ready);
    let provided_cap = Duration::from_millis(u64::from(*ms));
    let cap = provided_cap.max(rule.min_cap);
    let waited = wait_until_ready_or_timeout(output_rx, cap, &rule).await;
    info!(
        session_id = %session_id,
        startup = ?injector.startup,
        provided_cap_ms = u64::from(*ms),
        effective_cap_ms = u64::try_from(cap.as_millis()).unwrap_or(u64::MAX),
        waited_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
        "injector startup wait done"
    );
    1
}

/// Phases 2 to 4 of an injector: its steps from `cursor` on, with the mode
/// marker verified between its pre-input and its prompt.
async fn type_steps(
    session_id: &str,
    typist: Typist<'_>,
    output_rx: broadcast::Receiver<Vec<u8>>,
    injector: &PromptInjector,
    cursor: usize,
) {
    let mut output_rx = output_rx;
    let steps = &injector.steps;
    let verify_marker = injector.verify_mode_marker.as_deref();

    // Locate the prompt boundary: the first `Text { newline: false }`
    // step after the startup delay. `build_injector` always emits
    // exactly one such step for the prompt body. Steps from `cursor`
    // up to (but not including) `pre_input_end` are pre_input;
    // `pre_input_end` onward is prompt + post_input.
    let prompt_idx = steps[cursor..]
        .iter()
        .position(|s| matches!(s, InjectorStep::Text { newline: false, .. }))
        .map(|p| p + cursor);
    let pre_input_end = prompt_idx.unwrap_or(steps.len());

    // Phase 2: pre_input
    for (offset, step) in steps[cursor..pre_input_end].iter().enumerate() {
        execute_step(session_id, typist, cursor + offset, step).await;
    }

    // Phase 3: verify the mode marker appeared, retry if not.
    // Only meaningful when there's an actual prompt boundary AND a
    // marker — verifying without a prompt boundary would re-send the
    // entire script.
    if let (Some(marker), Some(_)) = (verify_marker, prompt_idx) {
        verify_and_retry(
            session_id,
            typist,
            &mut output_rx,
            marker,
            &steps[cursor..pre_input_end],
            cursor,
        )
        .await;
    }

    // Phase 4: prompt + post_input
    for (offset, step) in steps[pre_input_end..].iter().enumerate() {
        execute_step(session_id, typist, pre_input_end + offset, step).await;
    }

    debug!(session_id = %session_id, "injector finished");
}

async fn execute_step(session_id: &str, typist: Typist<'_>, idx: usize, step: &InjectorStep) {
    match step {
        InjectorStep::Delay { ms } => {
            debug!(session_id = %session_id, step = idx, ms, "injector delay");
            sleep(Duration::from_millis(u64::from(*ms))).await;
        }
        InjectorStep::Write { data_b64 } => {
            match base64::engine::general_purpose::STANDARD.decode(data_b64) {
                Ok(bytes) => {
                    debug!(
                        session_id = %session_id,
                        step = idx,
                        len = bytes.len(),
                        "injector write"
                    );
                    typist.press(bytes);
                }
                Err(err) => {
                    warn!(
                        session_id = %session_id,
                        step = idx,
                        ?err,
                        "injector write step has invalid base64; skipping"
                    );
                }
            }
        }
        InjectorStep::Text { content, newline } => {
            let mut bytes = content.as_bytes().to_vec();
            if *newline {
                bytes.push(b'\r');
            }
            debug!(
                session_id = %session_id,
                step = idx,
                len = bytes.len(),
                newline,
                "injector text"
            );
            typist.press(bytes);
        }
        InjectorStep::Unknown => {
            warn!(
                session_id = %session_id,
                step = idx,
                "injector skipping unknown step kind (preset newer than daemon)"
            );
        }
    }
}

async fn verify_and_retry(
    session_id: &str,
    typist: Typist<'_>,
    output: &mut broadcast::Receiver<Vec<u8>>,
    marker: &str,
    pre_input: &[InjectorStep],
    pre_input_start_idx: usize,
) {
    let marker_lower = marker.to_lowercase();
    let mut buf: Vec<u8> = Vec::with_capacity(VERIFY_MARKER_SCROLLBACK_BYTES);

    for attempt in 1..=VERIFY_MAX_ATTEMPTS {
        match wait_for_marker_or_timeout(output, &marker_lower, &mut buf).await {
            MarkerOutcome::Found => {
                info!(
                    session_id = %session_id,
                    marker = %marker,
                    attempt,
                    "verify_mode_marker matched"
                );
                return;
            }
            MarkerOutcome::Timeout => {
                if attempt == VERIFY_MAX_ATTEMPTS {
                    let tail = stripped_tail(&buf, VERIFY_TAIL_BYTES_FOR_LOG);
                    warn!(
                        session_id = %session_id,
                        marker = %marker,
                        attempts = VERIFY_MAX_ATTEMPTS,
                        observed_tail = %tail,
                        "verify_mode_marker not seen after retries; proceeding without it"
                    );
                    return;
                }
                info!(
                    session_id = %session_id,
                    marker = %marker,
                    attempt,
                    "verify_mode_marker not seen; re-sending pre_input"
                );
                for (offset, step) in pre_input.iter().enumerate() {
                    execute_step(session_id, typist, pre_input_start_idx + offset, step).await;
                }
            }
        }
    }
}

enum MarkerOutcome {
    Found,
    Timeout,
}

/// Wait up to [`VERIFY_WINDOW`] for `marker_lower` (already lowercased) to
/// appear in the case-folded, ANSI-stripped tail of `buf`. `buf` is appended
/// to with each chunk received and is truncated to
/// [`VERIFY_MARKER_SCROLLBACK_BYTES`] from the tail. Caller retains the
/// buffer between attempts so a marker that arrived during one attempt's
/// re-send sleep can still be found on the next attempt's initial scan.
async fn wait_for_marker_or_timeout(
    output: &mut broadcast::Receiver<Vec<u8>>,
    marker_lower: &str,
    buf: &mut Vec<u8>,
) -> MarkerOutcome {
    // Drain anything that arrived while pre_input was being sent — output
    // ordering means the marker may already be in the broadcast queue
    // before we even start waiting.
    while let Ok(bytes) = output.try_recv() {
        append_capped(buf, &bytes, VERIFY_MARKER_SCROLLBACK_BYTES);
    }
    if check_marker(buf, marker_lower) {
        return MarkerOutcome::Found;
    }

    let deadline = Instant::now() + VERIFY_WINDOW;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return MarkerOutcome::Timeout;
        }
        let remaining = deadline.saturating_duration_since(now);
        match timeout(remaining, output.recv()).await {
            Ok(Ok(bytes)) => {
                append_capped(buf, &bytes, VERIFY_MARKER_SCROLLBACK_BYTES);
                if check_marker(buf, marker_lower) {
                    return MarkerOutcome::Found;
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                // A lag means we may have missed the marker frame. Keep
                // waiting until the window closes — the agent's footer is
                // re-emitted on each render so it should reappear.
            }
            Err(_) | Ok(Err(broadcast::error::RecvError::Closed)) => {
                return MarkerOutcome::Timeout;
            }
        }
    }
}

fn check_marker(buf: &[u8], marker_lower: &str) -> bool {
    let stripped = strip_ansi(buf);
    let tail_start = stripped.len().saturating_sub(2048);
    stripped[tail_start..].to_lowercase().contains(marker_lower)
}

fn append_capped(buf: &mut Vec<u8>, bytes: &[u8], cap: usize) {
    buf.extend_from_slice(bytes);
    if buf.len() > cap {
        let drop = buf.len() - cap;
        buf.drain(0..drop);
    }
}

fn stripped_tail(buf: &[u8], n: usize) -> String {
    let stripped = strip_ansi(buf);
    let tail_start = stripped.len().saturating_sub(n);
    stripped[tail_start..].to_string()
}

/// Strip CSI / OSC escape sequences and `\r`, dropping non-ASCII bytes.
/// Small, self-contained and only used here: the injectors match a plain
/// byte tail, while the status watchers read a rendered screen instead.
fn strip_ansi(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == 0x1B {
            if i + 1 < input.len() && input[i + 1] == b'[' {
                i += 2;
                while i < input.len() && !(0x40..=0x7E).contains(&input[i]) {
                    i += 1;
                }
                i += 1;
                continue;
            }
            if i + 1 < input.len() && input[i + 1] == b']' {
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
        if b.is_ascii() {
            out.push(b as char);
        }
        i += 1;
    }
    out
}

/// When a leading `Delay` declares the program ready: after `min_wait`,
/// once the output has been quiet for `quiet_for` and at least `min_bytes`
/// have printed; at the latest after the step's cap raised to `min_cap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StartupRule {
    min_wait: Duration,
    quiet_for: Duration,
    min_bytes: usize,
    min_cap: Duration,
}

impl StartupRule {
    /// An agent's TUI: its plugin says how much output must print and how
    /// long the stream must then stay quiet before the TUI takes a keystroke.
    fn agent_tui(ready: ReadySignal) -> Self {
        Self {
            min_wait: STARTUP_MIN_WAIT,
            quiet_for: ready.quiet_for,
            min_bytes: ready.min_output_bytes,
            min_cap: MIN_STARTUP_CAP,
        }
    }

    /// A shell, whose prompt prints in one short burst. The byte gate skips
    /// a lone terminal query (the pseudo console's 4-byte `ESC[6n`), which comes before
    /// the shell has started. The tracer answers and removes that query, so only a
    /// session on an older tracer shows it here.
    const SHELL_PROMPT: Self = Self {
        min_wait: Duration::ZERO,
        quiet_for: SHELL_PROMPT_QUIET_FOR,
        min_bytes: SHELL_PROMPT_MIN_OUTPUT_BYTES,
        min_cap: Duration::ZERO,
    };

    fn of(startup: InjectorStartup, ready: ReadySignal) -> Self {
        match startup {
            InjectorStartup::ShellPrompt => Self::SHELL_PROMPT,
            InjectorStartup::AgentTui | InjectorStartup::Unknown => Self::agent_tui(ready),
        }
    }
}

/// Wait for the PTY output stream to be quiet for `rule.quiet_for`
/// continuously, with `rule.min_wait` as a floor, at least `rule.min_bytes`
/// bytes observed, and the caller's `cap` as a ceiling. Returns the actual
/// time waited.
async fn wait_until_ready_or_timeout(
    output: &mut broadcast::Receiver<Vec<u8>>,
    cap: Duration,
    rule: &StartupRule,
) -> Duration {
    let started = Instant::now();
    let deadline = started + cap;
    let mut last_activity = started;
    let mut total_bytes: usize = 0;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return now.saturating_duration_since(started);
        }
        let waited = now.saturating_duration_since(started);
        let quiet_for = now.saturating_duration_since(last_activity);
        let need_floor = waited < rule.min_wait;
        let need_quiet = quiet_for < rule.quiet_for;
        let need_bytes = total_bytes < rule.min_bytes;
        if !need_floor && !need_quiet && !need_bytes {
            return waited;
        }
        // Sleep until the next threshold could plausibly change. When the
        // only thing blocking us is the byte gate, we have no timer-based
        // signal — only an output chunk or the cap will move us — so we
        // wait until the cap (avoiding a tight loop with `wait_for == 0`).
        let mut next_wake = deadline;
        if need_floor {
            next_wake = next_wake.min(started + rule.min_wait);
        }
        if need_quiet {
            next_wake = next_wake.min(last_activity + rule.quiet_for);
        }
        let wait_for = next_wake.saturating_duration_since(now);
        match timeout(wait_for, output.recv()).await {
            Err(_) => {} // timer fired; loop iteration re-checks thresholds
            Ok(Ok(bytes)) => {
                last_activity = Instant::now();
                total_bytes = total_bytes.saturating_add(bytes.len());
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                last_activity = Instant::now();
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                return Instant::now().saturating_duration_since(started);
            }
        }
    }
}

/// The most bytes a typed chat message may hold.
pub const MAX_TYPED_BYTES: usize = 65_536;

/// How long a chat send waits for the typing lane.
const SEND_LANE_WAIT: Duration = Duration::from_millis(250);

/// The longest a chat send types for, from taking the lane to its Enter.
const SEND_CEILING: Duration = Duration::from_secs(30);

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

pub(crate) const NO_RESPONSE: &str = "the terminal did not respond";
const LEFT_IN_BOX: &str = "the message was left in the box";
const DRAFT: &str = "a draft is in the terminal's box";
const DIALOG: &str = "a dialog is open in the terminal";
const NOT_AT_PROMPT: &str = "the terminal is not at its prompt";
const NO_RETURN: &str = "the terminal could not return to the main conversation";
pub(crate) const OWNER_TYPED: &str = "someone typed in the terminal";
const NOT_SHOWN: &str = "the message did not appear in the box; it may be left there";
const AGENT_ENDED: &str = "the agent is not running in this session";

/// `text` as a chat send types it: `\r\n` made `\n`.
///
/// # Errors
///
/// Why the text may not be typed: it is empty or only whitespace, it holds a
/// C0 or C1 control character or DEL other than a newline or a tab (an
/// escape could end the paste early and type keys), or it is over
/// [`MAX_TYPED_BYTES`].
pub fn check_typed_text(text: &str) -> Result<String, &'static str> {
    let text = text.replace("\r\n", "\n");
    if text.trim().is_empty() {
        return Err("the message is empty");
    }
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err("the message holds a control character");
    }
    if text.len() > MAX_TYPED_BYTES {
        return Err("the message is too long");
    }
    Ok(text)
}

/// The session a chat message is typed into.
pub struct TypeTarget {
    pub session_id: String,
    pub agent: AgentId,
    pub pty: Arc<PtyHandle>,
    pub screen: SharedScreen,
    pub sessions: Arc<SessionRegistry>,
    pub mirror: ChatMirror,
}

/// Why a chat message was not typed, or not sent: the `Error` that answers
/// its send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRefusal {
    pub code: &'static str,
    pub message: String,
}

impl TypeRefusal {
    pub(crate) fn refused(message: impl Into<String>) -> Self {
        Self {
            code: CHAT_REFUSED,
            message: message.into(),
        }
    }

    /// A refusal after the paste, which stays in the terminal's box.
    pub(crate) fn left_in_box(reason: &str) -> Self {
        Self::refused(format!("{reason}; {LEFT_IN_BOX}"))
    }

    fn busy(holder: Option<Holder>) -> Self {
        let message = match holder {
            Some(Holder::ViewReturn) => "the terminal is switching views",
            Some(Holder::Injector) => "the session is starting",
            Some(Holder::ChatSend | Holder::ChatAnswer) => "another message is being typed",
            None => "the terminal is busy",
        };
        Self {
            code: BUSY,
            message: message.to_owned(),
        }
    }
}

/// Type `text`, already passed by [`check_typed_text`], into `target`'s
/// terminal as the user's message: return to the main conversation if an
/// agent is in view, paste the text, and press Enter once the box shows it,
/// adding the chat's pending entry for it just before. Holds the session's
/// typing lane throughout, and gives it back one settled reading after the
/// Enter. A send clears no pending prompt.
///
/// # Errors
///
/// The session's first prompt is still held by its chain, or the lane is held
/// by another writer ([`BUSY`]), or the terminal's state
/// does not take the message, or changed under the send, or did not answer
/// within [`SEND_CEILING`] ([`CHAT_REFUSED`]).
pub async fn type_message(target: &TypeTarget, text: &str) -> Result<(), TypeRefusal> {
    // A message typed now would reach the agent ahead of its first prompt.
    if target.pty.first_prompt_held() {
        return Err(TypeRefusal {
            code: BUSY,
            message: FIRST_PROMPT_HELD.to_owned(),
        });
    }
    let turn = take_lane(target, Holder::ChatSend).await?;
    let mut reader = screen_reader(target);
    let mut pasted = false;
    let typed = timeout(
        SEND_CEILING,
        type_message_under(target, &turn, &mut reader, text, &mut pasted),
    )
    .await;
    match typed {
        Ok(sent) => sent?,
        Err(_) if pasted => return Err(TypeRefusal::left_in_box(NO_RESPONSE)),
        Err(_) => return Err(TypeRefusal::refused(NO_RESPONSE)),
    }
    // The next writer starts from the frame the Enter brought.
    reader.after_key().await;
    drop(turn);
    Ok(())
}

/// `target`'s typing lane for `who`, a chat writer: refused at once while a
/// send or an answer holds it, else once its holder has kept it for
/// [`SEND_LANE_WAIT`].
///
/// # Errors
///
/// Another writer holds the lane ([`BUSY`]).
pub(crate) async fn take_lane(target: &TypeTarget, who: Holder) -> Result<TypingTurn, TypeRefusal> {
    let lane = target.pty.typing();
    // Another message is refused at once, never queued behind the one typed.
    if let Some(holder @ (Holder::ChatSend | Holder::ChatAnswer)) = lane.holder() {
        return Err(TypeRefusal::busy(Some(holder)));
    }
    lane.take(who, SEND_LANE_WAIT)
        .await
        .map_err(TypeRefusal::busy)
}

/// A reader of `target`'s screen, for a writer that types into it.
pub(crate) fn screen_reader(target: &TypeTarget) -> ScreenReader {
    ScreenReader::new(
        Arc::clone(&target.pty),
        Arc::clone(&target.screen),
        target.agent.clone(),
    )
}

/// [`type_message`]'s steps up to its Enter, under a `turn` of the lane its
/// caller already holds, read through `reader`; sets `pasted` once the paste
/// is typed. Gives no ceiling and reads nothing after the Enter: those are
/// the caller's.
///
/// # Errors
///
/// As [`type_message`], but for the lane and the ceiling.
pub async fn type_message_under(
    target: &TypeTarget,
    turn: &TypingTurn,
    reader: &mut ScreenReader,
    text: &str,
    pasted: &mut bool,
) -> Result<(), TypeRefusal> {
    let reading = reader.current().await;
    gate(target, &reading)?;
    let in_agent_view = reading
        .view
        .as_ref()
        .is_some_and(|view| view.agent_in_view.is_some());
    let reading = if in_agent_view {
        return_to_main(target, turn, reader, reading).await?
    } else {
        reading
    };
    gate_in_main(target, &reading)?;
    reader.arm();
    turn.press(&target.pty, bracketed_paste(text));
    *pasted = true;
    wait_for_paste(reader).await?;
    press_enter(target, turn, reader, text)
}

/// `text` as one bracketed paste.
fn bracketed_paste(text: &str) -> Vec<u8> {
    [PASTE_START, text.as_bytes(), PASTE_END].concat()
}

/// Whether a send of `text` is a command (`/compact`, `!ls`), whose
/// transcript record never becomes a user entry.
fn is_command(text: &str) -> bool {
    text.trim_start().starts_with(['/', '!'])
}

/// The refusal for a prompt the session's record says is waiting for an
/// answer; `None` when none is (or the record is gone).
fn waiting_prompt(target: &TypeTarget) -> Option<TypeRefusal> {
    let record = target.sessions.get(&target.session_id)?;
    let record = lock(&record);
    waiting_refusal(target, &record)
}

/// The refusal for a prompt `record` says is waiting for an answer.
fn waiting_refusal(target: &TypeTarget, record: &SessionRecord) -> Option<TypeRefusal> {
    record.pending_input.is_some().then(|| {
        let label = agent_registry::plugin(&target.agent).map_or_else(
            || target.agent.0.clone(),
            |plugin| plugin.descriptor().label.clone(),
        );
        TypeRefusal::refused(format!("{label} is waiting for an answer in the terminal"))
    })
}

/// The refusal for a session whose record says its agent, typed into its
/// shell, has ended: the shell would run the message as a command.
fn agent_ended(target: &TypeTarget) -> Option<TypeRefusal> {
    let record = target.sessions.get(&target.session_id)?;
    let record = lock(&record);
    let shell_named = record.shell_label.is_some() && record.program_name == record.shell_label;
    (record.agent_in_shell && (record.agent_exit_announced || shell_named))
        .then(|| TypeRefusal::refused(AGENT_ENDED))
}

/// Refuse a reading that cannot take a message in any view: the agent
/// ended, a prompt waiting, a draft in the box, a dialog, or no prompt on
/// screen.
fn gate(target: &TypeTarget, reading: &Reading) -> Result<(), TypeRefusal> {
    if let Some(refusal) = agent_ended(target).or_else(|| waiting_prompt(target)) {
        return Err(refusal);
    }
    let Some(view) = reading.view.as_ref() else {
        return Err(TypeRefusal::refused(NOT_AT_PROMPT));
    };
    let composer = reading.chat.as_ref().map(|chat| &chat.composer);
    // A draft whose first row is blank shows no first line, but its
    // composer reads as a draft.
    if view.box_first_line.is_some() || composer == Some(&ComposerState::Draft) {
        return Err(TypeRefusal::refused(DRAFT));
    }
    // An overlay also reads as a dialog in the view; it is named first.
    if let None | Some(ComposerState::Overlay { .. } | ComposerState::Unknown) = composer {
        return Err(TypeRefusal::refused(NOT_AT_PROMPT));
    }
    if view.dialog {
        return Err(TypeRefusal::refused(DIALOG));
    }
    Ok(())
}

/// [`gate`], and the main conversation in view with its box empty and
/// taking text.
fn gate_in_main(target: &TypeTarget, reading: &Reading) -> Result<(), TypeRefusal> {
    gate(target, reading)?;
    let in_main = reading
        .view
        .as_ref()
        .is_some_and(|view| next_view_key(&target.agent, ViewGoal::Main, view) == ViewStep::Done);
    if !in_main {
        return Err(TypeRefusal::refused(NO_RETURN));
    }
    match reading.chat.as_ref().map(|chat| &chat.composer) {
        Some(ComposerState::Ready | ComposerState::Working) => Ok(()),
        Some(ComposerState::Dialog) => Err(TypeRefusal::refused(DIALOG)),
        _ => Err(TypeRefusal::refused(NOT_AT_PROMPT)),
    }
}

/// Drive the terminal from an agent's view, read as `reading`, back to the
/// main conversation, one key per settled reading; the reading that shows
/// main.
pub(crate) async fn return_to_main(
    target: &TypeTarget,
    turn: &TypingTurn,
    reader: &mut ScreenReader,
    mut reading: Reading,
) -> Result<Reading, TypeRefusal> {
    let mut drive = ViewDrive::new(ViewGoal::Main, Instant::now());
    loop {
        let Some(view) = reading.view.as_ref() else {
            return Err(TypeRefusal::refused(NO_RETURN));
        };
        let step = next_view_key(&target.agent, ViewGoal::Main, view);
        let key = match drive.step(view, step, Instant::now()) {
            DriveAction::Press(key) => key,
            DriveAction::Reached => break,
            DriveAction::Failed(failure) => {
                debug!(session_id = %target.session_id, ?failure, "chat send: the return to main failed");
                return Err(TypeRefusal::refused(NO_RETURN));
            }
        };
        if turn.owner_typed_since() {
            return Err(TypeRefusal::refused(OWNER_TYPED));
        }
        if key == Key::Enter
            && let Some(refusal) = waiting_prompt(target)
        {
            return Err(refusal);
        }
        reader.arm();
        turn.press(&target.pty, key.to_bytes());
        reading = reader.after_key().await;
    }
    let still_away = reading
        .view
        .as_ref()
        .is_none_or(|view| view.agent_in_view.is_some());
    if still_away {
        return Err(TypeRefusal::refused(NO_RETURN));
    }
    Ok(reading)
}

/// Wait up to [`KEY_WAIT`] for a settled reading of main with the paste in
/// its box, which holds the keys.
async fn wait_for_paste(reader: &mut ScreenReader) -> Result<(), TypeRefusal> {
    let deadline = Instant::now() + KEY_WAIT;
    while let Some(reading) = reader.next_before(deadline).await {
        let shows = reading.view.as_ref().is_some_and(|view| {
            view.box_first_line.is_some()
                && !view.dialog
                && view.agent_in_view.is_none()
                && view.key_holder == KeyHolder::Box
        });
        if shows {
            return Ok(());
        }
    }
    Err(TypeRefusal::refused(NOT_SHOWN))
}

/// Press Enter under the record's and the screen's locks, once the screen
/// still shows the paste in main's box with no dialog and no owner key or
/// waiting prompt came, adding the chat's pending entry just before (none
/// for a command).
fn press_enter(
    target: &TypeTarget,
    turn: &TypingTurn,
    reader: &mut ScreenReader,
    text: &str,
) -> Result<(), TypeRefusal> {
    #[cfg(test)]
    enter_seam::run(&target.pty);
    let output = reader.subscribe();
    // The lock order is record, screen, mirror. Both are held from the
    // check to the key, so no prompt or frame comes between them.
    let record = target.sessions.get(&target.session_id);
    let record = record.as_deref().map(lock);
    let waiting = record
        .as_deref()
        .and_then(|record| waiting_refusal(target, record));
    let screen = lock(&target.screen);
    let reading = Reading::of(&target.agent, &screen);
    if let Some(refusal) = enter_refusal(&reading, turn, waiting) {
        return Err(refusal);
    }
    let pending = if is_command(text) {
        None
    } else {
        target.mirror.note_sent(&target.session_id, text)
    };
    debug!(session_id = %target.session_id, ?pending, "chat send: pressing Enter");
    reader.arm_at(output, screen.fed());
    turn.press(&target.pty, Key::Enter.to_bytes());
    Ok(())
}

/// Why the Enter may not be pressed on `reading`; `waiting` is the refusal
/// for a prompt the record says is waiting.
fn enter_refusal(
    reading: &Reading,
    turn: &TypingTurn,
    waiting: Option<TypeRefusal>,
) -> Option<TypeRefusal> {
    let Some(view) = reading.view.as_ref() else {
        return Some(TypeRefusal::left_in_box(NOT_AT_PROMPT));
    };
    if view.dialog {
        return Some(TypeRefusal::left_in_box(DIALOG));
    }
    if view.box_first_line.is_none() {
        return Some(TypeRefusal::refused(NOT_SHOWN));
    }
    if view.agent_in_view.is_some() {
        return Some(TypeRefusal::left_in_box(NO_RETURN));
    }
    if turn.owner_typed_since() {
        return Some(TypeRefusal::left_in_box(OWNER_TYPED));
    }
    waiting.map(|refusal| TypeRefusal::left_in_box(&refusal.message))
}

/// The enter seam: a test's hook that [`press_enter`] runs once the paste
/// has shown and before it takes the locks for the Enter's check, so a test
/// can change the screen or the record in that window.
#[cfg(test)]
pub(crate) mod enter_seam {
    use std::sync::{Arc, Mutex};

    use crate::pty::PtyHandle;
    use crate::sync::lock;

    type Hook = Box<dyn FnOnce() + Send>;

    /// The hooks set, each by the address of the terminal it is for.
    static HOOKS: Mutex<Vec<(usize, Hook)>> = Mutex::new(Vec::new());

    /// Run `hook` at the next Enter a chat send presses into `pty`.
    pub(crate) fn set(pty: &Arc<PtyHandle>, hook: impl FnOnce() + Send + 'static) {
        lock(&HOOKS).push((key(pty), Box::new(hook)));
    }

    /// Run and forget the hook set for `pty`, if any.
    pub(super) fn run(pty: &Arc<PtyHandle>) {
        let hook = {
            let mut hooks = lock(&HOOKS);
            let at = hooks.iter().position(|(held, _)| *held == key(pty));
            at.map(|at| hooks.remove(at).1)
        };
        if let Some(hook) = hook {
            hook();
        }
    }

    fn key(pty: &Arc<PtyHandle>) -> usize {
        Arc::as_ptr(pty).addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;

    #[tokio::test(start_paused = true)]
    async fn the_injector_holds_the_lane_for_its_chain() {
        let (pty, mut input, _exit) = crate::history::test_support::recording_pty();
        let injector = PromptInjector {
            steps: vec![
                InjectorStep::Text {
                    content: "first".to_owned(),
                    newline: true,
                },
                InjectorStep::Delay { ms: 1000 },
                InjectorStep::Text {
                    content: "second".to_owned(),
                    newline: true,
                },
            ],
            verify_mode_marker: None,
            startup: InjectorStartup::AgentTui,
        };
        run(
            "s1".to_owned(),
            Arc::clone(&pty),
            injector,
            ReadySignal::default(),
        );
        assert_eq!(
            pty.typing().holder(),
            Some(Holder::Injector),
            "the lane is taken before the chain's task runs"
        );
        sleep(Duration::from_millis(10)).await;
        assert_eq!(input.try_recv().ok(), Some(b"first\r".to_vec()));
        assert_eq!(
            pty.typing().holder(),
            Some(Holder::Injector),
            "the lane is held between the chain's steps"
        );
        assert!(
            pty.typing().try_take(Holder::ViewReturn).is_none(),
            "no other writer gets in between"
        );
        sleep(Duration::from_secs(1)).await;
        assert_eq!(input.try_recv().ok(), Some(b"second\r".to_vec()));
        assert_eq!(
            pty.typing().holder(),
            None,
            "the lane is given back once the chain ends"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_initial_prompt_waits_for_the_agent_to_start() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let up = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&up);
        tokio::spawn(async move {
            sleep(Duration::from_secs(5)).await;
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let agent_up = move || {
            if up.load(Ordering::SeqCst) {
                AgentReading::Ready
            } else {
                STARTING
            }
        };
        let mut clock = HoldClock::new(MIN_STARTUP_CAP);
        assert!(
            wait_for_agent(&agent_up, &mut clock, &mut || {})
                .await
                .is_ok()
        );
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_secs(5) && waited < Duration::from_secs(6),
            "the prompt is released once the agent is up, not before: {waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_agent_start_drops_the_initial_prompt() {
        let started = Instant::now();
        let mut clock = HoldClock::new(MIN_STARTUP_CAP);
        assert!(
            wait_for_agent(&|| STARTING, &mut clock, &mut || {})
                .await
                .is_err(),
            "an agent that never starts never gets its prompt"
        );
        assert!(started.elapsed() >= MIN_STARTUP_CAP);
    }

    /// A reading that has not seen the agent.
    const STARTING: AgentReading = AgentReading::Starting {
        shell_prompt: false,
    };

    /// The reading a test sets, and the [`AgentUp`] that reads it.
    fn scripted(first: AgentReading) -> (Arc<std::sync::Mutex<AgentReading>>, AgentUp) {
        let reading = Arc::new(std::sync::Mutex::new(first));
        let read = Arc::clone(&reading);
        (reading, Box::new(move || *lock(&read)))
    }

    fn set(reading: &std::sync::Mutex<AgentReading>, to: AgentReading) {
        *lock(reading) = to;
    }

    /// The launch line, then the agent's first prompt, as a spawn into a
    /// shell chains them.
    fn launch_then_prompt() -> Vec<PromptInjector> {
        vec![
            PromptInjector {
                steps: vec![InjectorStep::Text {
                    content: "claude".to_owned(),
                    newline: true,
                }],
                verify_mode_marker: None,
                startup: InjectorStartup::ShellPrompt,
            },
            PromptInjector {
                steps: vec![
                    InjectorStep::Delay { ms: 1000 },
                    InjectorStep::Text {
                        content: "hello".to_owned(),
                        newline: false,
                    },
                    InjectorStep::Text {
                        content: String::new(),
                        newline: true,
                    },
                ],
                verify_mode_marker: None,
                startup: InjectorStartup::AgentTui,
            },
        ]
    }

    /// The writes typed and not yet read.
    fn typed_now(input: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<Vec<u8>> {
        let mut writes = Vec::new();
        while let Ok(write) = input.try_recv() {
            writes.push(write);
        }
        writes
    }

    /// The first prompt's writes: its text, then Enter.
    fn prompt_writes() -> Vec<Vec<u8>> {
        vec![b"hello".to_vec(), b"\r".to_vec()]
    }

    /// Long enough for the first prompt's startup wait, which no output
    /// ends early here, to run to its cap.
    const STARTUP_RUN: Duration = Duration::from_secs(16);

    #[tokio::test(start_paused = true)]
    async fn the_first_prompt_waits_behind_a_dialog_and_types_when_ready() {
        let (pty, mut input, _exit) = crate::history::test_support::recording_pty();
        let (reading, agent_up) = scripted(AgentReading::Dialog);
        let chain = launch_then_prompt();
        run_chain(
            "s1".to_owned(),
            Arc::clone(&pty),
            chain,
            Some(agent_up),
            ReadySignal::default(),
        );
        sleep(Duration::from_millis(10)).await;
        assert_eq!(typed_now(&mut input), [b"claude\r".to_vec()]);
        sleep(Duration::from_secs(30)).await;
        assert_eq!(
            typed_now(&mut input),
            Vec::<Vec<u8>>::new(),
            "nothing is typed while a dialog holds the keys"
        );
        set(&reading, AgentReading::Ready);
        sleep(STARTUP_RUN).await;
        assert_eq!(typed_now(&mut input), prompt_writes(), "typed once ready");
        sleep(STARTUP_RUN).await;
        assert_eq!(typed_now(&mut input), Vec::<Vec<u8>>::new(), "typed once");
        assert!(!pty.first_prompt_held());
        assert_eq!(pty.typing().holder(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_chat_answer_goes_through_while_the_first_prompt_is_held() {
        let (pty, mut input, _exit) = crate::history::test_support::recording_pty();
        let (_reading, agent_up) = scripted(AgentReading::Dialog);
        let chain = launch_then_prompt();
        run_chain(
            "s1".to_owned(),
            Arc::clone(&pty),
            chain,
            Some(agent_up),
            ReadySignal::default(),
        );
        sleep(Duration::from_secs(1)).await;
        assert_eq!(typed_now(&mut input), [b"claude\r".to_vec()]);
        assert!(pty.first_prompt_held(), "the first prompt is held");
        let answer = pty.typing().try_take(Holder::ChatAnswer);
        assert!(answer.is_some(), "the lane is free while a dialog is up");
        if let Some(answer) = answer {
            answer.press(&pty, b"1".to_vec());
        }
        assert_eq!(typed_now(&mut input), [b"1".to_vec()]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_held_first_prompt_is_dropped_when_the_agent_exits() {
        let (pty, mut input, _exit) = crate::history::test_support::recording_pty();
        let (reading, agent_up) = scripted(AgentReading::Dialog);
        let chain = launch_then_prompt();
        run_chain(
            "s1".to_owned(),
            Arc::clone(&pty),
            chain,
            Some(agent_up),
            ReadySignal::default(),
        );
        sleep(Duration::from_secs(5)).await;
        set(&reading, AgentReading::Starting { shell_prompt: true });
        sleep(Duration::from_secs(1)).await;
        assert!(!pty.first_prompt_held(), "the prompt is dropped");
        set(&reading, AgentReading::Ready);
        sleep(STARTUP_RUN).await;
        assert_eq!(
            typed_now(&mut input),
            [b"claude\r".to_vec()],
            "the prompt is never typed into the shell"
        );
        assert_eq!(pty.typing().holder(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn the_held_first_prompt_is_dropped_after_ten_minutes() {
        let (pty, mut input, _exit) = crate::history::test_support::recording_pty();
        let (reading, agent_up) = scripted(AgentReading::Dialog);
        let chain = launch_then_prompt();
        run_chain(
            "s1".to_owned(),
            Arc::clone(&pty),
            chain,
            Some(agent_up),
            ReadySignal::default(),
        );
        sleep(HOLD_CEILING.saturating_sub(Duration::from_secs(1))).await;
        assert!(pty.first_prompt_held(), "still held before the ceiling");
        sleep(Duration::from_secs(2)).await;
        assert!(!pty.first_prompt_held(), "dropped at the ceiling");
        set(&reading, AgentReading::Ready);
        sleep(STARTUP_RUN).await;
        assert_eq!(typed_now(&mut input), [b"claude\r".to_vec()]);
    }

    #[tokio::test(start_paused = true)]
    async fn dialog_time_does_not_count_against_the_startup_cap() {
        let (pty, mut input, _exit) = crate::history::test_support::recording_pty();
        let (reading, agent_up) = scripted(STARTING);
        let chain = launch_then_prompt();
        run_chain(
            "s1".to_owned(),
            Arc::clone(&pty),
            chain,
            Some(agent_up),
            ReadySignal::default(),
        );
        sleep(Duration::from_secs(10)).await;
        set(&reading, AgentReading::Dialog);
        sleep(Duration::from_secs(60)).await;
        assert!(pty.first_prompt_held(), "still held behind the dialog");
        assert_eq!(
            typed_now(&mut input),
            [b"claude\r".to_vec()],
            "nothing but the launch line is typed while the dialog is up"
        );
        set(&reading, AgentReading::Ready);
        sleep(STARTUP_RUN).await;
        assert_eq!(typed_now(&mut input), prompt_writes());
    }

    #[tokio::test(start_paused = true)]
    async fn sufficient_output_then_quiescence_exits_early() {
        // 1500 bytes (above the plugin's byte gate) arrive immediately, then
        // silence. We should exit at STARTUP_MIN_WAIT (the floor), well
        // before the cap. The paused clock makes the wake instant exact.
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(16);
        assert!(tx.send(vec![b'.'; 1500]).is_ok());
        let cap = Duration::from_secs(20);
        let rule = StartupRule::of(InjectorStartup::AgentTui, ReadySignal::default());
        let waited = wait_until_ready_or_timeout(&mut rx, cap, &rule).await;
        assert_eq!(
            waited, STARTUP_MIN_WAIT,
            "should exit at the floor once quiet and the byte gate are met; waited {waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn agent_tui_startup_uses_the_plugins_ready_signal() {
        // A plugin whose TUI counts as up after 10 bytes and 200 ms of
        // silence: 10 bytes arrive and never reach Claude's 1024-byte gate,
        // so the 3 s floor ends the wait. Claude's own numbers would block
        // on the byte gate and run to the cap.
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(16);
        assert!(tx.send(vec![b'.'; 10]).is_ok());
        let ready = ReadySignal {
            min_output_bytes: 10,
            quiet_for: Duration::from_millis(200),
        };
        let rule = StartupRule::of(InjectorStartup::AgentTui, ready);
        let cap = Duration::from_secs(20);
        let waited = wait_until_ready_or_timeout(&mut rx, cap, &rule).await;
        assert!(
            waited < cap,
            "the plugin's byte gate is met, so the cap never fires; waited {waited:?}"
        );
        assert_eq!(
            waited, STARTUP_MIN_WAIT,
            "the floor alone ends the wait once quiet and the byte gate are met; waited {waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn min_output_bytes_blocks_early_exit() {
        // Only 200 bytes seen (below the plugin's 1024-byte gate). Without
        // this gate, the silence + floor would trigger early exit; with it,
        // we must hit the cap. Cap chosen low so the test is fast.
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(16);
        assert!(tx.send(vec![b'.'; 200]).is_ok());
        let cap = Duration::from_millis(500);
        let rule = StartupRule::of(InjectorStartup::AgentTui, ReadySignal::default());
        let waited = wait_until_ready_or_timeout(&mut rx, cap, &rule).await;
        // With a sub-floor cap and insufficient bytes, the deadline is
        // the only wake left: exactly the cap, never an early exit.
        assert_eq!(
            waited, cap,
            "should hit cap when output is below the byte gate; waited {waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_output_hits_cap() {
        // No output at all: total_bytes stays at 0 forever, the byte gate
        // blocks quiescence exit, we hit the cap.
        let (_tx, mut rx) = broadcast::channel::<Vec<u8>>(16);
        let cap = Duration::from_millis(500);
        let rule = StartupRule::of(InjectorStartup::AgentTui, ReadySignal::default());
        let waited = wait_until_ready_or_timeout(&mut rx, cap, &rule).await;
        assert_eq!(
            waited, cap,
            "should hit cap with no output; waited {waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cap_hit_when_output_never_settles() {
        // Continuous output keeps refreshing last_activity, so we should
        // hit the ceiling instead of declaring ready. The 32-byte sends
        // keep the byte gate shut for the whole window, so the deadline is
        // the only exit. Under the paused clock the producer's sleeps
        // auto-advance and the deadline is reached exactly.
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(16);
        let cap = Duration::from_millis(800);
        let producer = tokio::spawn(async move {
            loop {
                if tx.send(vec![b'.'; 32]).is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        let rule = StartupRule::of(InjectorStartup::AgentTui, ReadySignal::default());
        let waited = wait_until_ready_or_timeout(&mut rx, cap, &rule).await;
        producer.abort();
        assert_eq!(
            waited, cap,
            "should hit cap when output never settles; waited {waited:?}"
        );
    }

    #[test]
    fn claude_ready_signal_keeps_the_agent_tui_rule() {
        let ready = agent_api::AgentPlugin::ready_signal(&agent_claude::CLAUDE);
        let rule = StartupRule::of(InjectorStartup::AgentTui, ready);
        assert_eq!(
            rule,
            StartupRule {
                min_wait: Duration::from_secs(3),
                quiet_for: Duration::from_millis(1500),
                min_bytes: 1024,
                min_cap: Duration::from_secs(15),
            }
        );
        assert_eq!(StartupRule::of(InjectorStartup::Unknown, ready), rule);
    }

    #[test]
    fn shell_prompt_rule_keeps_the_steps_own_cap() {
        let rule = StartupRule::of(InjectorStartup::ShellPrompt, ReadySignal::default());
        let cap = Duration::from_millis(2000);
        assert_eq!(cap.max(rule.min_cap), cap);
        assert_eq!(rule.min_wait, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn shell_prompt_is_ready_shortly_after_the_prompt_prints() {
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(16);
        assert!(tx.send(b"D:\\proj>".repeat(8)).is_ok());
        let cap = Duration::from_secs(2);
        let waited = wait_until_ready_or_timeout(&mut rx, cap, &StartupRule::SHELL_PROMPT).await;
        // The prompt's quiet period is the only gate left once the bytes
        // have printed (min_wait is zero for a shell), so we wake exactly
        // then, never at the cap.
        assert_eq!(
            waited, SHELL_PROMPT_QUIET_FOR,
            "should wait exactly the quiet period once the prompt printed; waited {waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shell_prompt_is_not_ready_on_a_lone_terminal_query() {
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(16);
        assert!(tx.send(b"\x1b[6n".to_vec()).is_ok());
        let cap = Duration::from_millis(800);
        let waited = wait_until_ready_or_timeout(&mut rx, cap, &StartupRule::SHELL_PROMPT).await;
        // Four bytes stay below the shell byte gate forever, so the cap is
        // the only exit.
        assert_eq!(
            waited, cap,
            "a cursor query alone is not a prompt; waited {waited:?}"
        );
    }

    #[tokio::test]
    async fn marker_found_immediately_in_pre_existing_buffer() {
        // Marker already present in the rolling buffer (simulates output
        // arriving during pre_input keystrokes — drained on next attempt).
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(16);
        assert!(
            tx.send(b"... plan mode (shift+tab to cycle) ...".to_vec())
                .is_ok()
        );
        let mut buf = Vec::new();
        let outcome = wait_for_marker_or_timeout(&mut rx, "plan mode", &mut buf).await;
        assert!(matches!(outcome, MarkerOutcome::Found));
    }

    #[tokio::test(start_paused = true)]
    async fn marker_found_after_arriving_mid_window() {
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(16);
        let producer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = tx.send(b"Plan Mode on".to_vec());
        });
        let mut buf = Vec::new();
        let outcome = wait_for_marker_or_timeout(&mut rx, "plan mode", &mut buf).await;
        let _ = producer.await;
        assert!(matches!(outcome, MarkerOutcome::Found));
    }

    #[tokio::test(start_paused = true)]
    async fn marker_absent_times_out() {
        // Stream emits noise but never the marker; expect Timeout within
        // VERIFY_WINDOW. The producer ends after its sends, closing the
        // channel, so the wait returns then — but never past the window.
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(16);
        let producer = tokio::spawn(async move {
            for _ in 0..5 {
                let _ = tx.send(b"unrelated noise".to_vec());
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        let mut buf = Vec::new();
        let started = Instant::now();
        let outcome = wait_for_marker_or_timeout(&mut rx, "plan mode", &mut buf).await;
        let elapsed = started.elapsed();
        producer.abort();
        assert!(matches!(outcome, MarkerOutcome::Timeout));
        assert!(
            elapsed <= VERIFY_WINDOW,
            "the wait never overruns the verify window; elapsed {elapsed:?}"
        );
    }

    #[test]
    fn check_typed_text_keeps_lines_and_tabs_and_normalises_crlf() {
        assert_eq!(
            check_typed_text("one\r\ntwo\n\tthree"),
            Ok("one\ntwo\n\tthree".to_owned())
        );
    }

    #[test]
    fn check_typed_text_refuses_empty_control_and_long_text() {
        let long = "a".repeat(MAX_TYPED_BYTES + 1);
        let cases = [
            ("", "the message is empty"),
            (" \n\t\r\n ", "the message is empty"),
            ("hi\x1b[201~\r", "the message holds a control character"),
            ("stop\x03", "the message holds a control character"),
            ("lone\rreturn", "the message holds a control character"),
            ("del\x7f", "the message holds a control character"),
            ("csi\u{9b}201~", "the message holds a control character"),
            ("c1 start\u{80}", "the message holds a control character"),
            ("c1 end\u{9f}", "the message holds a control character"),
            (long.as_str(), "the message is too long"),
        ];
        for (text, why) in cases {
            assert_eq!(check_typed_text(text), Err(why), "{text:?}");
        }
        let at_limit = "a".repeat(MAX_TYPED_BYTES);
        assert_eq!(check_typed_text(&at_limit), Ok(at_limit.clone()));
    }

    #[test]
    fn strip_ansi_drops_csi_osc_and_keeps_text() {
        let raw = b"\x1b[2J\x1b[?25hhello \x1b]0;title\x07world\r";
        assert_eq!(strip_ansi(raw), "hello world");
    }
}
