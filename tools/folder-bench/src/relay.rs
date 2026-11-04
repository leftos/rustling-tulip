//! `relay -- <command...>`: runs on the builder, runs the command with this
//! process's stdin, stdout and stderr handed straight to it, and at its exit
//! writes `{"type":"end","cpu_ms":<n>}` (the command's CPU) to stderr, as
//! `serve-tar --frame raw` does. The fetch candidates run the bridge under it
//! so the relay's CPU is counted.

use std::io::{self, Write as _};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context as _, ensure};

use crate::{cpu, frame};

/// The program and its arguments after the leading `--`.
pub fn split_command(rest: &[String]) -> anyhow::Result<(&str, &[String])> {
    ensure!(
        rest.len() >= 2 && rest[0] == "--",
        "usage: folder-bench relay -- <program> [args...]"
    );
    Ok((&rest[1], &rest[2..]))
}

pub fn command(rest: &[String]) -> anyhow::Result<ExitCode> {
    let (program, args) = split_command(rest)?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    let status = child
        .wait()
        .with_context(|| format!("waiting for {program}"))?;
    let cpu_ms = cpu::child_cpu_ms(&child);
    let mut err = io::stderr().lock();
    serde_json::to_writer(&mut err, &frame::Frame::End { cpu_ms })?;
    writeln!(err)?;
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_command_follows_a_leading_double_dash() {
        let args = strings(&["--", "tool.exe", "bridge", "--x"]);
        let (program, rest) = split_command(&args).expect("split");
        assert_eq!(program, "tool.exe");
        assert_eq!(rest, ["bridge", "--x"]);
        assert!(split_command(&strings(&["tool.exe"])).is_err());
        assert!(split_command(&strings(&["--"])).is_err());
    }
}
