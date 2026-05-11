//! Scripted PTY input runner — drives a [`protocol::PromptInjector`] against
//! a live [`PtyHandle`] after spawn.
//!
//! Used by the preset launcher to enter Claude's plan mode and submit a
//! prompt without relying on the `-p` CLI flag (which auto-executes). The
//! runner is intentionally dumb: it sleeps, writes bytes, repeats. No
//! awareness of TUI state; the script is responsible for choosing delays
//! that give the TUI time to repaint.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use protocol::{InjectorStep, PromptInjector};
use tokio::time::sleep;
use tracing::{debug, warn};

use crate::pty::PtyHandle;

/// Spawn a background task that walks `injector.steps` in order. Returns
/// immediately. If the PTY is dropped or the child exits mid-script the task
/// will keep writing into a closed channel — the writes are silently
/// discarded by `PtyHandle::write_input`, so we just log and move on.
pub fn run(session_id: String, pty: Arc<PtyHandle>, injector: PromptInjector) {
    tokio::spawn(async move {
        debug!(
            session_id = %session_id,
            steps = injector.steps.len(),
            "injector starting"
        );
        for (idx, step) in injector.steps.iter().enumerate() {
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
            }
        }
        debug!(session_id = %session_id, "injector finished");
    });
}
