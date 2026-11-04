//! The builder: its host, the bench folders on it, the commands the harness
//! runs there, and `push`.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail, ensure};
use base64::Engine as _;

use crate::{args, guard};

pub const HOST: &str = "builder@leftos-buildbox";
/// Every file the bench puts on the builder lives under this folder.
pub const REMOTE_ROOT: &str = r"C:\Users\builder\.tmp\folder-bench";
pub const REMOTE_BIN: &str = r"C:\Users\builder\.tmp\folder-bench\bin";
/// The wrapper `push` writes beside the daemon: it sets the isolation
/// variables, then runs `rustling-tulipd.exe` with its arguments.
pub const DAEMON_WRAPPER: &str = "rtd.cmd";
/// The wrapper that sets the isolation variables, then runs its arguments as
/// a command line (the timed fetch session runs `relay` under it, so the
/// relay's child is the bridge itself, not a `cmd`).
pub const ISOLATED_WRAPPER: &str = "isolated.cmd";
/// The system bsdtar (Git's GNU tar is first on PATH on the PC).
pub const TAR: &str = r"C:\Windows\System32\tar.exe";
/// The zstd CLI on the builder, `serve-tar`'s default `--zstd`.
pub const REMOTE_ZSTD: &str = r"C:\Tools\bin\zstd.exe";
/// The zstd CLI on the PC (a PATH lookup), `run`'s default `--zstd`.
pub const LOCAL_ZSTD: &str = "zstd.exe";
const OPENSSH: &str = r"C:\Windows\System32\OpenSSH";
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// The Windows `OpenSSH` client tool `name` (`ssh`, `scp`, `sftp`).
pub fn tool(name: &str) -> PathBuf {
    Path::new(OPENSSH).join(format!("{name}.exe"))
}

/// `ssh` to the builder running `remote_command` in its default shell.
pub fn ssh(remote_command: &str) -> Command {
    let mut cmd = Command::new(tool("ssh"));
    cmd.args(["-o", "BatchMode=yes", HOST, remote_command]);
    cmd
}

/// The isolation variables the builder's daemon runs under: every folder
/// under [`REMOTE_ROOT`], never the user's own.
pub fn isolation_env() -> [(&'static str, String); 4] {
    [
        (
            "RUSTLING_TULIP_CONFIG_DIR",
            format!(r"{REMOTE_ROOT}\config"),
        ),
        (
            "RUSTLING_TULIP_BINARIES_DIR",
            format!(r"{REMOTE_ROOT}\binaries"),
        ),
        (
            "RUSTLING_TULIP_WORKTREES_DIR",
            format!(r"{REMOTE_ROOT}\worktrees"),
        ),
        (
            "RUSTLING_TULIP_TAILNET_DIR",
            format!(r"{REMOTE_ROOT}\tailnet"),
        ),
    ]
}

/// A batch file (CRLF) that sets the isolation variables, then runs `tail`.
/// It never creates the config folder: over an elevated SSH logon the daemon
/// refuses one that inherits its parent's permissions.
fn isolating_script(tail: &str) -> String {
    let mut text = String::from("@echo off\r\n");
    for (name, value) in isolation_env() {
        for piece in ["set \"", name, "=", &value, "\"\r\n"] {
            text.push_str(piece);
        }
    }
    text.push_str(tail);
    text.push_str("\r\n");
    text
}

/// The text of [`DAEMON_WRAPPER`].
pub fn wrapper_script() -> String {
    isolating_script("\"%~dp0rustling-tulipd.exe\" %*")
}

/// The text of [`ISOLATED_WRAPPER`].
pub fn isolated_script() -> String {
    isolating_script("%*")
}

/// A remote command running `script` in Windows `PowerShell`, encoded so no
/// shell between here and there rewrites it.
pub fn powershell(script: &str) -> String {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    format!("powershell -NoProfile -NonInteractive -EncodedCommand {encoded}")
}

/// Refuse a remote path the builder's `cmd` would split or rewrite.
pub fn check_remote_path(path: &str) -> anyhow::Result<()> {
    ensure!(
        !path.is_empty()
            && !path
                .chars()
                .any(|c| c.is_whitespace() || "\"'&|<>^%;".contains(c)),
        "remote path {path:?} must have no spaces, quotes or shell characters"
    );
    Ok(())
}

/// `C:\a\b` as the `/C:/a/b` an SFTP-mode `scp` or `sftp` names it by.
pub fn sftp_path(windows_path: &str) -> String {
    let slashed = windows_path.replace('\\', "/");
    if slashed.as_bytes().get(1) == Some(&b':') {
        format!("/{slashed}")
    } else {
        slashed
    }
}

/// Run `cmd` to completion with its output captured, killed after `timeout`.
/// A failed read of either stream, or a reader thread's panic, is an error.
pub fn run_bounded(mut cmd: Command, timeout: Duration) -> anyhow::Result<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {cmd:?}"))?;
    let mut stdout = child.stdout.take().context("stdout")?;
    let mut stderr = child.stderr.take().context("stderr")?;
    let child = guard::share(child);
    let watch = guard::HangGuard::start(vec![child.clone()], timeout);
    let err_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).map(|_| buf)
    });
    let mut out = Vec::new();
    let read_out = stdout.read_to_end(&mut out);
    let read_err = err_reader
        .join()
        .map_err(|_| anyhow!("the stderr reader of {cmd:?} panicked"));
    let status = guard::wait(&child);
    if watch.finish() {
        bail!("hung: {cmd:?} ran past {}s", timeout.as_secs());
    }
    read_out.with_context(|| format!("reading the stdout of {cmd:?}"))?;
    let err = read_err?.with_context(|| format!("reading the stderr of {cmd:?}"))?;
    Ok(Output {
        status: status.with_context(|| format!("waiting for {cmd:?}"))?,
        stdout: out,
        stderr: err,
    })
}

