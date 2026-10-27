//! `rt-tracer` — per-session PTY supervisor.
//!
//! The daemon spawns one of these for every interactive and plain-shell
//! session. The tracer owns the PTY master and the session's child process
//! (an agent CLI or a shell). When the daemon dies, the tracer keeps
//! running and buffers child output to a ring; when a new daemon starts, it
//! reconnects over the same local socket and the tracer replays the buffer.
//! The daemon/tracer wire contract lives in `tracer-protocol` (see
//! `docs/tracer-abi.md`).

use anyhow::Context as _;
use clap::Parser;
use tracing::info;

mod hook;
#[cfg(windows)]
mod job_object;
mod ring;
mod supervisor;

#[derive(Debug, Parser)]
#[command(
    name = "rt-tracer",
    version,
    about = "rustling-tulip per-session PTY supervisor"
)]
struct Cli {
    /// Session id used to derive the local-socket name when `--pipe-name`
    /// is omitted (see [`tracer_protocol::socket_name`]).
    #[arg(long)]
    session_id: String,

    /// Local-socket name supplied by the daemon. Defaults to the protocol's
    /// session-derived name when omitted.
    #[arg(long)]
    pipe_name: Option<String>,

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

    /// The agent a shell child hosts, as the daemon's JSON note. The tracer
    /// only writes it to its log, where the daemon reads it back when it
    /// rebuilds history; it must precede the program.
    #[arg(long)]
    agent_note: Option<String>,

    /// Program to spawn followed by its arguments. The tracer just shells
    /// out — it doesn't know anything about agent specifics; the
    /// daemon's spawn pipeline builds the argv.
    #[arg(trailing_var_arg = true, required = true)]
    program_and_args: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    // `rt-tracer hook` is the shim an agent session's injected settings run
    // (see `hook`). It is dispatched before anything else and before a tokio
    // runtime exists, so a hook event pays for neither, and its clock is taken
    // here, at the earliest point the process can take one.
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "hook") {
        hook::run_from_env(hook::unix_nanos());
        return Ok(());
    }

    // The multi-threaded, all-enabled runtime `#[tokio::main]` used to build
    // for the tracer itself.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build the tokio runtime")?;
    runtime.block_on(run_tracer())
}

async fn run_tracer() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();
    log_start(&cli);

    let (program, args) = cli
        .program_and_args
        .split_first()
        .context("program_and_args must contain at least the program name")?;

    let pipe_name = cli
        .pipe_name
        .unwrap_or_else(|| tracer_protocol::socket_name(&cli.session_id));

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

/// The log's starting line, then the agent note's line when there is one.
/// The daemon's tracer-log reader parses both.
fn log_start(cli: &Cli) {
    info!(
        session_id = %cli.session_id,
        cwd = %cli.cwd.display(),
        cols = cli.cols,
        rows = cli.rows,
        argc = cli.program_and_args.len(),
        "rt-tracer starting"
    );
    if let Some(note) = &cli.agent_note {
        info!(agent_note = %note, "rt-tracer agent note");
    }
}

fn init_tracing() {
    // If the daemon set `RUSTLING_TULIP_TRACER_LOG`, mirror tracing output
    // to that file (truncated on each start). Otherwise fall back to
    // stderr — useful in `cargo run -p tracer` / e2e harness contexts but
    // pointless under the daemon (which routes our stderr to NUL).
    let builder = tracing_subscriber::fmt()
        .with_target(true)
        .with_ansi(false)
        .compact();
    if let Some(path) = std::env::var_os("RUSTLING_TULIP_TRACER_LOG")
        && let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
    {
        builder.with_writer(std::sync::Mutex::new(file)).init();
    } else {
        builder.init();
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests unwrap parsed fixtures with expect for clear failure messages"
)]
mod tests {
    use super::{Cli, log_start};
    use clap::Parser;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    const NOTE: &str = r#"{"agent":"test-agent","options":{},"trusted":true,"conversation_id":null,"extra_dirs":["D:\\b c"]}"#;

    fn cli(args: &[&str]) -> Cli {
        let argv = ["rt-tracer", "--session-id", "s1", "--cwd", "D:\\a"];
        Cli::try_parse_from(argv.iter().chain(args)).expect("the args parse")
    }

    #[test]
    fn agent_note_is_optional_and_precedes_the_program() {
        let with_note = cli(&["--agent-note", NOTE, "pwsh.exe", "-NoExit"]);
        assert_eq!(with_note.agent_note.as_deref(), Some(NOTE));
        assert_eq!(with_note.program_and_args, ["pwsh.exe", "-NoExit"]);

        let without = cli(&["pwsh.exe", "-NoExit"]);
        assert_eq!(without.agent_note, None);
        assert_eq!(without.program_and_args, ["pwsh.exe", "-NoExit"]);

        let after_program = cli(&["pwsh.exe", "--agent-note", NOTE]);
        assert_eq!(
            after_program.agent_note, None,
            "a flag after the program is the program's"
        );
        assert_eq!(
            after_program.program_and_args,
            ["pwsh.exe", "--agent-note", NOTE]
        );
    }

    /// A log writer that keeps what the subscriber writes.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The lines [`log_start`] writes for `cli` under the tracer's format.
    fn start_lines(cli: &Cli) -> String {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_target(true)
            .with_ansi(false)
            .compact()
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || log_start(cli));
        let bytes = captured.0.lock().expect("capture lock").clone();
        String::from_utf8(bytes).expect("the log is UTF-8")
    }

    #[test]
    fn the_start_writes_the_agent_note_line() {
        let lines = start_lines(&cli(&["--agent-note", NOTE, "pwsh.exe"]));

        assert!(
            lines.contains("rt-tracer starting session_id=s1 "),
            "{lines}"
        );
        assert!(
            lines.contains(&format!("rt-tracer agent note agent_note={NOTE}\n")),
            "{lines}"
        );
    }

    #[test]
    fn a_start_without_a_note_writes_no_note_line() {
        let lines = start_lines(&cli(&["pwsh.exe"]));

        assert!(lines.contains("rt-tracer starting "), "{lines}");
        assert!(!lines.contains("agent note"), "{lines}");
    }
}
