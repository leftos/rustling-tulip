//! Starting a process that inherits none of the daemon's handles.
//!
//! `std::process::Command` on Windows always calls `CreateProcessW` with
//! `bInheritHandles = TRUE`, so a child gets every inheritable handle the
//! daemon holds, among them its loopback listener and its clients' WebSocket
//! connections. A tracer that outlives the daemon would keep those sockets
//! open, and a client would stay connected to a dead daemon. This module calls
//! `CreateProcessW` itself with inheritance off. The child is not handed std
//! handles either: it runs in a hidden console of its own and its std handles
//! are that console's.

use anyhow::{Context as _, bail};
use std::ffi::{OsStr, OsString};
use std::iter::repeat_n;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use tracing::warn;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::core::{PCWSTR, PWSTR};

const QUOTE: u16 = b'"' as u16;
const BACKSLASH: u16 = b'\\' as u16;
const SPACE: u16 = b' ' as u16;
const TAB: u16 = b'\t' as u16;
const EQUALS: u16 = b'=' as u16;

/// Starts `program` with `args` and no inherited handles, and returns its pid.
///
/// A console program gets a hidden console of its own (`CREATE_NO_WINDOW`,
/// no `STARTF_USESTDHANDLES`), and its std handles are that console's.
///
/// The child's environment is the daemon's with `set` applied and then
/// `remove`, so a name in both is removed; names compare case-insensitively.
/// The child's process and thread handles are closed before this returns.
///
/// # Errors
///
/// When an argument, a variable name or a value holds a NUL, the program path
/// holds a quote, or `CreateProcessW` fails.
pub(crate) fn spawn_detached(
    program: &Path,
    args: &[OsString],
    set: &[(OsString, OsString)],
    remove: &[OsString],
) -> anyhow::Result<u32> {
    let spawned = create_process(program, args, set, remove)?;
    Ok(spawned.pid)
}

/// A started child: its pid and the process and thread handles
/// `CreateProcessW` returned, closed on drop.
struct Spawned {
    pid: u32,
    process: HANDLE,
    thread: HANDLE,
}

impl Drop for Spawned {
    fn drop(&mut self) {
        for handle in [self.process, self.thread] {
            // SAFETY: `handle` came from a successful `CreateProcessW`, this
            // value owns it alone, and drop closes it exactly once.
            if let Err(err) = unsafe { CloseHandle(handle) } {
                warn!(
                    ?err,
                    pid = self.pid,
                    "closing a spawned child's handle failed"
                );
            }
        }
    }
}

fn create_process(
    program: &Path,
    args: &[OsString],
    set: &[(OsString, OsString)],
    remove: &[OsString],
) -> anyhow::Result<Spawned> {
    let mut application = wide_without_nul(program.as_os_str()).context("the program path")?;
    application.push(0);
    let mut command_line = command_line(program, args)?;
    command_line.push(0);
    let environment = environment_block(std::env::vars_os(), set, remove)?;
    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).context("STARTUPINFOW size")?,
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: `application` and `environment` are NUL-terminated (the block
    // with its double NUL) and outlive the call; `command_line` is a mutable,
    // NUL-terminated buffer the call may write into; `startup` is a zeroed
    // STARTUPINFOW with its size set; `info` is writable for the call.
    unsafe {
        CreateProcessW(
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            Some(environment.as_ptr().cast()),
            PCWSTR::null(),
            &raw const startup,
            &raw mut info,
        )
    }
    .with_context(|| format!("CreateProcessW for {}", program.display()))?;
    Ok(Spawned {
        pid: info.dwProcessId,
        process: info.hProcess,
        thread: info.hThread,
    })
}

/// The command line `CreateProcessW` is given: the program path in quotes,
/// then each arg quoted as `CommandLineToArgvW` and `std::env::args` parse it.
fn command_line(program: &Path, args: &[OsString]) -> anyhow::Result<Vec<u16>> {
    let program_wide = wide_without_nul(program.as_os_str()).context("the program path")?;
    if program_wide.contains(&QUOTE) {
        bail!("the program path {} holds a quote", program.display());
    }
    let mut line = vec![QUOTE];
    line.extend(program_wide);
    line.push(QUOTE);
    for (index, arg) in args.iter().enumerate() {
        line.push(SPACE);
        append_quoted(
            &mut line,
            &wide_without_nul(arg).with_context(|| format!("argument {index}"))?,
        );
    }
    Ok(line)
}

