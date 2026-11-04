//! The daemon's log filter, installed once at start and reloadable at runtime
//! by the Detailed logging switch.

use std::sync::OnceLock;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::Registry;
use tracing_subscriber::reload;

/// The filter a daemon logs with unless the Detailed logging switch is on:
/// everything at info, plus the daemon crate's own debug lines, which
/// `daemon.log` has always carried.
pub const DEFAULT_FILTER: &str = "info,daemon=debug,tower_http=info";

/// The filter the Detailed logging switch selects: the default filter plus
/// the `status_trace` decision lines the switch exists for. Every other
/// dependency's debug output stays out, so the 5 MiB log keeps its room for
/// those lines instead of rolling over minutes after the switch is turned on.
pub const DETAILED_FILTER: &str = "info,daemon=debug,tower_http=info,status_trace=debug";

/// The running filter's handle. Absent when `RUST_LOG` set the filter at
/// start: that filter is the user's, and the switch never replaces it.
static HANDLE: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

/// Whether `RUST_LOG` set the filter at start, recorded where the environment
/// is read so a later call cannot mistake a process without tracing for one.
static RUST_LOG_SET: OnceLock<bool> = OnceLock::new();

/// What a [`set_detailed`] call did, so the caller can log it exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The filter was reloaded to the requested position.
    Applied,
    /// `RUST_LOG` set the filter at start: the choice is saved and the filter
    /// stays as the environment made it.
    RustLogSet,
    /// No filter handle was installed (a test process), or the one that was
    /// installed is gone.
    NoHandle,
}

/// The filter layer `init_tracing` builds its subscriber around. `RUST_LOG`
/// wins when it is set: that filter is installed and no handle is kept, so
/// the switch cannot reload it.
pub fn filter_layer() -> reload::Layer<EnvFilter, Registry> {
    let (filter, rust_log_set) = match EnvFilter::try_from_default_env() {
        Ok(filter) => (filter, true),
        Err(_) => (EnvFilter::new(DEFAULT_FILTER), false),
    };
    let _ = RUST_LOG_SET.set(rust_log_set);
    let (layer, handle) = reload::Layer::new(filter);
    if !rust_log_set {
        let _ = HANDLE.set(handle);
    }
    layer
}

/// The filter for one switch position.
fn filter_for(enabled: bool) -> &'static str {
    if enabled {
        DETAILED_FILTER
    } else {
        DEFAULT_FILTER
    }
}

/// Reload the log filter to match the switch and say what happened, leaving
/// the line to the caller so one switch is logged once. A daemon started with
/// `RUST_LOG` keeps that filter.
pub fn set_detailed(enabled: bool) -> Outcome {
    let Some(handle) = HANDLE.get() else {
        return if RUST_LOG_SET.get().copied().unwrap_or_default() {
            Outcome::RustLogSet
        } else {
            Outcome::NoHandle
        };
    };
    match handle.reload(EnvFilter::new(filter_for(enabled))) {
        Ok(()) => Outcome::Applied,
        Err(err) => {
            tracing::warn!(
                ?err,
                "daemon log filter reload failed; the saved switch is not in effect"
            );
            Outcome::NoHandle
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    /// Captures what a subscriber writes, so a test can assert which lines a
    /// filter lets through.
    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
            let bytes = self.0.lock().expect("the log buffer is not poisoned");
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    impl std::io::Write for Capture {
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

    impl tracing_subscriber::fmt::MakeWriter<'_> for Capture {
        type Writer = Self;

        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn filter_for_picks_the_switch_position() {
        assert_eq!(filter_for(false), DEFAULT_FILTER);
        assert_eq!(filter_for(true), DETAILED_FILTER);
        assert!(
            DETAILED_FILTER.starts_with(DEFAULT_FILTER),
            "the detailed filter is the default one plus the decision lines: {DETAILED_FILTER}"
        );
        assert!(
            DETAILED_FILTER.contains("status_trace=debug"),
            "the detailed filter lets the status-decision lines through: {DETAILED_FILTER}"
        );
    }

    #[test]
    fn a_status_trace_debug_line_shows_only_when_detailed() {
        let capture = Capture::default();
        // The test builds its own handle over the same reload layer a daemon
        // uses, so the process-wide one is never touched.
        let (layer, handle) = reload::Layer::new(EnvFilter::new(filter_for(false)));
        let subscriber = tracing_subscriber::registry().with(layer).with(
            tracing_subscriber::fmt::layer()
                .with_writer(capture.clone())
                .with_ansi(false)
                .with_target(true),
        );

        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "status_trace", "a decision the default filter hides");
            assert!(
                !capture
                    .text()
                    .contains("a decision the default filter hides"),
                "the default filter leaves a status_trace debug line out"
            );

            handle
                .reload(EnvFilter::new(filter_for(true)))
                .expect("reload to the detailed filter");
            tracing::debug!(target: "status_trace", "a decision the detailed filter keeps");
            assert!(
                capture
                    .text()
                    .contains("a decision the detailed filter keeps"),
                "the detailed filter lets a status_trace debug line through"
            );

            tracing::debug!(target: "hyper", "a dependency's debug chatter");
            assert!(
                !capture.text().contains("a dependency's debug chatter"),
                "the detailed filter leaves a dependency's debug line out, so the log keeps its room"
            );
        });
    }
}
