//! Starting a process that inherits none of its caller's handles.
//!
//! `std::process::Command` on Windows always calls `CreateProcessW` with
//! `bInheritHandles = TRUE`, so a child gets every inheritable handle the
//! caller holds. A tracer the daemon starts would keep the daemon's loopback
//! listener and its clients' WebSocket connections open after the daemon
//! died, and a daemon a `bridge` starts would keep the SSH channel's pipes
//! open, so the ssh session would not end when the bridge exits. This module
//! calls `CreateProcessW` itself so the child inherits nothing of the caller's:
//! with [`StdHandles::Console`] inheritance is off and a console program
//! started with `CREATE_NO_WINDOW` runs in a hidden console of its own, its
//! std handles that console's; with [`StdHandles::Nul`] the child's three std
//! handles are the NUL device, the one handle a `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
//! lets it inherit.

use anyhow::{Context as _, bail};
use std::ffi::{OsStr, OsString};
use std::iter::repeat_n;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use tracing::warn;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, STARTUPINFOW_FLAGS, TerminateProcess,
    UpdateProcThreadAttribute,
};
use windows::core::{PCWSTR, PWSTR};

const QUOTE: u16 = b'"' as u16;
const BACKSLASH: u16 = b'\\' as u16;
const SPACE: u16 = b' ' as u16;
const TAB: u16 = b'\t' as u16;
const EQUALS: u16 = b'=' as u16;

/// What a child started by [`spawn_detached`] gets as its std handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdHandles {
    /// The NUL device for stdin, stdout and stderr (`STARTF_USESTDHANDLES`),
    /// the only handle the child inherits. For a GUI-subsystem child such as
    /// the daemon, whose std handles would otherwise be whatever values the
    /// caller's process parameters held.
    Nul,
    /// None: inheritance is off and no `STARTF_USESTDHANDLES` is set, so a
    /// console program started with `CREATE_NO_WINDOW` gets a hidden console
    /// of its own and its std handles are that console's. For tracers.
    Console,
}

/// Starts `program` (a path, run as given) with `args`, the process creation
/// flags `creation_flags` and the std handles `std_handles`, inheriting no
/// handle of the caller's, and returns the child with its process handle
/// kept, so the caller can end the very process it started: a pid looked up
/// later may name another process once this one has exited.
/// `CREATE_UNICODE_ENVIRONMENT` is always added.
///
/// The child's environment is the caller's with `set` applied and then
/// `remove`, so a name in both is removed; names compare case-insensitively.
/// Its working directory is the caller's. The child's process and thread
/// handles are closed when the returned [`OwnedChild`] drops.
///
/// # Errors
///
/// When an argument, a variable name or a value holds a NUL, the program path
/// holds a quote, the NUL device cannot be opened, or `CreateProcessW` fails;
/// that failure is a [`windows::core::Error`] in the chain, carrying the Win32
/// error.
pub fn spawn_detached(
    program: &Path,
    args: &[OsString],
    set: &[(OsString, OsString)],
    remove: &[OsString],
    creation_flags: u32,
    std_handles: StdHandles,
) -> anyhow::Result<OwnedChild> {
    create_process(program, args, set, remove, creation_flags, std_handles).map(OwnedChild)
}

/// The std handles a child is given, which are also the only handles it
/// inherits: the NUL device for [`StdHandles::Nul`], none for
/// [`StdHandles::Console`]. The NUL handle is closed on drop, once the child
/// has its own copy.
pub(crate) struct ChildStd {
    nul: Option<HANDLE>,
}