/// The `PowerShell` that prints the isolated daemon's pid and CPU time from
/// `<config>\daemon.json`, or `none <reason>`. Under a config override the
/// daemon keeps its handshake at the override itself, elevated or not
/// (`daemon_client::profile`), so no other file is read.
pub fn daemon_probe_script() -> String {
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         $f = '{REMOTE_ROOT}\\config\\daemon.json'\n\
         if (-not (Test-Path -LiteralPath $f)) {{ 'none no daemon.json at ' + $f; exit 0 }}\n\
         $d = Get-Content -LiteralPath $f -Raw | ConvertFrom-Json\n\
         if (-not $d.pid) {{ 'none daemon.json names no pid'; exit 0 }}\n\
         $p = Get-Process -Id $d.pid -ErrorAction SilentlyContinue\n\
         if (-not $p) {{ 'none no process has pid ' + $d.pid; exit 0 }}\n\
         '{{0}} {{1}}' -f $d.pid, [long]$p.TotalProcessorTime.TotalMilliseconds\n"
    )
}

/// The probe's `<pid> <ms>`, or its `none <reason>` as the error.
pub fn parse_probe_output(text: &str) -> Result<(u32, u64), String> {
    let text = text.trim();
    if let Some(reason) = text.strip_prefix("none ") {
        return Err(reason.to_owned());
    }
    let mut parts = text.split_whitespace();
    let pid = parts.next().and_then(|p| p.parse().ok());
    let ms = parts.next().and_then(|m| m.parse().ok());
    match (pid, ms, parts.next()) {
        (Some(pid), Some(ms), None) => Ok((pid, ms)),
        _ => Err(format!("unexpected CPU probe output {text:?}")),
    }
}

