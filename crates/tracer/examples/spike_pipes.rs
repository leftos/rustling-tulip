//! C.1 spike: Windows named-pipe behavior under daemon-restart and
//! daemon-race scenarios.
//!
//! This binary exercises three of the five spike questions in
//! `docs/plans/upgrade-survivable-sessions.md`:
//!   - Question 3: can a server-side named pipe survive a client (daemon)
//!     disconnect and accept a fresh connection on the same name?
//!   - Question 5: what happens when two clients race to connect to the
//!     same pipe?
//!   - Implicitly, question 2: does this all work from a regular console-
//!     subsystem rust binary (yes — this example IS one).
//!
//! Run via:
//!
//! ```text
//! cargo run --example spike_pipes -p tracer -- <subcmd>
//! ```
//!
//! Subcommands:
//!     server        Host a pipe; accept first connection, then accept
//!                   another after that one closes. Logs each phase.
//!     race-clients  Spawn two client tasks that race to connect to the
//!                   pipe. Reports which won and how the second failed.
//!
//! Findings get rolled into `docs/spikes/c1-tracer-spike.md`.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, PipeMode, ServerOptions};

const PIPE_NAME: &str = r"\\.\pipe\rt-tracer-spike-c1";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();
    let subcmd = std::env::args().nth(1).unwrap_or_default();
    match subcmd.as_str() {
        "server" => run_server().await,
        "race-clients" => run_race_clients().await,
        other => {
            tracing::error!(?other, "unknown subcmd; use 'server' or 'race-clients'");
            anyhow::bail!("unknown subcmd")
        }
    }
}

/// Hosts the pipe, accepts the first connection, reads "hello\n" from it,
/// echoes back, then waits for the client to disconnect. Spins up a fresh
/// server instance on the SAME pipe name and accepts a second connection.
/// This validates Question 3: tracer can keep serving across daemon
/// restarts on a stable pipe name.
async fn run_server() -> anyhow::Result<()> {
    // First server instance.
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .pipe_mode(PipeMode::Byte)
        .create(PIPE_NAME)?;
    tracing::info!(pipe = %PIPE_NAME, "phase-1 server listening (first instance)");

    server.connect().await?;
    tracing::info!("phase-1 client connected");

    let (mut reader, mut writer) = tokio::io::split(server);
    let mut buf = vec![0u8; 64];
    let n = reader.read(&mut buf).await?;
    tracing::info!(bytes = n, "phase-1 read from client");
    writer.write_all(b"phase-1-ack\n").await?;
    writer.flush().await?;
    drop(writer);
    drop(reader);
    tracing::info!("phase-1 server closed instance");

    // Brief pause to mimic "daemon died, new daemon starting".
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Fresh server instance on the SAME name. This is the key test —
    // can a successor process claim the name and serve a new connection?
    let server2 = ServerOptions::new()
        .pipe_mode(PipeMode::Byte)
        .create(PIPE_NAME)?;
    tracing::info!(pipe = %PIPE_NAME, "phase-2 server listening (second instance)");
    server2.connect().await?;
    tracing::info!("phase-2 client connected");

    let (mut reader, mut writer) = tokio::io::split(server2);
    let n = reader.read(&mut buf).await?;
    tracing::info!(bytes = n, "phase-2 read from client");
    writer.write_all(b"phase-2-ack\n").await?;
    writer.flush().await?;

    tracing::info!("server: both phases complete; success");
    Ok(())
}

/// Race two clients against the same pipe. The first should succeed,
/// the second should get a clear "already connected / pipe busy" error
/// rather than silently joining. Validates Question 5.
async fn run_race_clients() -> anyhow::Result<()> {
    // Give the server a moment to set up if started concurrently.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let h1 = tokio::spawn(client_attempt(1, b"hello-from-client-1"));
    let h2 = tokio::spawn(client_attempt(2, b"hello-from-client-2"));
    let r1 = h1.await?;
    let r2 = h2.await?;
    tracing::info!(client_1 = %describe(&r1), client_2 = %describe(&r2), "race results");
    Ok(())
}

async fn client_attempt(idx: u32, payload: &'static [u8]) -> anyhow::Result<String> {
    // Stagger slightly so we can observe ordering.
    if idx == 2 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let client = match ClientOptions::new().open(PIPE_NAME) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(idx, ?err, "client open failed");
            return Err(err.into());
        }
    };
    tracing::info!(idx, "client connected; writing");
    let (mut reader, mut writer) = tokio::io::split(client);
    writer.write_all(payload).await?;
    writer.flush().await?;
    let mut buf = vec![0u8; 64];
    let n = reader.read(&mut buf).await?;
    let reply = String::from_utf8_lossy(&buf[..n]).to_string();
    tracing::info!(idx, %reply, "client read ack");
    Ok(reply)
}

fn describe<T: std::fmt::Debug>(r: &anyhow::Result<T>) -> String {
    match r {
        Ok(v) => format!("ok({v:?})"),
        Err(e) => format!("err({e:?})"),
    }
}
