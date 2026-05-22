//! Scripted PTY input runner — drives a [`protocol::PromptInjector`] against
//! a live [`PtyHandle`] after spawn.
//!
//! Used by the preset launcher to enter Claude's plan mode and submit a
//! prompt without relying on the `-p` CLI flag (which auto-executes). The
//! runner has two pieces of intelligence layered onto a mostly-dumb
//! sleep/write loop:
//!
//! 1. The **first** step, when it's a `Delay`, is treated as a startup-wait
//!    with PTY-output-quiescence-based early exit. We proceed as soon as
//!    the agent's TUI has finished its initial paint rather than always
//!    waiting the full ceiling, but only after we've seen enough output to
//!    be confident the TUI has actually started painting (Claude's banner
//!    is ~2 KB; sub-1 KB means it hasn't started yet, so quiescence is
//!    meaningless).
//! 2. If the injector declares a `verify_mode_marker`, the runner scans
//!    PTY output for that marker after the pre-input steps and re-sends
//!    them up to a few times if the marker doesn't appear. This self-heals
//!    the common Plan-Mode case where `Shift+Tab` bytes get dropped
//!    because Claude wasn't yet listening on its keystroke handler.
//!
//! Subsequent delays in the script (typically inter-keystroke pacing) are
//! literal sleeps.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use protocol::{InjectorStep, PromptInjector};
use tokio::sync::broadcast;
use tokio::time::{Instant, sleep, timeout};
use tracing::{debug, info, warn};

use crate::pty::PtyHandle;

/// Minimum wait before declaring the TUI ready, even if no output has been
/// observed. Raised from the original 1500 ms after observing that under
/// burst spawn load (9-prompt preset) Claude's banner-paint sometimes has
/// silence gaps in the 500–1500 ms range that would otherwise trip early
/// exit on a TUI whose keystroke handler isn't yet bound.
const STARTUP_MIN_WAIT: Duration = Duration::from_secs(3);

/// Quiescence threshold: how long the PTY output stream must be silent
/// before we declare the TUI "ready for input". Raised from the original
/// 500 ms to demand sustained silence — Claude's banner can stream in
/// bursts with sub-second pauses that fooled the old threshold.
const STARTUP_QUIET_FOR: Duration = Duration::from_millis(1500);

/// Minimum cumulative PTY output bytes seen before quiescence is allowed
/// to fire. Claude's startup banner is ~2 KB; if we've seen less than 1 KB
/// and the stream goes quiet, it almost certainly means Claude hasn't
/// started painting yet (not that it's finished). Without this gate the
/// quiescence heuristic exits before the input handler is bound and the
/// pre-input keystrokes are dropped.
const STARTUP_MIN_OUTPUT_BYTES: usize = 1024;

/// Lower bound on the caller-provided startup cap. The cap is a safety
/// ceiling, not a target — the early-exit heuristic still wins on a
/// healthy spawn — so a generous floor here costs nothing on the happy
/// path but protects burst launches whose preset declares a tight cap
/// (the bundled `smoke-inline` preset historically used 6000 ms, which
/// is too tight under load).
const MIN_STARTUP_CAP: Duration = Duration::from_secs(15);

/// How long to wait for `verify_mode_marker` to appear in output before
/// declaring this attempt a miss and re-sending `pre_input`.
const VERIFY_WINDOW: Duration = Duration::from_secs(2);

/// Total attempts to wait for `verify_mode_marker` (1 initial + N-1
/// retries). After this we log a warning and proceed with the prompt
/// anyway — best-effort matches the existing inject contract.
const VERIFY_MAX_ATTEMPTS: u32 = 3;

/// How much PTY output (raw bytes, pre-strip) to keep buffered when
/// scanning for `verify_mode_marker`. Sized generously above Claude's
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
pub fn run(session_id: String, pty: Arc<PtyHandle>, injector: PromptInjector) {
    // Subscribe BEFORE spawning the runner task so the receiver is hooked
    // up synchronously with the caller's view of the PTY — otherwise a
    // fast-booting child could emit its banner before the spawned task
    // subscribes and we'd miss the startup output we want to track
    // quiescence on.
    let output_rx = pty.output.subscribe();
    tokio::spawn(async move {
        let mut output_rx = output_rx;
        let steps = injector.steps;
        let verify_marker = injector.verify_mode_marker;

        debug!(
            session_id = %session_id,
            steps = steps.len(),
            verify_marker = ?verify_marker.as_deref(),
            "injector starting"
        );

        // Phase 1: startup wait — consume leading Delay as a quiescence-
        // gated wait, not a literal sleep.
        let mut cursor = 0;
        if let Some(InjectorStep::Delay { ms }) = steps.first() {
            let provided_cap = Duration::from_millis(u64::from(*ms));
            let cap = provided_cap.max(MIN_STARTUP_CAP);
            let waited = wait_until_ready_or_timeout(&mut output_rx, cap).await;
            info!(
                session_id = %session_id,
                provided_cap_ms = u64::from(*ms),
                effective_cap_ms = u64::try_from(cap.as_millis()).unwrap_or(u64::MAX),
                waited_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
                "injector startup wait done"
            );
            cursor = 1;
        }

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
            execute_step(&session_id, &pty, cursor + offset, step).await;
        }

        // Phase 3: verify the mode marker appeared, retry if not.
        // Only meaningful when there's an actual prompt boundary AND a
        // marker — verifying without a prompt boundary would re-send the
        // entire script.
        if let (Some(marker), Some(_)) = (verify_marker.as_deref(), prompt_idx) {
            verify_and_retry(
                &session_id,
                &pty,
                &mut output_rx,
                marker,
                &steps[cursor..pre_input_end],
                cursor,
            )
            .await;
        }

        // Phase 4: prompt + post_input
        for (offset, step) in steps[pre_input_end..].iter().enumerate() {
            execute_step(&session_id, &pty, pre_input_end + offset, step).await;
        }

        debug!(session_id = %session_id, "injector finished");
    });
}

