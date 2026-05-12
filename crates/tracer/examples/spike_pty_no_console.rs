//! C.1 spike Question 2: verify `portable-pty`'s `ConPTY` backend works
//! from a console-subsystem rust binary AND from a no-console parent.
//!
//! This binary spawns `cmd.exe /c echo hello` via `portable-pty` and reads
//! its output. Run it directly to verify the standard case; the parent's
//! console flags shouldn't affect `ConPTY`, which allocates its own
//! pseudoconsole regardless.
//!
//! To test the no-console case more rigorously, spawn this binary itself
//! from a parent that uses `CREATE_NO_WINDOW`. The skeleton already
//! demonstrates the call shape in `supervisor.rs`.

use std::io::Read;
use std::time::Duration;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    tracing::info!("ConPTY opened successfully from rust binary");

    let mut cmd = CommandBuilder::new("cmd.exe");
    cmd.arg("/c");
    cmd.arg("echo hello-from-pty");
    let _child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut output = Vec::new();
    let mut buf = [0u8; 1024];
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if std::time::Instant::now() >= deadline {
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&buf[..n]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err.into()),
        }
        if String::from_utf8_lossy(&output).contains("hello-from-pty") {
            break;
        }
    }
    let captured = String::from_utf8_lossy(&output);
    tracing::info!(bytes = output.len(), "PTY output captured");
    if captured.contains("hello-from-pty") {
        tracing::info!("Q2: confirmed ConPTY works from this binary");
        Ok(())
    } else {
        tracing::error!(?captured, "Q2: did not see expected output");
        anyhow::bail!("expected output not seen")
    }
}