impl ChildStd {
    /// Opens the NUL device, inheritable, when `std_handles` asks for it.
    pub(crate) fn open(std_handles: StdHandles) -> anyhow::Result<Self> {
        if std_handles == StdHandles::Console {
            return Ok(Self { nul: None });
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
                .context("SECURITY_ATTRIBUTES size")?,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        // The device's name, NUL-terminated UTF-16 that outlives the call.
        let name: Vec<u16> = "NUL\0".encode_utf16().collect();
        // SAFETY: `name` is NUL-terminated and `attributes` is a live
        // SECURITY_ATTRIBUTES with its size set; the returned handle is owned
        // by `Self` and closed on drop.
        let nul = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                Some(&raw const attributes),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .context("opening the NUL device for the child's std handles")?;
        Ok(Self { nul: Some(nul) })
    }

    /// The handles the child inherits: the NUL handle, or none.
    pub(crate) fn inherited(&self) -> Vec<HANDLE> {
        self.nul.into_iter().collect()
    }
}

impl Drop for ChildStd {
    fn drop(&mut self) {
        if let Some(nul) = self.nul {
            // SAFETY: `nul` came from a successful `CreateFileW`, this value
            // owns it alone, and drop closes it exactly once.
            if let Err(err) = unsafe { CloseHandle(nul) } {
                warn!(?err, "closing the child's NUL handle failed");
            }
        }
    }
}

/// A `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` naming `handles`, the only handles a
/// child started with it inherits. The list and the handles it points at live
/// as long as this value, which deletes the list on drop.
struct HandleList {
    _buffer: Vec<usize>,
    handles: Vec<HANDLE>,
    list: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl HandleList {
    fn new(handles: Vec<HANDLE>) -> anyhow::Result<Self> {
        let mut size = 0usize;
        // SAFETY: a null list only asks for the size one attribute needs; the
        // call reports that through `size` and fails with a short buffer.
        let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &raw mut size) };
        if size == 0 {
            bail!("the attribute list size query returned nothing");
        }
        // `usize` cells keep the buffer pointer-aligned.
        let mut buffer = vec![0usize; size.div_ceil(size_of::<usize>())];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buffer.as_mut_ptr().cast());
        // SAFETY: `list` points at a writable buffer of at least `size` bytes,
        // which `Self` keeps alive until drop deletes the list.
        unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &raw mut size) }
            .context("InitializeProcThreadAttributeList")?;
        let this = Self {
            _buffer: buffer,
            handles,
            list,
        };
        let attribute =
            usize::try_from(PROC_THREAD_ATTRIBUTE_HANDLE_LIST).context("the attribute id")?;
        // SAFETY: `this.list` was initialised above for one attribute; the
        // handle array it points at is owned by `this` and outlives the list,
        // as the attribute requires.
        unsafe {
            UpdateProcThreadAttribute(
                this.list,
                0,
                attribute,
                Some(this.handles.as_ptr().cast()),
                size_of_val(this.handles.as_slice()),
                None,
                None,
            )
        }
        .context("UpdateProcThreadAttribute with the handle list")?;
        Ok(this)
    }
}

impl Drop for HandleList {
    fn drop(&mut self) {
        // SAFETY: `self.list` was initialised in `new` and is deleted once,
        // here, before its buffer is freed.
        unsafe { DeleteProcThreadAttributeList(self.list) };
    }
}

/// A child started by [`spawn_detached`]. Dropping it closes its
/// handles and leaves the process running.
pub struct OwnedChild(Spawned);

// SAFETY: the process and thread handles are kernel handles, valid from any
// thread of this process; `OwnedChild` owns them alone and only terminates
// and closes them, so moving it to another thread (a spawn awaited on the
// runtime's workers) shares nothing.
unsafe impl Send for OwnedChild {}