/// The isolated daemon's pid and CPU time on the builder; the reason when it
/// cannot be read.
pub fn daemon_cpu_ms() -> Result<(u32, u64), String> {
    let output = run_bounded(ssh(&powershell(&daemon_probe_script())), PROBE_TIMEOUT)
        .map_err(|e| format!("{e:#}"))?;
    if !output.status.success() {
        return Err(format!(
            "the CPU probe exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_probe_output(&String::from_utf8_lossy(&output.stdout))
}

/// Read the daemon until two readings `pause` apart match, at most `polls`
/// readings: the last reading, and whether the daemon was idle.
pub fn wait_idle(
    mut read: impl FnMut() -> Result<(u32, u64), String>,
    mut pause: impl FnMut(),
    polls: usize,
) -> Result<((u32, u64), bool), String> {
    let mut last = read()?;
    for _ in 1..polls {
        pause();
        let next = read()?;
        if next == last {
            return Ok((next, true));
        }
        last = next;
    }
    Ok((last, false))
}

/// The `PowerShell` that stops the builder's `folder-bench.exe`, `tar.exe`
/// and `zstd.exe` whose command line names `folder`, and those processes'
/// children of the same names (a `zstd` names no folder; its `serve-tar`
/// parent does).
pub fn cleanup_script(folder: &str) -> String {
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         $names = 'folder-bench.exe','tar.exe','zstd.exe'\n\
         $all = @(Get-CimInstance Win32_Process | Where-Object {{ $names -contains $_.Name }})\n\
         $hit = @($all | Where-Object {{ $_.CommandLine -and $_.CommandLine.Contains('{folder}') }})\n\
         $ids = @($hit | ForEach-Object {{ $_.ProcessId }})\n\
         $ids += @($all | Where-Object {{ $ids -contains $_.ParentProcessId }} | ForEach-Object {{ $_.ProcessId }})\n\
         foreach ($id in $ids) {{ Stop-Process -Id $id -Force -ErrorAction SilentlyContinue }}\n\
         $left = @($ids | Where-Object {{ Get-Process -Id $_ -ErrorAction SilentlyContinue }})\n\
         if ($left.Count) {{ throw ('still running: ' + ($left -join ',')) }}\n\
         'stopped ' + $ids.Count\n"
    )
}

/// Stop what a hung run left on the builder ([`cleanup_script`]).
pub fn stop_remote(folder: &str) -> Result<(), String> {
    let output = run_bounded(ssh(&powershell(&cleanup_script(folder))), PROBE_TIMEOUT)
        .map_err(|e| format!("{e:#}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

pub fn push_command(rest: &[String]) -> anyhow::Result<ExitCode> {
    let parsed = args::parse(rest, &[])?;
    let exe = Path::new(parsed.single("local folder-bench.exe")?);
    ensure!(exe.is_file(), "{} is not a file", exe.display());
    let dir = exe.parent().context("the exe's folder")?;
    let mut files = vec![exe.to_path_buf()];
    for name in ["rustling-tulipd.exe", "rt-tracer.exe"] {
        let path = dir.join(name);
        if path.is_file() {
            files.push(path);
        } else {
            writeln!(
                std::io::stderr().lock(),
                "folder-bench push: {} is missing; the fetch candidates need it",
                path.display()
            )?;
        }
    }
    let scratch = std::env::temp_dir();
    let pid = std::process::id();
    let mut scripts = Vec::new();
    for (name, text) in [
        (DAEMON_WRAPPER, wrapper_script()),
        (ISOLATED_WRAPPER, isolated_script()),
    ] {
        let path = scratch.join(format!("folder-bench-{pid}-{name}"));
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        scripts.push((path, name));
    }

    let mkdir = format!("New-Item -ItemType Directory -Force -Path '{REMOTE_BIN}' | Out-Null");
    let made = run_bounded(ssh(&powershell(&mkdir)), PROBE_TIMEOUT)?;
    ensure!(
        made.status.success(),
        "creating {REMOTE_BIN} failed: {}",
        String::from_utf8_lossy(&made.stderr).trim()
    );
    let target = |name: &str| format!("{HOST}:{}/{name}", sftp_path(REMOTE_BIN));
    let mut copies = Vec::new();
    for file in &files {
        let name = file
            .file_name()
            .context("a file name")?
            .to_string_lossy()
            .into_owned();
        copies.push((file.clone(), target(&name)));
    }
    for (path, name) in &scripts {
        copies.push((path.clone(), target(name)));
    }
    for (from, to) in &copies {
        let mut scp = Command::new(tool("scp"));
        scp.args(["-q", "-o", "BatchMode=yes"]).arg(from).arg(to);
        let copied = run_bounded(scp, Duration::from_secs(600))?;
        ensure!(
            copied.status.success(),
            "scp {} {to} failed: {}",
            from.display(),
            String::from_utf8_lossy(&copied.stderr).trim()
        );
        writeln!(
            std::io::stdout().lock(),
            "pushed {} -> {to}",
            from.display()
        )?;
    }
    for (path, _) in &scripts {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn the_wrappers_isolate_every_folder_under_the_bench_root() {
        for script in [wrapper_script(), isolated_script()] {
            for name in [
                "RUSTLING_TULIP_CONFIG_DIR",
                "RUSTLING_TULIP_BINARIES_DIR",
                "RUSTLING_TULIP_WORKTREES_DIR",
            ] {
                assert!(
                    script.contains(&format!("set \"{name}={REMOTE_ROOT}\\")),
                    "{script}"
                );
            }
            assert!(!script.contains("mkdir"));
        }
        assert!(wrapper_script().ends_with("\"%~dp0rustling-tulipd.exe\" %*\r\n"));
        assert!(isolated_script().ends_with("\r\n%*\r\n"));
    }

    #[test]
    fn remote_paths_convert_and_unsafe_ones_are_refused() {
        assert_eq!(sftp_path(r"C:\Users\b\x"), "/C:/Users/b/x");
        assert!(check_remote_path(r"C:\Users\b\x").is_ok());
        assert!(check_remote_path(r"C:\Users\b c").is_err());
        assert!(check_remote_path(r"C:\a&b").is_err());
    }

    #[test]
    fn the_probe_reads_only_the_config_folders_daemon_json() {
        let script = daemon_probe_script();
        assert!(script.contains(&format!("$f = '{REMOTE_ROOT}\\config\\daemon.json'")));
        assert!(!script.contains("-Recurse"));
        assert!(!script.contains("Get-ChildItem"));
    }

    #[test]
    fn probe_output_parses_or_names_why_not() {
        assert_eq!(parse_probe_output("1234 5678\r\n"), Ok((1234, 5678)));
        assert_eq!(
            parse_probe_output("none daemon.json names no pid\r\n"),
            Err("daemon.json names no pid".to_owned())
        );
        assert!(parse_probe_output("").is_err());
        assert!(parse_probe_output("12 x").is_err());
        assert!(parse_probe_output("12 34 56").is_err());
    }

    #[test]
    fn idle_waits_for_two_equal_readings_and_gives_up_after_its_polls() {
        let mut readings = vec![Ok((7, 100)), Ok((7, 140)), Ok((7, 140))].into_iter();
        let mut pauses = 0;
        let result = wait_idle(|| readings.next().expect("a reading"), || pauses += 1, 31);
        assert_eq!(result, Ok(((7, 140), true)));
        assert_eq!(pauses, 2);

        let mut busy = (0..).map(|n| Ok((7, n * 10)));
        let result = wait_idle(|| busy.next().expect("a reading"), || {}, 4);
        assert_eq!(result, Ok(((7, 30), false)));

        let mut failing = vec![Ok((7, 1)), Err("no daemon.json".to_owned())].into_iter();
        let result = wait_idle(|| failing.next().expect("a reading"), || {}, 31);
        assert_eq!(result, Err("no daemon.json".to_owned()));
    }

    #[test]
    fn cleanup_targets_this_runs_folder_and_the_bench_processes() {
        let script = cleanup_script(r"C:\Users\b\fx\small-files");
        assert!(script.contains(r"$_.CommandLine.Contains('C:\Users\b\fx\small-files')"));
        assert!(script.contains("'folder-bench.exe','tar.exe','zstd.exe'"));
        assert!(script.contains("ParentProcessId"));
        assert!(script.contains("Stop-Process"));
    }
}
