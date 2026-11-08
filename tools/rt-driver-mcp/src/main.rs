//! `rt-driver-mcp`: a stdio MCP server that launches isolated, cloaked,
//! driven native clients and calls their driver endpoints.
//!
//! stdout carries only JSON-RPC frames; logs go to stderr and to
//! `<repo root>/.tmp/driver/rt-driver-mcp-<pid>.log`. When stdin closes,
//! every live session is stopped before the server exits.
//!
//! `rt-driver-mcp set-token` instead stores the one line on stdin as the
//! driver token (`token.rs`) and exits.

mod launch;
mod pipe;
mod rpc;
mod session;
mod token;
mod tools;

use std::ffi::OsString;
use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;

use anyhow::{Context as _, Result, bail};
use tracing_subscriber::fmt::writer::MakeWriterExt as _;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let _ = writeln!(std::io::stderr(), "rt-driver-mcp: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1).peekable();
    if args.peek().is_some_and(|arg| arg == "set-token") {
        if let Some(extra) = args.nth(1) {
            bail!(
                "set-token takes no arguments, not {}; it reads the token from stdin",
                extra.display()
            );
        }
        return set_token();
    }
    let repo_root = repo_root_arg(args)?;
    init_logging(&repo_root);
    tracing::info!(version = %rpc::server_version(), repo_root = %repo_root.display(), "rt-driver-mcp started");
    let mut server = rpc::Server::new(repo_root);
    let served = serve(&mut server);
    server.stop_all();
    tracing::info!("stdin closed; every session stopped");
    served
}

/// `set-token`: stores the one line on stdin as the driver token, and prints
/// only that it did. The owner runs it through
/// `tools/rt-driver-mcp/set-driver-token.ps1`.
fn set_token() -> Result<()> {
    token::set_token(&mut std::io::stdin().lock(), &token::CredentialManager)
        .map_err(anyhow::Error::msg)?;
    writeln!(std::io::stdout(), "driver token stored").context("writing stdout")?;
    Ok(())
}

/// Answers each stdin line until stdin closes.
fn serve(server: &mut rpc::Server) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = line.context("reading stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = server.handle_line(&line) {
            let mut text = serde_json::to_string(&response).context("encoding a response")?;
            text.push('\n');
            stdout
                .write_all(text.as_bytes())
                .context("writing stdout")?;
            stdout.flush().context("flushing stdout")?;
        }
    }
    Ok(())
}

/// `--repo-root <path>`, else the current directory.
fn repo_root_arg(mut args: impl Iterator<Item = OsString>) -> Result<PathBuf> {
    let mut root = None;
    while let Some(arg) = args.next() {
        if arg != "--repo-root" {
            bail!(
                "unknown argument {}; usage: rt-driver-mcp [--repo-root <path>] | rt-driver-mcp set-token",
                arg.display()
            );
        }
        let Some(path) = args.next() else {
            bail!("--repo-root needs a path");
        };
        root = Some(PathBuf::from(path));
    }
    match root {
        Some(root) => Ok(root),
        None => std::env::current_dir().context("reading the current directory"),
    }
}

/// Logs to stderr, and to the server's log file when it can be made.
fn init_logging(repo_root: &std::path::Path) {
    let dir = repo_root.join(".tmp").join("driver");
    let path = dir.join(format!("rt-driver-mcp-{}.log", std::process::id()));
    let file = std::fs::create_dir_all(&dir).and_then(|()| std::fs::File::create(&path));
    let builder = tracing_subscriber::fmt().with_ansi(false);
    match file {
        Ok(file) => builder
            .with_writer(std::io::stderr.and(Mutex::new(file)))
            .init(),
        Err(err) => {
            builder.with_writer(std::io::stderr).init();
            tracing::warn!(path = %path.display(), "no log file: {err}");
        }
    }
}
