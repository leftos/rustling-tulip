//! Starting the daemon inside the user's own interactive logon through an
//! on-demand scheduled task, for `rustling-tulipd bridge`.
//!
//! An SSH logon is a network logon in session 0: a daemon started there gets
//! no working Credential Manager vault, and for an administrator it runs
//! elevated. When the same user also has an interactive session on the host,
//! the bridge registers one task per user (`\leftos\rustling-tulip\daemon-<SID>`,
//! `InteractiveToken`, `LeastPrivilege`, no trigger) and runs it, so the daemon
//! starts inside that logon at the user's ordinary integrity. The task runs
//! only in an existing interactive session; with none it starts without error
//! and never runs, so the caller waits for the ordinary profile's handshake
//! and starts the daemon itself when none appears.

use std::path::Path;
use std::time::Duration;

/// How the bridge starts a daemon when none runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartRoute {
    /// Through the user's scheduled task, inside their interactive logon.
    LogonTask,
    /// Here, as `ensure_running` starts it for every other caller.
    Direct,
}

/// How long the bridge waits for the task's daemon to report its handshake.
pub(crate) const LOGON_TASK_WAIT: Duration = Duration::from_secs(20);

/// The bound on each `schtasks.exe` call; a call still running is killed.
#[cfg(windows)]
const SCHTASKS_TIMEOUT: Duration = Duration::from_secs(30);

/// The route for a caller in session `caller_session`: the task only from
/// session 0 (an SSH logon) when the caller's own user has an interactive
/// session and no override folder is set (`override_set`), else here. The
/// task's daemon starts in the user's logon environment, without the caller's
/// overrides, so an isolated run would start a real-profile daemon.
#[cfg(any(windows, test))]
pub(crate) fn start_route(
    caller_session: u32,
    user_has_interactive_session: bool,
    override_set: bool,
) -> StartRoute {
    if caller_session == 0 && user_has_interactive_session && !override_set {
        StartRoute::LogonTask
    } else {
        StartRoute::Direct
    }
}

/// The task's name for the user `user_sid`: one task per user.
#[cfg_attr(
    not(any(windows, test)),
    expect(dead_code, reason = "the task route runs on Windows only")
)]
pub(crate) fn task_name(user_sid: &str) -> String {
    format!(r"\leftos\rustling-tulip\daemon-{user_sid}")
}

/// The Task Scheduler 1.2 definition that runs `exe --detach` in `work_dir`
/// as the user `user_sid`, inside their existing interactive logon, at least
/// privilege, hidden, on demand only, with no time limit, at priority 4
/// (normal class, normal I/O and memory priority; the scheduler's default 7
/// runs below normal, and the daemon's children inherit it). Its security
/// descriptor gives Administrators and SYSTEM full access and the user read
/// and delete only, so the user's own uninstaller can delete a task an
/// elevated logon registered, while the user cannot rewrite it.
#[cfg_attr(
    not(any(windows, test)),
    expect(dead_code, reason = "the task route runs on Windows only")
)]
pub(crate) fn task_xml(user_sid: &str, exe: &Path, work_dir: &Path) -> String {
    let sid = xml_escape(user_sid);
    let command = xml_escape(&exe.to_string_lossy());
    let work_dir = xml_escape(&work_dir.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <SecurityDescriptor>D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;FRSD;;;{sid})</SecurityDescriptor>
  </RegistrationInfo>
  <Principals>
    <Principal id="Author">
      <UserId>{sid}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>--detach</Arguments>
      <WorkingDirectory>{work_dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// `text` with the five XML special characters escaped.
#[cfg_attr(
    not(any(windows, test)),
    expect(dead_code, reason = "the task route runs on Windows only")
)]
fn xml_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// This process's route and the session it names: the user's interactive
/// session for [`StartRoute::LogonTask`], this process's own otherwise.
/// `override_set` says the caller runs with an override folder set. A read
/// that fails counts as no interactive session and is logged at warn.
#[cfg(windows)]
pub(crate) fn this_process_route(override_set: bool) -> (StartRoute, u32) {
    let caller_session = match win::own_session() {
        Ok(session) => session,
        Err(err) => {
            tracing::warn!(
                err = %format!("{err:#}"),
                "could not read this process's session; starting the daemon here"
            );
            return (StartRoute::Direct, 0);
        }
    };
    if override_set {
        tracing::info!("an override folder is set; starting the daemon here");
    }
    if caller_session != 0 || override_set {
        // No session lookup: either input alone rules the task out.
        return (
            start_route(caller_session, false, override_set),
            caller_session,
        );
    }
    let user_session = win::user_interactive_session().unwrap_or_else(|err| {
        tracing::warn!(
            err = %format!("{err:#}"),
            "could not look for the user's interactive session; starting the daemon here"
        );
        None
    });
    match user_session {
        Some(session) => (start_route(caller_session, true, override_set), session),
        None => (
            start_route(caller_session, false, override_set),
            caller_session,
        ),
    }
}

