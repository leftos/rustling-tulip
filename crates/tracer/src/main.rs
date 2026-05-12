//! `rt-tracer` — Phase C.2 skeleton.
//!
//! The intent is that the daemon spawns one of these per session instead of
//! spawning `claude` directly. The tracer owns the `ConPTY` master handle
//! and the `claude` child process. When the daemon dies, the tracer keeps
//! running and buffers child output to a ring; when a new daemon starts, it
//! reconnects to the same named pipe and the tracer replays the buffer.
//!
//! Status: skeleton only — implementation is intentionally stubbed because
//! the design doc lists five empirical questions (see `docs/tracer-abi.md`)
//! that must be answered before the architecture is locked in. The pieces
//! here exist so the daemon integration in C.3 has a stable surface to
//! target, not as a working binary.

use anyhow::Context as _;
use clap::Parser;
use tracing::info;

mod ring;
mod supervisor;

#[derive(Debug, Parser)]
#[command(
    name = "rt-tracer",
    version,
    about = "rustling-tulip per-session PTY supervisor"
)]
struct Cli {
    /// Session id used to derive the named-pipe path
    /// (`\\.\pipe\rt-tracer-<session-id>`).
    #[arg(long)]
    session_id: String,

    /// Working directory passed to the child.
    #[arg(long)]
    cwd: std::path::PathBuf,

    /// Initial PTY column count. Daemon may resize later via
    /// [`tracer_protocol::TracerRequest::Resize`].
    #[arg(long, default_value_t = 120)]
    cols: u16,

    /// Initial PTY row count.
    #[arg(long, default_value_t = 32)]
    rows: u16,

    /// Program to spawn followed by its arguments. The tracer just shells
    /// out — it doesn't know anything about claude/codex specifics; the
    /// daemon's spawn pipeline builds the argv.
    #[arg(trailing_var_arg = true, required = true)]
    program_and_args: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    info!(
        session_id = %cli.session_id,
        cwd = %cli.cwd.display(),
        cols = cli.cols,
        rows = cli.rows,
        argc = cli.program_and_args.len(),
        "rt-tracer starting"
    );

    let (program, args) = cli
        .program_and_args
        .split_first()
        .context("program_and_args must contain at least the program name")?;

    let pipe_name = tracer_protocol::pipe_name(&cli.session_id);

    let cfg = supervisor::Config {
        session_id: cli.session_id,
        pipe_name,
        program: program.clone(),
        args: args.to_vec(),
        cwd: cli.cwd,
        cols: cli.cols,
        rows: cli.rows,
    };

    supervisor::run(cfg).await
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_target(true)
        .with_ansi(false)
        .compact()
        .init();
}
