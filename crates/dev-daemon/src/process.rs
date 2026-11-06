//! Starting the daemon, asking its `/health`, and the process clean-up that
//! kills only what runs from an isolated daemon's own folders.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::Value;

/// Starts `daemon` with `envs` set, `removed` taken out of the environment it
/// would inherit, and no console or standard streams.
pub(crate) fn spawn_daemon(
    daemon: &Path,
    envs: &[(&'static str, OsString)],
    removed: &[&'static str],
) -> std::io::Result<Child> {
    let mut command = Command::new(daemon);
    // A variable the spec leaves out of `envs` would otherwise arrive from
    // the parent shell and decide the daemon's shell.
    for name in removed {
        command.env_remove(*name);
    }
    command
        .envs(envs.iter().map(|(key, value)| (*key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    no_console(&mut command);
    command.spawn()
}

/// A loopback port nothing listens on right now.
pub fn free_port() -> std::io::Result<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
}

/// A plain `GET /health` over loopback; true on a 200.
pub(crate) fn health_ok(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let request = "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply);
    reply.starts_with("HTTP/1.1 200")
}

/// The tracer pids the daemon's session sidecars name.
pub(crate) fn sidecar_tracer_pids(config: &Path) -> HashSet<u32> {
    let Ok(entries) = std::fs::read_dir(config.join("sessions")) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("meta.json")).ok())
        .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
        .filter_map(|meta| meta["tracer_pid"].as_u64())
        .filter_map(|pid| u32::try_from(pid).ok())
        .collect()
}

pub(crate) fn remove_dir_retrying(dir: &Path) {
    for _ in 0..20 {
        if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    tracing::warn!("could not remove {}", dir.display());
}

/// Starts `command` with no console window.
#[cfg(windows)]
pub fn no_console(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

/// Starts `command` with no console window; nothing to do off Windows.
#[cfg(not(windows))]
pub fn no_console(_command: &mut Command) {}

/// Force-kills `pid` and, on Windows, every process under it.
pub fn kill_tree(pid: u32) {
    kill(pid, true);
}

/// Force-kills `pid` alone, leaving the processes under it to whatever
/// owns them (a tracer's child dies with the tracer's kill-on-close job).
pub fn kill_process(pid: u32) {
    kill(pid, false);
}

/// Force-kills `pid`, with its tree when `tree` is set. A process that is
/// already gone is not an error.
#[cfg(windows)]
fn kill(pid: u32, tree: bool) {
    let pid = pid.to_string();
    let mut command = Command::new("taskkill");
    command.args(["/PID", &pid, "/F"]);
    if tree {
        command.arg("/T");
    }
    command.stdout(Stdio::null()).stderr(Stdio::null());
    no_console(&mut command);
    let _ = command.status();
}

/// Force-kills `pid`; `kill` has no tree walk, so `tree` changes nothing.
#[cfg(not(windows))]
fn kill(pid: u32, _tree: bool) {
    let _ = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status();
}

/// Whether process `pid` runs an image under `dir`, so a pid read from a
/// file is never killed after another process reused it.
pub(crate) fn image_under(pid: u32, dir: &Path) -> bool {
    image_path(pid).is_some_and(|image| path_under(&image, dir))
}

/// Every running process whose image lies under `dir`, with that image.
#[must_use]
pub fn processes_under(dir: &Path) -> Vec<(u32, PathBuf)> {
    all_pids()
        .into_iter()
        .filter_map(|pid| {
            image_path(pid)
                .filter(|image| path_under(image, dir))
                .map(|image| (pid, image))
        })
        .collect()
}

/// Whether `path` lies inside `dir`, compared as Windows compares paths.
fn path_under(path: &Path, dir: &Path) -> bool {
    let norm = |p: &Path| {
        let text = p.to_string_lossy().replace('/', "\\").to_ascii_lowercase();
        let text = text.strip_prefix("\\\\?\\").unwrap_or(&text).to_owned();
        text.trim_end_matches('\\').to_owned()
    };
    norm(path)
        .strip_prefix(&norm(dir))
        .is_some_and(|rest| rest.starts_with('\\'))
}

/// The full path of the image process `pid` runs, when it can be read.
#[cfg(windows)]
fn image_path(pid: u32) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;
    // SAFETY: opens a query-only handle, closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buffer = vec![0_u16; 32_768];
    let mut len = u32::try_from(buffer.len()).ok()?;
    // SAFETY: the buffer and its length are locals that outlive the call.
    let read = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &raw mut len,
        )
    };
    // SAFETY: the handle OpenProcess returned, closed once.
    let _ = unsafe { CloseHandle(process) };
    read.ok()?;
    buffer.truncate(usize::try_from(len).ok()?);
    Some(PathBuf::from(OsString::from_wide(&buffer)))
}

#[cfg(not(windows))]
fn image_path(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// The id of every running process.
#[cfg(windows)]
fn all_pids() -> Vec<u32> {
    use windows::Win32::System::ProcessStatus::EnumProcesses;
    let mut pids = vec![0_u32; 1024];
    loop {
        let Ok(size) = u32::try_from(pids.len() * size_of::<u32>()) else {
            return Vec::new();
        };
        let mut needed = 0;
        // SAFETY: `pids` holds `size` bytes and outlives the call.
        if unsafe { EnumProcesses(pids.as_mut_ptr(), size, &raw mut needed) }.is_err() {
            return Vec::new();
        }
        if needed < size {
            pids.truncate(usize::try_from(needed).unwrap_or(0) / size_of::<u32>());
            return pids;
        }
        pids.resize(pids.len() * 2, 0);
    }
}

#[cfg(not(windows))]
fn all_pids() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}