/// Always [`StartRoute::Direct`]: the task route exists on Windows only.
#[cfg(not(windows))]
pub(crate) fn this_process_route(override_set: bool) -> (StartRoute, u32) {
    let _ = override_set;
    (StartRoute::Direct, 0)
}

/// Register the user's task for `exe` (replacing any earlier one) and run it;
/// `session` is the user's interactive session, for the log.
///
/// # Errors
///
/// The caller's SID could not be read, the definition could not be written,
/// or a `schtasks.exe` call failed, exited non-zero or timed out.
#[cfg(windows)]
pub(crate) async fn start_in_user_logon(exe: &Path, session: u32) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::ffi::OsString;

    let sid = win::own_user_sid()?;
    let name = task_name(&sid);
    let work_dir = exe
        .parent()
        .with_context(|| format!("{} has no parent folder", exe.display()))?;
    tracing::info!(
        task = %name,
        session,
        "starting the daemon in the user's logon through a scheduled task"
    );
    let xml_path = std::env::temp_dir().join(format!(
        "rustling-tulip-daemon-task-{}.xml",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&xml_path, utf16_le_with_bom(&task_xml(&sid, exe, work_dir)))
        .with_context(|| format!("writing {}", xml_path.display()))?;
    let created = schtasks(
        "/Create",
        vec![
            OsString::from("/Create"),
            OsString::from("/TN"),
            OsString::from(&name),
            OsString::from("/XML"),
            xml_path.clone().into_os_string(),
            OsString::from("/F"),
        ],
    )
    .await;
    if let Err(err) = std::fs::remove_file(&xml_path) {
        tracing::debug!(path = %xml_path.display(), %err, "could not delete the task definition");
    }
    created?;
    schtasks(
        "/Run",
        vec![
            OsString::from("/Run"),
            OsString::from("/TN"),
            OsString::from(&name),
        ],
    )
    .await
}

/// Never called: [`this_process_route`] is always direct on this platform.
///
/// # Errors
///
/// Always: there is no task route here.
#[cfg(not(windows))]
#[expect(
    clippy::unused_async,
    reason = "async in step with the Windows route, which awaits schtasks"
)]
pub(crate) async fn start_in_user_logon(exe: &Path, session: u32) -> anyhow::Result<()> {
    let _ = (exe, session);
    Err(anyhow::anyhow!(
        "the logon task route exists on Windows only"
    ))
}

/// `text` as UTF-16 LE with a byte-order mark, the encoding `schtasks /XML`
/// reads.
#[cfg(windows)]
fn utf16_le_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    bytes
}

