//! Single-instance pipe server for the race test. Accepts the first
//! connection, holds it for 2 seconds (during which the second client
//! tries and fails). Lets us observe exactly what error the loser sees.

use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};

const PIPE_NAME: &str = r"\\.\pipe\rt-tracer-spike-c1";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();

    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .pipe_mode(PipeMode::Byte)
        .max_instances(1)
        .create(PIPE_NAME)?;
    tracing::info!("race server: listening (max_instances=1)");

    server.connect().await?;
    tracing::info!("race server: first client connected; holding for 2s");

    let (_reader, mut writer) = tokio::io::split(server);
    writer.write_all(b"first-wins\n").await?;
    writer.flush().await?;

    // Hold the connection so the second client has to face an active
    // first connection.
    tokio::time::sleep(Duration::from_secs(2)).await;
    tracing::info!("race server: exiting");
    Ok(())
}