/// Appends `arg` to `line`, in quotes when it is empty or holds a space, tab or
/// quote. A quote is escaped with a backslash and the backslashes before it
/// doubled; backslashes before the closing quote are doubled too.
fn append_quoted(line: &mut Vec<u16>, arg: &[u16]) {
    let quote = arg.is_empty() || arg.iter().any(|&c| c == SPACE || c == TAB || c == QUOTE);
    if quote {
        line.push(QUOTE);
    }
    let mut backslashes = 0;
    for &c in arg {
        if c == BACKSLASH {
            backslashes += 1;
        } else {
            if c == QUOTE {
                line.extend(repeat_n(BACKSLASH, backslashes + 1));
            }
            backslashes = 0;
        }
        line.push(c);
    }
    if quote {
        line.extend(repeat_n(BACKSLASH, backslashes));
        line.push(QUOTE);
    }
}

/// The UTF-16 environment block for `CreateProcessW`: `current` with `set`
/// applied and then `remove`, sorted by name case-insensitively, each
/// `NAME=value` NUL-terminated and the block ending in an extra NUL.
pub(crate) fn environment_block(
    current: impl IntoIterator<Item = (OsString, OsString)>,
    set: &[(OsString, OsString)],
    remove: &[OsString],
) -> anyhow::Result<Vec<u16>> {
    let mut vars: Vec<(OsString, OsString)> = current.into_iter().collect();
    // A set name matching an inherited one keeps the inherited spelling, as
    // `std::process::Command` does, and replaces only the value.
    for (name, value) in set {
        match vars
            .iter_mut()
            .find(|(existing, _)| same_name(existing, name))
        {
            Some((_, existing_value)) => existing_value.clone_from(value),
            None => vars.push((name.clone(), value.clone())),
        }
    }
    for name in remove {
        vars.retain(|(existing, _)| !same_name(existing, name));
    }
    vars.sort_by_cached_key(|(name, _)| name_key(name));
    let mut block = Vec::new();
    for (name, value) in &vars {
        let context = || format!("environment variable {}", name.display());
        block.extend(wide_without_nul(name).with_context(context)?);
        block.push(EQUALS);
        block.extend(wide_without_nul(value).with_context(context)?);
        block.push(0);
    }
    if vars.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

/// The form environment names compare and sort by: Windows treats them
/// case-insensitively.
fn name_key(name: &OsStr) -> String {
    name.to_string_lossy().to_uppercase()
}

fn same_name(a: &OsStr, b: &OsStr) -> bool {
    name_key(a) == name_key(b)
}

/// `s` as UTF-16, refused when it holds a NUL (which would end it early). The
/// error never quotes `s`, since a value can be a secret.
fn wide_without_nul(s: &OsStr) -> anyhow::Result<Vec<u16>> {
    let wide: Vec<u16> = s.encode_wide().collect();
    if wide.contains(&0) {
        bail!("holds a NUL character");
    }
    Ok(wide)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert spawn and handle preconditions with expect for clear failure messages"
)]
mod tests {
    use super::{append_quoted, create_process, environment_block, spawn_detached};
    use std::ffi::OsString;
    use std::path::Path;
    use tracing::warn;
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
    };

    const POWERSHELL: &str = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
    const CMD: &str = r"C:\Windows\System32\cmd.exe";

    fn quoted(arg: &str) -> String {
        let mut line = Vec::new();
        append_quoted(&mut line, &arg.encode_utf16().collect::<Vec<u16>>());
        String::from_utf16(&line).expect("quoted arg is UTF-16")
    }

    fn os_strings(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// Terminates the process with this pid on drop, so a failing test leaves
    /// no child behind.
    struct KillOnDrop(u32);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            // SAFETY: no pointer arguments; the returned handle is closed below.
            let handle = match unsafe { OpenProcess(PROCESS_TERMINATE, false, self.0) } {
                Ok(handle) => handle,
                Err(err) => {
                    warn!(?err, pid = self.0, "cannot open the test child to end it");
                    return;
                }
            };
            // SAFETY: `handle` was opened above with PROCESS_TERMINATE.
            if let Err(err) = unsafe { TerminateProcess(handle, 1) } {
                warn!(?err, pid = self.0, "cannot end the test child");
            }
            // SAFETY: `handle` was opened above and is closed once, here.
            if let Err(err) = unsafe { CloseHandle(handle) } {
                warn!(?err, pid = self.0, "cannot close the test child's handle");
            }
        }
    }

    #[test]
    fn setting_a_variable_keeps_the_inherited_spelling_of_its_name() {
        let current = vec![
            (OsString::from("Path"), OsString::from("old")),
            (OsString::from("alpha"), OsString::from("1")),
        ];
        let set = [(OsString::from("PATH"), OsString::from("new"))];
        let block = environment_block(current, &set, &[]).expect("build the block");
        let text = String::from_utf16(&block).expect("block is UTF-16");
        assert_eq!(
            text, "alpha=1\0Path=new\0\0",
            "one Path entry under its inherited spelling, names sorted case-insensitively, the block ending in a double NUL"
        );
    }

    #[test]
    fn args_are_quoted_as_command_line_to_argv_parses_them() {
        assert_eq!(quoted("a"), "a");
        assert_eq!(quoted("a b"), r#""a b""#);
        assert_eq!(quoted(""), r#""""#);
        assert_eq!(quoted(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quoted(r"a\b"), r"a\b");
        assert_eq!(quoted(r#"a\\"b"#), r#""a\\\\\"b""#);
        assert_eq!(quoted(r"C:\Program Files\x\"), r#""C:\Program Files\x\\""#);
    }

    #[tokio::test]
    async fn child_does_not_keep_the_daemon_listener_open() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the loopback listener");
        let addr = listener.local_addr().expect("listener address");
        let args = os_strings(&["-NoProfile", "-Command", "Start-Sleep", "-Seconds", "10"]);
        let pid = spawn_detached(Path::new(POWERSHELL), &args, &[], &[]).expect("spawn the child");
        let _child = KillOnDrop(pid);
        drop(listener);
        let rebound = std::net::TcpListener::bind(addr);
        assert!(
            rebound.is_ok(),
            "the port is free once the daemon drops its listener, so the child holds no copy of it: {rebound:?}"
        );
    }

    #[test]
    fn child_sees_set_variables_and_not_removed_ones() {
        assert!(
            std::env::var_os("COMPUTERNAME").is_some(),
            "precondition: the test process has COMPUTERNAME to remove"
        );
        let args = os_strings(&[
            "/c",
            "if",
            "defined",
            "RT_SPAWN_SET",
            "(if",
            "defined",
            "COMPUTERNAME",
            "(exit",
            "5)",
            "else",
            "(exit",
            "3))",
            "else",
            "(exit",
            "4)",
        ]);
        let set = [(OsString::from("RT_SPAWN_SET"), OsString::from("1"))];
        let remove = [OsString::from("computername")];
        let spawned = create_process(Path::new(CMD), &args, &set, &remove).expect("spawn cmd");
        // SAFETY: `spawned.process` is a live process handle `spawned` owns.
        let waited = unsafe { WaitForSingleObject(spawned.process, 60_000) };
        assert_eq!(waited, WAIT_OBJECT_0, "cmd exits within a minute");
        let mut code = 0u32;
        // SAFETY: `spawned.process` is a live process handle; `code` is writable.
        unsafe { GetExitCodeProcess(spawned.process, &raw mut code) }
            .expect("read cmd's exit code");
        assert_eq!(
            code, 3,
            "3: the set variable is seen and the removed one is not; 4: the set one is missing; 5: the removed one is still there"
        );
    }
}