async fn execute_step(session_id: &str, pty: &Arc<PtyHandle>, idx: usize, step: &InjectorStep) {
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
                    pty.write_input(bytes);
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
            pty.write_input(bytes);
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
    pty: &Arc<PtyHandle>,
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
                    execute_step(session_id, pty, pre_input_start_idx + offset, step).await;
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
                // waiting until the window closes — Claude's footer is
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
/// Intentionally duplicated from [`pty_state::strip_ansi`] — small,
/// self-contained, and only used here; factoring into a shared module
/// would be premature for one caller.
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

/// Wait for the PTY output stream to be quiet for [`STARTUP_QUIET_FOR`]
/// continuously, with [`STARTUP_MIN_WAIT`] as a floor, at least
/// [`STARTUP_MIN_OUTPUT_BYTES`] bytes observed, and the caller's `cap` as
/// a ceiling. Returns the actual time waited.
async fn wait_until_ready_or_timeout(
    output: &mut broadcast::Receiver<Vec<u8>>,
    cap: Duration,
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
        let need_floor = waited < STARTUP_MIN_WAIT;
        let need_quiet = quiet_for < STARTUP_QUIET_FOR;
        let need_bytes = total_bytes < STARTUP_MIN_OUTPUT_BYTES;
        if !need_floor && !need_quiet && !need_bytes {
            return waited;
        }
        // Sleep until the next threshold could plausibly change. When the
        // only thing blocking us is the byte gate, we have no timer-based
        // signal — only an output chunk or the cap will move us — so we
        // wait until the cap (avoiding a tight loop with `wait_for == 0`).
        let mut next_wake = deadline;
        if need_floor {
            next_wake = next_wake.min(started + STARTUP_MIN_WAIT);
        }
        if need_quiet {
            next_wake = next_wake.min(last_activity + STARTUP_QUIET_FOR);
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;

    #[tokio::test]
    async fn sufficient_output_then_quiescence_exits_early() {
        // 1500 bytes (above MIN_OUTPUT_BYTES) arrive immediately, then
        // silence. We should exit after STARTUP_MIN_WAIT (the floor),
        // well before the cap.
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(16);
        assert!(tx.send(vec![b'.'; 1500]).is_ok());
        let cap = Duration::from_secs(20);
        let started = Instant::now();
        let waited = wait_until_ready_or_timeout(&mut rx, cap).await;
        let elapsed = started.elapsed();
        assert!(
            waited >= STARTUP_MIN_WAIT,
            "should wait at least the floor; waited {waited:?}"
        );
        assert!(
            elapsed < cap,
            "should exit well before cap; elapsed {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn min_output_bytes_blocks_early_exit() {
        // Only 200 bytes seen (below MIN_OUTPUT_BYTES). Without this
        // gate, the silence + floor would trigger early exit; with it,
        // we must hit the cap. Cap chosen low so the test is fast.
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(16);
        assert!(tx.send(vec![b'.'; 200]).is_ok());
        let cap = Duration::from_millis(500);
        let started = Instant::now();
        let waited = wait_until_ready_or_timeout(&mut rx, cap).await;
        let elapsed = started.elapsed();
        // With a sub-floor cap and insufficient bytes, we should hit the
        // cap. Tolerate some scheduler jitter on either side.
        assert!(
            waited >= cap.saturating_sub(Duration::from_millis(100)),
            "should hit cap when output is below MIN_OUTPUT_BYTES; waited {waited:?}"
        );
        assert!(
            elapsed < cap + Duration::from_millis(300),
            "shouldn't overrun cap by much; elapsed {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn no_output_hits_cap() {
        // No output at all: total_bytes stays at 0 forever, MIN_OUTPUT_BYTES
        // gate blocks quiescence exit, we hit the cap.
        let (_tx, mut rx) = broadcast::channel::<Vec<u8>>(16);
        let cap = Duration::from_millis(500);
        let started = Instant::now();
        let waited = wait_until_ready_or_timeout(&mut rx, cap).await;
        let elapsed = started.elapsed();
        assert!(
            waited >= cap.saturating_sub(Duration::from_millis(100)),
            "should hit cap with no output; waited {waited:?}"
        );
        assert!(
            elapsed < cap + Duration::from_millis(300),
            "shouldn't overrun cap by much; elapsed {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn cap_hit_when_output_never_settles() {
        // Continuous output keeps refreshing last_activity, so we should
        // hit the ceiling instead of declaring ready.
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
        let started = Instant::now();
        let waited = wait_until_ready_or_timeout(&mut rx, cap).await;
        let elapsed_under_test = started.elapsed();
        producer.abort();
        assert!(
            waited >= cap.saturating_sub(Duration::from_millis(100)),
            "should hit cap when output never settles; waited {waited:?}"
        );
        assert!(
            elapsed_under_test < cap + Duration::from_millis(300),
            "shouldn't overrun cap by much; elapsed {elapsed_under_test:?}"
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

    #[tokio::test]
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

    #[tokio::test]
    async fn marker_absent_times_out() {
        // Stream emits noise but never the marker; expect Timeout within
        // VERIFY_WINDOW (plus a little jitter).
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
            elapsed < VERIFY_WINDOW + Duration::from_millis(300),
            "shouldn't overrun window by much; elapsed {elapsed:?}"
        );
    }

    #[test]
    fn strip_ansi_drops_csi_osc_and_keeps_text() {
        let raw = b"\x1b[2J\x1b[?25hhello \x1b]0;title\x07world\r";
        assert_eq!(strip_ansi(raw), "hello world");
    }
}
