//! `folder-bench`: times fetching a folder from the builder by each transfer
//! candidate (per-file `FetchFile` over `rustling-tulipd bridge`, a tar stream
//! in base64 JSON-line frames or raw, `scp`, `sftp`, and `ssh` piping `tar`).
//! A development tool, never shipped; see `README.md`.

mod args;
mod cpu;
mod fixtures;
mod frame;
mod guard;
mod relay;
mod remote;
mod report;
mod run;
mod serve;

use std::io::Write as _;
use std::process::ExitCode;

const USAGE: &str = "usage: folder-bench make-fixtures <dir> [--scale small|full] \
| serve-tar <dir> --codec none|zstd1|zstd3 --frame base64|raw [--zstd <path>] \
| run <candidate> --shape <name> --remote-dir <dir on builder> --dest <local dir> \
[--in-flight N] [--scale small|full] [--timeout-s S] [--zstd <path>] \
| push <local folder-bench.exe> \
| relay -- <program> [args...]";

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&argv) {
        Ok(code) => code,
        Err(err) => {
            let _ = writeln!(std::io::stderr().lock(), "folder-bench: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(argv: &[String]) -> anyhow::Result<ExitCode> {
    let Some((command, rest)) = argv.split_first() else {
        anyhow::bail!("{USAGE}");
    };
    match command.as_str() {
        "make-fixtures" => fixtures::command(rest),
        "serve-tar" => serve::command(rest),
        "run" => run::command(rest),
        "push" => remote::push_command(rest),
        "relay" => relay::command(rest),
        "help" | "--help" | "-h" => {
            writeln!(std::io::stdout().lock(), "{USAGE}")?;
            Ok(ExitCode::SUCCESS)
        }
        other => anyhow::bail!("unknown command {other:?}; {USAGE}"),
    }
}
