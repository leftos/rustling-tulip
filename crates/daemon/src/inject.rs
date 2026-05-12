//! Scripted PTY input runner — drives a [`protocol::PromptInjector`] against
//! a live [`PtyHandle`] after spawn.
//!
//! Used by the preset launcher to enter Claude's plan mode and submit a
//! prompt without relying on the `-p` CLI flag (which auto-executes). The
//! runner is mostly dumb: it sleeps, writes bytes, repeats. The one bit of
//! intelligence is for the **first** step when it's a `Delay`: that's
//! treated as a startup-wait with PTY-output-quiescence-based early exit,
//! so we proceed as soon as the agent's TUI has stopped emitting output
//! rather than always waiting the full ceiling. Subsequent delays in the
//! script (typically inter-keystroke pacing) remain literal sleeps.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use protocol::{InjectorStep, PromptInjector};
use tokio::sync::broadcast;
use tokio::time::{Instant, sleep, timeout};
use tracing::{debug, info, warn};

use crate::pty::PtyHandle;

/// Minimum wait before declaring the TUI ready, even if no output has been
/// observed. Some terminals don't emit anything for the first split-second
/// after spawn; bailing out too early would race the agent's initial paint.
const STARTUP_MIN_WAIT: Duration = Duration::from_millis(1500);

/// Quiescence threshold: how long the PTY output stream must be silent
/// before we declare the TUI "ready for input". Tuned for Claude Code's
/// startup, which streams a banner + size probe and then settles.
const STARTUP_QUIET_FOR: Duration = Duration::from_millis(500);

/// Spawn a background task that walks `injector.steps` in order. Returns
/// immediately. If the PTY is dropped or the child exits mid-script the task
/// will keep writing into a closed channel — the writes are silently
/// discarded by `PtyHandle::write_input`, so we just log and move on.
pub fn run(session_id: String, pty: Arc<PtyHandle>, injector: PromptInjector) {
    // Subscribe BEFORE spawning the runner task so the receiver is hooked up
    // synchronously with the caller's view of the PTY — otherwise a fast-
    // booting child could emit its banner before the spawned task subscribes
    // and we'd miss the startup output we want to track quiescence on.
    let output_rx = pty.output.subscribe();
    tokio::spawn(async move {
        let mut output_rx = output_rx;
        debug!(
            session_id = %session_id,
            steps = injector.steps.len(),
            "injector starting"
        );
        for (idx, step) in injector.steps.iter().enumerate() {
            match step {
                InjectorStep::Delay { ms } if idx == 0 => {
                    let cap = Duration::from_millis(u64::from(*ms));
                    let waited = wait_until_ready_or_timeout(&mut output_rx, cap).await;
                    info!(
                        session_id = %session_id,
                        step = idx,
                        cap_ms = u64::from(*ms),
                        waited_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
                        "injector startup wait done"
                    );
                }
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
            }
        }
        debug!(session_id = %session_id, "injector finished");
    });
}

/// Wait for the PTY output stream to be quiet for [`STARTUP_QUIET_FOR`]
/// continuously, with [`STARTUP_MIN_WAIT`] as a floor and the caller's
/// `cap` as a ceiling. Returns the actual time waited.
///
/// Rationale: the previous behaviour was a fixed `sleep(startup_delay_ms)`
/// — typically 6 s for Claude. Under burst spawn load (e.g. a 9-prompt
/// preset launch where the 9th spawn happens after 8 prior worktree adds
/// have warmed up the FS / antivirus / claude itself), Claude was not yet
/// painting its input prompt at 6 s and the injected Shift+Tab×4 + prompt
/// landed in startup-screen state, silently dropped. Output-quiescence is
/// a much better "is the TUI ready?" signal: as long as Claude is still
/// emitting bytes it isn't ready; once 500 ms of silence elapses it almost
/// certainly is. We still keep the ceiling so a stuck child can't block
/// the injector forever.
async fn wait_until_ready_or_timeout(
    output: &mut broadcast::Receiver<Vec<u8>>,
    cap: Duration,
) -> Duration {
    let started = Instant::now();
    let deadline = started + cap;
    let mut last_activity = started;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return now.saturating_duration_since(started);
        }
        let waited = now.saturating_duration_since(started);
        let quiet_for = now.saturating_duration_since(last_activity);
        if waited >= STARTUP_MIN_WAIT && quiet_for >= STARTUP_QUIET_FOR {
            return waited;
        }
        // Wake up either when we'd next cross the quiescence threshold or
        // when the ceiling fires, whichever comes first. `wait_for` is the
        // bounded time we'll block on the next output chunk.
        let next_quiet_check = last_activity + STARTUP_QUIET_FOR;
        let next_floor = started + STARTUP_MIN_WAIT;
        let next_wake = next_quiet_check.min(next_floor).min(deadline);
        let wait_for = next_wake.saturating_duration_since(now);
        match timeout(wait_for, output.recv()).await {
            Err(_) => {} // timer fired; loop iteration re-checks thresholds
            Ok(Ok(_bytes)) => {
                last_activity = Instant::now();
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
    async fn quiet_exit_after_floor_with_no_output() {
        // No output at all: we should wait at least STARTUP_MIN_WAIT and
        // then exit on quiescence, well before the cap.
        let (_tx, mut rx) = broadcast::channel::<Vec<u8>>(16);
        let cap = Duration::from_secs(10);
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
    async fn cap_hit_when_output_never_settles() {
        // Continuous output keeps refreshing last_activity, so we should
        // hit the ceiling instead of declaring ready.
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(16);
        let cap = Duration::from_millis(800);
        // Drive output past the cap so quiescence never wins. Aborted at
        // the end so the producer's wall-time doesn't dominate the test.
        let producer = tokio::spawn(async move {
            loop {
                if tx.send(vec![b'.']).is_err() {
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
}
