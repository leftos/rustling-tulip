//! The daemon's `shutdown` subcommand: retire a running daemon and exit.
//!
//! `rustling-tulipd shutdown` is what the installer and the uninstaller run
//! before they replace or delete the installed files. It retires the daemon the
//! way a client does — the listener proves it holds the token before the bearer
//! `/shutdown` is sent on that proven connection — and leaves every
//! `rt-tracer.exe` alive, so the next daemon reattaches the user's sessions.

use daemon_client::Shutdown;
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

/// The whole stop — reading the handshake, the proof, the shutdown request and
/// the wait for the daemon to exit — must finish inside this, or the subcommand
/// gives up, so an installer never hangs on it.
const STOP_TIMEOUT: Duration = Duration::from_secs(25);

/// The exit code for a listener that did not prove it holds the token: nothing
/// was sent and nothing was killed.
const NOT_PROVEN_CODE: u8 = 3;

/// True when the daemon's arguments ask for `shutdown`: the first argument is
/// exactly `shutdown`.
pub fn requested(args: impl IntoIterator<Item = OsString>) -> bool {
    args.into_iter().next().is_some_and(|arg| arg == "shutdown")
}

/// Retire the daemon the config dir's `daemon.json` names, writing one line
/// about what happened to `out`, and return the exit code for it: `0` when a
/// daemon stopped, none was running or a stale `daemon.json` was removed, `3`
/// when its listener did not prove it holds the token, and `1` when the stop
/// failed.
///
/// # Errors
///
/// Fails only when the line cannot be written.
pub async fn run(config_dir: &Path, out: &mut impl Write) -> anyhow::Result<u8> {
    match tokio::time::timeout(STOP_TIMEOUT, daemon_client::shutdown_in(config_dir)).await {
        Err(_) => {
            let secs = STOP_TIMEOUT.as_secs();
            writeln!(out, "could not stop the daemon within {secs} s")?;
            Ok(1)
        }
        Ok(Err(err)) => {
            writeln!(out, "could not stop the daemon: {err:#}")?;
            Ok(1)
        }
        Ok(Ok(Shutdown::NoDaemon)) => {
            writeln!(out, "no daemon is running")?;
            Ok(0)
        }
        Ok(Ok(Shutdown::Stale)) => {
            writeln!(out, "no daemon was running; removed a stale daemon.json")?;
            Ok(0)
        }
        Ok(Ok(Shutdown::Stopped)) => {
            writeln!(out, "stopped the daemon")?;
            Ok(0)
        }
        Ok(Ok(Shutdown::NotProven(_))) => {
            writeln!(
                out,
                "the daemon's listener did not prove it holds its token; nothing was sent"
            )?;
            Ok(NOT_PROVEN_CODE)
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
    use crate::server::test_support::ScratchDir;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn shutdown_is_requested_only_as_the_first_argument() {
        assert!(requested(args(&["shutdown"])));
        let empty: &[&str] = &[];
        for list in [empty, &["pair"], &["--detach", "shutdown"], &["shutdowns"]] {
            assert!(!requested(args(list)), "{list:?}");
        }
    }

    #[tokio::test]
    async fn run_prints_one_line_and_exits_0_with_no_daemon() {
        let dir = ScratchDir::new("cli-shutdown-no-daemon");
        let mut out: Vec<u8> = Vec::new();

        let code = run(dir.path(), &mut out)
            .await
            .expect("the line is written");

        assert_eq!(code, 0);
        assert_eq!(
            String::from_utf8(out).expect("the line is UTF-8"),
            "no daemon is running\n"
        );
    }
}
