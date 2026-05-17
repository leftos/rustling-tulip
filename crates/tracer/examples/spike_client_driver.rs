//! Driver for the server-side spike. Connects, writes "phase-1", reads
//! ack, disconnects. Sleeps. Connects again, writes "phase-2", reads ack.
//! Simulates a daemon restart from the client side.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::ClientOptions;

const PIPE_NAME: &str = r"\\.\pipe\rt-tracer-spike-c1";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();
    // Give the server time to start.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut c1 = ClientOptions::new().open(PIPE_NAME)?;
    c1.write_all(b"phase-1-hello\n").await?;
    c1.flush().await?;
    let mut buf = vec![0u8; 64];
    let n = c1.read(&mut buf).await?;
    tracing::info!(bytes = n, ack = %String::from_utf8_lossy(&buf[..n]).trim(), "phase-1 ack");
    drop(c1);

    // Mimic daemon restart: pause, then reconnect.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let mut c2 = ClientOptions::new().open(PIPE_NAME)?;
    c2.write_all(b"phase-2-hello\n").await?;
    c2.flush().await?;
    let n = c2.read(&mut buf).await?;
    tracing::info!(bytes = n, ack = %String::from_utf8_lossy(&buf[..n]).trim(), "phase-2 ack");

    tracing::info!("client driver: both phases ok");
    Ok(())
}