/// Run `schtasks.exe` with `args` (`verb` names it in a failure), with no
/// stdin, its output captured, within [`SCHTASKS_TIMEOUT`].
#[cfg(windows)]
async fn schtasks(verb: &str, args: Vec<std::ffi::OsString>) -> anyhow::Result<()> {
    use anyhow::{Context as _, anyhow};
    use std::process::Stdio;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let program = std::env::var_os("SystemRoot").map_or_else(
        || std::path::PathBuf::from("schtasks.exe"),
        |root| {
            std::path::PathBuf::from(root)
                .join("System32")
                .join("schtasks.exe")
        },
    );
    let mut command = tokio::process::Command::new(&program);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        // A call that outlives its bound is killed when its wait is dropped.
        .kill_on_drop(true);
    let child = command
        .spawn()
        .with_context(|| format!("starting schtasks {verb}"))?;
    let output = tokio::time::timeout(SCHTASKS_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            anyhow!(
                "schtasks {verb} did not finish within {} s",
                SCHTASKS_TIMEOUT.as_secs()
            )
        })?
        .with_context(|| format!("waiting for schtasks {verb}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(anyhow!(
        "schtasks {verb} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

#[cfg(windows)]
mod win {
    //! The token and session reads the route decides on.

    use anyhow::{Context as _, anyhow};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{
        GetTokenInformation, LookupAccountSidW, PSID, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows::Win32::System::RemoteDesktop::{
        ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTS_INFO_CLASS, WTS_SESSION_INFOW,
        WTSActive, WTSDisconnected, WTSDomainName, WTSEnumerateSessionsW, WTSFreeMemory,
        WTSQuerySessionInformationW, WTSUserName,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, OpenProcessToken,
    };
    use windows::core::{PCWSTR, PWSTR};

    /// A handle closed when dropped.
    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle was opened by this module and is closed once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    /// The session this process runs in.
    pub(super) fn own_session() -> anyhow::Result<u32> {
        let mut session = 0u32;
        // SAFETY: `session` is a live out-pointer for the call.
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &raw mut session) }
            .context("reading this process's session")?;
        Ok(session)
    }

    /// The caller token's `TOKEN_USER`, as `u64` words aligned for it.
    fn own_token_user() -> anyhow::Result<Vec<u64>> {
        let mut token = HANDLE::default();
        // SAFETY: the current-process pseudo-handle needs no closing; `token`
        // receives a handle closed by `Handle`.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }
            .context("opening this process's token")?;
        let token = Handle(token);
        let mut needed = 0u32;
        // SAFETY: a size query with no buffer; it fails with
        // ERROR_INSUFFICIENT_BUFFER and sets `needed`.
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &raw mut needed) };
        if needed == 0 {
            return Err(anyhow!("this process's token reported no user"));
        }
        let mut buffer = vec![0u64; usize::try_from(needed)?.div_ceil(8)];
        // SAFETY: `buffer` holds at least `needed` bytes and outlives the call.
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                needed,
                &raw mut needed,
            )
        }
        .context("reading this process's token user")?;
        Ok(buffer)
    }

    /// The user SID in `user`, a `TOKEN_USER` buffer [`own_token_user`] read.
    fn sid_of(user: &[u64]) -> PSID {
        // SAFETY: the buffer holds a TOKEN_USER whose SID points into the
        // same buffer, which is alive and aligned while `user` is.
        unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    /// The caller token's user SID in string form (`S-1-5-21-…`).
    pub(super) fn own_user_sid() -> anyhow::Result<String> {
        let user = own_token_user()?;
        let mut wide = PWSTR::null();
        // SAFETY: the SID is valid while `user` lives; `wide` receives a
        // LocalAlloc'd string freed below.
        unsafe { ConvertSidToStringSidW(sid_of(&user), &raw mut wide) }
            .context("converting this process's user SID")?;
        // SAFETY: the conversion returned a NUL-terminated string.
        let text = unsafe { wide.to_string() };
        // SAFETY: `wide` is the string the conversion allocated.
        let _ = unsafe { LocalFree(Some(HLOCAL(wide.0.cast()))) };
        text.context("this process's user SID is not UTF-16")
    }

    /// The caller token's account name and domain.
    fn own_account() -> anyhow::Result<(String, String)> {
        let user = own_token_user()?;
        let sid = sid_of(&user);
        let (mut name_len, mut domain_len) = (0u32, 0u32);
        let mut kind = SID_NAME_USE::default();
        // SAFETY: a size query with no buffers; it fails with
        // ERROR_INSUFFICIENT_BUFFER and sets both lengths.
        let _ = unsafe {
            LookupAccountSidW(
                PCWSTR::null(),
                sid,
                None,
                &raw mut name_len,
                None,
                &raw mut domain_len,
                &raw mut kind,
            )
        };
        if name_len == 0 {
            return Err(anyhow!(
                "looking up this process's account reported no name"
            ));
        }
        let mut name = vec![0u16; usize::try_from(name_len)?];
        let mut domain = vec![0u16; usize::try_from(domain_len.max(1))?];
        // SAFETY: each buffer holds the length passed beside it and outlives
        // the call.
        unsafe {
            LookupAccountSidW(
                PCWSTR::null(),
                sid,
                Some(PWSTR(name.as_mut_ptr())),
                &raw mut name_len,
                Some(PWSTR(domain.as_mut_ptr())),
                &raw mut domain_len,
                &raw mut kind,
            )
        }
        .context("looking up this process's account")?;
        // On success the lengths exclude the terminating NUL.
        name.truncate(usize::try_from(name_len)?);
        domain.truncate(usize::try_from(domain_len)?);
        Ok((String::from_utf16(&name)?, String::from_utf16(&domain)?))
    }

    /// The string `class` of session `session`.
    fn session_string(session: u32, class: WTS_INFO_CLASS) -> anyhow::Result<String> {
        let mut buffer = PWSTR::null();
        let mut bytes = 0u32;
        // SAFETY: both out-pointers are live; `buffer` receives memory freed
        // with WTSFreeMemory below.
        unsafe {
            WTSQuerySessionInformationW(
                Some(WTS_CURRENT_SERVER_HANDLE),
                session,
                class,
                &raw mut buffer,
                &raw mut bytes,
            )
        }
        .with_context(|| format!("querying session {session}"))?;
        // SAFETY: the query returned a NUL-terminated string.
        let text = unsafe { buffer.to_string() };
        // SAFETY: `buffer` is the memory the query allocated.
        unsafe { WTSFreeMemory(buffer.0.cast()) };
        Ok(text?)
    }

    /// An Active or Disconnected session (other than session 0) whose user is
    /// the caller token's account, compared case-insensitively, or `None`.
    pub(super) fn user_interactive_session() -> anyhow::Result<Option<u32>> {
        let (name, domain) = own_account()?;
        let (name, domain) = (name.to_lowercase(), domain.to_lowercase());
        let mut info: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut count = 0u32;
        // SAFETY: both out-pointers are live; `info` receives memory freed
        // with WTSFreeMemory below.
        unsafe {
            WTSEnumerateSessionsW(
                Some(WTS_CURRENT_SERVER_HANDLE),
                0,
                1,
                &raw mut info,
                &raw mut count,
            )
        }
        .context("enumerating the host's sessions")?;
        let candidates: Vec<u32> = if info.is_null() {
            Vec::new()
        } else {
            // SAFETY: the call returned `count` entries at `info`, alive until
            // freed below.
            let sessions = unsafe { std::slice::from_raw_parts(info, usize::try_from(count)?) };
            let ids = sessions
                .iter()
                .filter(|s| {
                    s.SessionId != 0 && (s.State == WTSActive || s.State == WTSDisconnected)
                })
                .map(|s| s.SessionId)
                .collect();
            // SAFETY: `info` is the memory the enumeration allocated.
            unsafe { WTSFreeMemory(info.cast()) };
            ids
        };
        for session in candidates {
            let account = session_string(session, WTSUserName)
                .and_then(|user| Ok((user, session_string(session, WTSDomainName)?)));
            let (user, user_domain) = match account {
                Ok(account) => account,
                Err(err) => {
                    // One unreadable session leaves the others to look at.
                    tracing::debug!(
                        session,
                        err = %format!("{err:#}"),
                        "could not read a session's user; skipping it"
                    );
                    continue;
                }
            };
            if user.to_lowercase() == name && user_domain.to_lowercase() == domain {
                return Ok(Some(session));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::{StartRoute, start_route, task_name, task_xml};
    use std::path::Path;

    const SID: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";

    #[test]
    fn start_route_uses_the_task_only_from_session_0_with_a_user_session() {
        assert_eq!(start_route(0, true, false), StartRoute::LogonTask);
        assert_eq!(start_route(0, false, false), StartRoute::Direct);
        assert_eq!(start_route(1, true, false), StartRoute::Direct);
        assert_eq!(start_route(1, false, false), StartRoute::Direct);
    }

    #[test]
    fn start_route_starts_here_when_an_override_folder_is_set() {
        for (session, has_session) in [(0, true), (0, false), (1, true), (1, false)] {
            assert_eq!(
                start_route(session, has_session, true),
                StartRoute::Direct,
                "session {session}, user session {has_session}"
            );
        }
    }

    #[test]
    fn task_xml_runs_the_daemon_hidden_at_least_privilege_only_while_logged_on() {
        let exe = Path::new(r"C:\Program Files\rustling-tulip\rustling-tulipd.exe");
        let xml = task_xml(SID, exe, Path::new(r"C:\Program Files\rustling-tulip"));
        for needle in [
            r#"encoding="UTF-16""#,
            r#"version="1.2""#,
            &format!("<UserId>{SID}</UserId>"),
            "<LogonType>InteractiveToken</LogonType>",
            "<RunLevel>LeastPrivilege</RunLevel>",
            "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<Priority>4</Priority>",
            "<Hidden>true</Hidden>",
            "<AllowStartOnDemand>true</AllowStartOnDemand>",
            "<Enabled>true</Enabled>",
            r"<Command>C:\Program Files\rustling-tulip\rustling-tulipd.exe</Command>",
            "<Arguments>--detach</Arguments>",
            r"<WorkingDirectory>C:\Program Files\rustling-tulip</WorkingDirectory>",
        ] {
            assert!(xml.contains(needle), "{needle} missing from:\n{xml}");
        }
        assert!(
            !xml.contains("<Triggers>"),
            "the task has no trigger:\n{xml}"
        );
    }

    #[test]
    fn task_xml_lets_the_user_delete_the_task_but_not_change_it() {
        let dir = Path::new(r"C:\Program Files\rustling-tulip");
        let xml = task_xml(SID, &dir.join("rustling-tulipd.exe"), dir);
        assert!(xml.contains("<RegistrationInfo>"), "{xml}");
        let descriptor = format!(
            "<SecurityDescriptor>D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;FRSD;;;{SID})</SecurityDescriptor>"
        );
        assert!(
            xml.contains(&descriptor),
            "{descriptor} missing from:\n{xml}"
        );
    }

    /// The installer's uninstall step deletes the task by this folder and
    /// name (`tools/installer/rustling-tulip.nsi`); the two stay in step.
    #[test]
    fn task_name_is_the_folder_and_daemon_dash_sid() {
        assert_eq!(
            task_name(SID),
            format!(r"\leftos\rustling-tulip\daemon-{SID}")
        );
    }

    #[test]
    fn task_xml_escapes_paths() {
        let dir = Path::new(r"C:\Tom & Jerry's <tools>");
        let xml = task_xml(SID, &dir.join("rustling-tulipd.exe"), dir);
        // The joined exe carries this platform's separator.
        #[cfg(windows)]
        let command =
            r"<Command>C:\Tom &amp; Jerry&apos;s &lt;tools&gt;\rustling-tulipd.exe</Command>";
        #[cfg(not(windows))]
        let command =
            r"<Command>C:\Tom &amp; Jerry&apos;s &lt;tools&gt;/rustling-tulipd.exe</Command>";
        assert!(xml.contains(command), "{xml}");
        assert!(
            xml.contains(
                r"<WorkingDirectory>C:\Tom &amp; Jerry&apos;s &lt;tools&gt;</WorkingDirectory>"
            ),
            "{xml}"
        );
        assert!(!xml.contains("Tom & Jerry"), "{xml}");
    }
}
