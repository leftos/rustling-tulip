//! The daemon's `shutdown` subcommand: retire a running daemon and exit.
//!
//! `rustling-tulipd shutdown` is what the installer and the uninstaller run
//! before they replace or delete the installed files. It retires the daemon the
//! way a client does — the listener proves it holds the token before the bearer
//! `/shutdown` is sent on that proven connection — and leaves every
//! `rt-tracer.exe` alive, so the next daemon reattaches the user's sessions.
//! The caller's integrity decides which daemon it reaches, and it never stops
//! one of another integrity.

use daemon_client::profile::Profile;
use daemon_client::{LocatedDaemon, Shutdown};
use std::ffi::OsString;
use std::io::Write;
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

/// Retire the daemon `located` names, writing one line about what happened, and
/// return the exit code for it: `0` when a daemon stopped, none was running or
/// a stale `daemon.json` was removed, `3` when its listener did not prove it
/// holds the token, and `1` when the stop failed or `caller`'s profile may not
/// stop that daemon. A success goes to `out`, a failure (exit `1` or `3`) to
/// `err`.
///
/// `caller` is this process's profile: an elevated caller is refused a daemon
/// whose process runs ordinary, or a running one whose integrity cannot be
/// read ([`daemon_client::refuse_stop_across_integrity`]), since supervision
/// never crosses integrity. A handshake naming a pid no process has is stale:
/// nothing runs to stop across integrity, and its file is removed.
///
/// # Errors
///
/// Fails only when the line cannot be written.
pub async fn run(
    located: &LocatedDaemon,
    caller: Profile,
    out: &mut impl Write,
    err: &mut impl Write,
) -> anyhow::Result<u8> {
    if let Err(refusal) = daemon_client::refuse_stop_across_integrity(caller, located) {
        writeln!(err, "could not stop the daemon: {refusal}")?;
        return Ok(1);
    }
    let config_dir = &located.config_dir;
    match tokio::time::timeout(STOP_TIMEOUT, daemon_client::shutdown_in(config_dir)).await {
        Err(_) => {
            let secs = STOP_TIMEOUT.as_secs();
            writeln!(err, "could not stop the daemon within {secs} s")?;
            Ok(1)
        }
        Ok(Err(failure)) => {
            writeln!(err, "could not stop the daemon: {failure:#}")?;
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
                err,
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
    use std::path::Path;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn located_ordinary(dir: &Path) -> LocatedDaemon {
        LocatedDaemon {
            profile: Profile::Ordinary,
            config_dir: dir.to_path_buf(),
        }
    }

    /// A `daemon.json` in `dir` naming `pid`, which no process has.
    fn write_handshake(dir: &Path, pid: u32) {
        let json = format!(
            r#"{{"protocol_version":1,"port":1,"auth_token":"test-token","pid":{pid},"supported_versions":[1],"build_version":""}}"#
        );
        std::fs::write(daemon_client::handshake_file_in(dir), json)
            .expect("write the handshake file");
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
        let mut err: Vec<u8> = Vec::new();

        let code = run(
            &located_ordinary(dir.path()),
            Profile::Ordinary,
            &mut out,
            &mut err,
        )
        .await
        .expect("the line is written");

        assert_eq!(code, 0);
        assert!(err.is_empty(), "nothing failed");
        assert_eq!(
            String::from_utf8(out).expect("the line is UTF-8"),
            "no daemon is running\n"
        );
    }

    /// A harmless Windows process that stays alive for half a minute unless
    /// killed: a running ordinary process for a handshake to name.
    #[cfg(windows)]
    fn stand_in() -> std::process::Child {
        std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn a stand-in process")
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shutdown_removes_a_dead_elevated_daemons_handshake_for_an_elevated_caller() {
        let dir = ScratchDir::new("cli-shutdown-dead-elevated-daemon");
        write_handshake(dir.path(), u32::MAX);
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let code = run(
            &located_ordinary(dir.path()),
            Profile::Elevated,
            &mut out,
            &mut err,
        )
        .await
        .expect("the line is written");

        assert_eq!(code, 0);
        assert!(err.is_empty(), "nothing failed");
        assert_eq!(
            String::from_utf8(out).expect("the line is UTF-8"),
            "no daemon was running; removed a stale daemon.json\n"
        );
        assert!(
            !daemon_client::handshake_file_in(dir.path()).exists(),
            "the stale handshake is gone"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shutdown_refuses_a_running_ordinary_daemon_for_an_elevated_caller() {
        let dir = ScratchDir::new("cli-shutdown-running-ordinary-daemon");
        let mut child = stand_in();
        write_handshake(dir.path(), child.id());
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let code = run(
            &located_ordinary(dir.path()),
            Profile::Elevated,
            &mut out,
            &mut err,
        )
        .await
        .expect("the line is written");
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(code, 1);
        assert!(out.is_empty(), "nothing succeeded");
        assert_eq!(
            String::from_utf8(err).expect("the line is UTF-8"),
            "could not stop the daemon: the running rustling-tulip daemon runs without \
             administrator rights; stop it from an ordinary terminal with \"rustling-tulipd \
             shutdown\"\n"
        );
        assert!(
            daemon_client::handshake_file_in(dir.path()).exists(),
            "the handshake is kept"
        );
    }
}