impl OwnedChild {
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.0.pid
    }

    /// Ends the child through the handle `CreateProcessW` returned. A failure
    /// (the child already gone, say) is logged, never raised.
    pub fn terminate(&self) {
        // SAFETY: `self.0.process` is the live process handle `Spawned` owns
        // until it drops.
        if let Err(err) = unsafe { TerminateProcess(self.0.process, 1) } {
            warn!(?err, pid = self.0.pid, "terminating a spawned child failed");
        }
    }
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
    creation_flags: u32,
    std_handles: StdHandles,
) -> anyhow::Result<Spawned> {
    let mut application = wide_without_nul(program.as_os_str()).context("the program path")?;
    application.push(0);
    let mut command_line = command_line(program, args)?;
    command_line.push(0);
    let environment = environment_block(std::env::vars_os(), set, remove)?;
    let child_std = ChildStd::open(std_handles)?;
    let inherited = child_std.inherited();
    // An empty handle list is invalid: with nothing to hand over, inheritance
    // is simply off.
    let handle_list = if inherited.is_empty() {
        None
    } else {
        Some(HandleList::new(inherited)?)
    };
    let mut flags = PROCESS_CREATION_FLAGS(creation_flags) | CREATE_UNICODE_ENVIRONMENT;
    if handle_list.is_some() {
        flags |= EXTENDED_STARTUPINFO_PRESENT;
    }
    let startup_size = if handle_list.is_some() {
        size_of::<STARTUPINFOEXW>()
    } else {
        size_of::<STARTUPINFOW>()
    };
    let nul = child_std.nul.unwrap_or_default();
    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: u32::try_from(startup_size).context("STARTUPINFO size")?,
            dwFlags: if child_std.nul.is_some() {
                STARTF_USESTDHANDLES
            } else {
                STARTUPINFOW_FLAGS::default()
            },
            hStdInput: nul,
            hStdOutput: nul,
            hStdError: nul,
            ..Default::default()
        },
        lpAttributeList: handle_list
            .as_ref()
            .map_or(LPPROC_THREAD_ATTRIBUTE_LIST::default(), |list| list.list),
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: `application` and `environment` are NUL-terminated (the block
    // with its double NUL) and outlive the call; `command_line` is a mutable,
    // NUL-terminated buffer the call may write into; `startup` is a zeroed
    // STARTUPINFOEXW whose `cb` names the part `flags` says is present, and
    // its attribute list and NUL handle (held by `handle_list` and
    // `child_std`) outlive the call; `info` is writable for the call.
    // Inheritance is on only with the handle list, which limits it to the NUL
    // handle.
    unsafe {
        CreateProcessW(
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            handle_list.is_some(),
            flags,
            Some(environment.as_ptr().cast()),
            PCWSTR::null(),
            (&raw const startup).cast::<STARTUPINFOW>(),
            &raw mut info,
        )
    }
    .with_context(|| format!("CreateProcessW for {}", program.display()))?;
    drop(handle_list);
    drop(child_std);
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
pub fn environment_block(
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
    use super::{StdHandles, append_quoted, create_process, environment_block, spawn_detached};
    use std::ffi::OsString;
    use std::path::Path;
    use tracing::warn;
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        CREATE_NO_WINDOW, GetExitCodeProcess, OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        TerminateProcess, WaitForSingleObject,
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
        let child = spawn_detached(
            Path::new(POWERSHELL),
            &args,
            &[],
            &[],
            CREATE_NO_WINDOW.0,
            StdHandles::Console,
        )
        .expect("spawn the child");
        let _kill = KillOnDrop(child.pid());
        drop(listener);
        let rebound = std::net::TcpListener::bind(addr);
        assert!(
            rebound.is_ok(),
            "the port is free once the daemon drops its listener, so the child holds no copy of it: {rebound:?}"
        );
    }

    #[test]
    fn an_owned_child_is_terminated_by_its_handle() {
        let args = os_strings(&["-NoProfile", "-Command", "Start-Sleep", "60"]);
        let child = spawn_detached(
            Path::new(POWERSHELL),
            &args,
            &[],
            &[],
            CREATE_NO_WINDOW.0,
            StdHandles::Console,
        )
        .expect("spawn the child");
        let pid = child.pid();
        let _guard = KillOnDrop(pid);
        child.terminate();
        // SAFETY: no pointer arguments; the returned handle is closed below.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
            .expect("open the child to wait on it");
        // SAFETY: `handle` was opened above with PROCESS_SYNCHRONIZE.
        let waited = unsafe { WaitForSingleObject(handle, 60_000) };
        // SAFETY: `handle` was opened above and is closed once, here.
        unsafe { CloseHandle(handle) }.expect("close the child's handle");
        assert_eq!(
            waited, WAIT_OBJECT_0,
            "the child has exited within a minute of terminate()"
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
        let spawned = create_process(
            Path::new(CMD),
            &args,
            &set,
            &remove,
            CREATE_NO_WINDOW.0,
            StdHandles::Console,
        )
        .expect("spawn cmd");
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
