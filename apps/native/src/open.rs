//! Opening what a terminal link points at: a URL in the browser, a path with a
//! `:line` in VS Code at that line, and a plain path in the app the OS has
//! registered for it.
//!
//! A path resolves against the session's folders and the first reading of the
//! link that exists on disk wins, so a path stitched over-eagerly across a
//! row break degrades to the fragment that was on screen. A network path is
//! only looked at on a host behind a mapped drive or one the user listed, and
//! a file whose default action runs code is only opened once the user says so.
//! Everything here that touches the system runs off the UI thread.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use protocol::SessionSnapshot;

use crate::links::TerminalLinkCandidate;
use crate::sidebar::load_unc_hosts;

/// The extensions whose default action runs code, besides the types Windows
/// itself counts a risk: a link to one asks before it opens.
const EXECUTABLE_EXTENSIONS: &[&str] = &[
    "exe",
    "bat",
    "cmd",
    "com",
    "ps1",
    "psm1",
    "psc1",
    "vbs",
    "vbe",
    "vb",
    "js",
    "jse",
    "ws",
    "wsf",
    "wsh",
    "hta",
    "msi",
    "msp",
    "msc",
    "lnk",
    "url",
    "scr",
    "scf",
    "pif",
    "cpl",
    "reg",
    "inf",
    "jar",
    "py",
    "pyw",
    "sh",
    "bash",
    "chm",
    "xll",
    "gadget",
    "cmdline",
    "appref-ms",
    "application",
    "appx",
    "appxbundle",
    "msix",
    "msixbundle",
    "appinstaller",
    "settingcontent-ms",
    "library-ms",
    "search-ms",
];

/// Where a terminal link is opened. The client passes [`SystemOpener`]; a
/// spec passes a recorder, since the test platform cannot open anything.
/// Every method may block, so each runs on a background thread.
pub trait Opener: Send + Sync {
    /// Opens `url`, an `http` or `https` URL [`validate_http_url`] passed.
    ///
    /// # Errors
    /// When the URL could not be handed to the browser.
    fn url(&self, url: &str) -> Result<(), String>;

    /// Opens `path` in VS Code at `line` and `column`, both 1-based.
    ///
    /// # Errors
    /// [`OpenFailure::VsCodeNotFound`] when no VS Code is installed where it
    /// is looked for, else why it would not start.
    fn vscode(&self, path: &Path, line: u32, column: u32) -> Result<(), OpenFailure>;

    /// Opens `path` with the app the OS has registered for it, which asks
    /// the user to pick one for a type it has none for; a folder opens in
    /// the file manager.
    ///
    /// # Errors
    /// When the OS refused to open it.
    fn default_app(&self, path: &Path) -> Result<(), String>;

    /// Shows `path` selected in its folder in the file manager.
    ///
    /// # Errors
    /// When the file manager would not start.
    fn reveal(&self, path: &Path) -> Result<(), String>;

    /// The hosts behind the network drives mapped right now.
    fn mapped_unc_hosts(&self) -> Vec<String>;

    /// Whether Windows counts files of type `extension`, given with its
    /// leading dot (`.exe`), a risk to open.
    fn is_dangerous_type(&self, extension: &str) -> bool;

    /// `reading` canonicalized when it exists, else `None`; the error says
    /// why an existing reading could not be canonicalized. Only asked about
    /// a reading on a network host once that host passed the allowlist.
    fn existing(&self, reading: &Path) -> Option<Result<PathBuf, String>>;
}

/// Why an open did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenFailure {
    /// No VS Code command was found to start.
    VsCodeNotFound,
    /// What went wrong, for the user.
    Failed(String),
}

/// The opener the client runs with: the browser, VS Code and the shell.
pub struct SystemOpener;

impl Opener for SystemOpener {
    fn url(&self, url: &str) -> Result<(), String> {
        shell_open(OsStr::new(url)).map_err(|err| format!("open {url}: {err}"))
    }

    fn vscode(&self, path: &Path, line: u32, column: u32) -> Result<(), OpenFailure> {
        spawn_vscode(path, line, column)
    }

    fn default_app(&self, path: &Path) -> Result<(), String> {
        if path.is_dir() {
            open_folder(path)
        } else {
            shell_open(path.as_os_str()).map_err(|err| format!("open {}: {err}", path.display()))
        }
    }

    fn reveal(&self, path: &Path) -> Result<(), String> {
        reveal_in_folder(path)
    }

    fn mapped_unc_hosts(&self) -> Vec<String> {
        mapped_unc_hosts()
    }

    fn is_dangerous_type(&self, extension: &str) -> bool {
        dangerous_file_type(extension)
    }

    fn existing(&self, reading: &Path) -> Option<Result<PathBuf, String>> {
        existing_on_disk(reading)
    }
}

/// How a resolved link path is opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAction {
    /// The link carried a `:line[:col]`, which only an editor can honour.
    VsCode {
        path: PathBuf,
        line: u32,
        column: Option<u32>,
    },
    /// A plain path, for the app the OS picks.
    DefaultApp(PathBuf),
}

/// One thing handed to the [`Opener`] on a background thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenJob {
    Url(String),
    VsCode {
        path: PathBuf,
        line: u32,
        column: u32,
    },
    /// A file for its default app. Unless the user `confirmed` it, a file
    /// that runs code comes back as [`JobOutcome::AskFirst`] unopened.
    DefaultApp {
        path: PathBuf,
        confirmed: bool,
    },
    Reveal(PathBuf),
}

/// How a job that did not fail ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    Opened,
    /// The file runs code and nobody said to run it yet: ask first.
    AskFirst(PathBuf),
}

impl OpenJob {
    /// Hands the job to `opener`. Blocks, so it runs off the UI thread.
    ///
    /// # Errors
    /// Why the opener did not open it.
    pub fn run(&self, opener: &dyn Opener) -> Result<JobOutcome, OpenFailure> {
        match self {
            Self::Url(url) => opener.url(url).map_err(OpenFailure::Failed),
            Self::VsCode { path, line, column } => opener.vscode(path, *line, *column),
            Self::DefaultApp {
                path,
                confirmed: false,
            } if runs_code(path, opener) => return Ok(JobOutcome::AskFirst(path.clone())),
            Self::DefaultApp { path, .. } => opener.default_app(path).map_err(OpenFailure::Failed),
            Self::Reveal(path) => opener.reveal(path).map_err(OpenFailure::Failed),
        }
        .map(|()| JobOutcome::Opened)
    }
}

/// What a path link came to once its readings were checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// A reading exists; this is how to open it.
    Open(OpenAction),
    /// No reading exists, for the reason given.
    NotFound(String),
    /// The link names a network host that is neither mapped nor listed, so
    /// nothing was looked up.
    Refused { host: String },
}

/// The folders a session's relative link paths resolve against: its current
/// folder, then its members' worktrees, then its own worktrees, blanks left
/// out and each folder once, in that order.
#[must_use]
pub fn base_dirs(session: &SessionSnapshot) -> Vec<String> {
    let all = session
        .current_cwd
        .iter()
        .chain(session.members.iter().map(|member| &member.worktree_path))
        .chain(session.worktree_paths.iter());
    let mut dirs: Vec<String> = Vec::new();
    for dir in all {
        if !dir.is_empty() && !dirs.contains(dir) {
            dirs.push(dir.clone());
        }
    }
    dirs
}

/// Accepts only `http://` and `https://` URLs with no whitespace or control
/// characters in them.
///
/// # Errors
/// Names what is wrong with `url`.
pub fn validate_http_url(url: &str) -> Result<&str, String> {
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("URL contains whitespace or control characters".to_owned());
    }
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err("only http:// and https:// URLs can be opened".to_owned());
    }
    Ok(url)
}

/// Whether `path`'s default action runs code: its extension is in
/// [`EXECUTABLE_EXTENSIONS`], or `opener` says Windows counts its type a risk.
#[must_use]
pub fn runs_code(path: &Path, opener: &dyn Opener) -> bool {
    let Some(ext) = path.extension().and_then(OsStr::to_str) else {
        return false;
    };
    EXECUTABLE_EXTENSIONS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(ext))
        || opener.is_dangerous_type(&format!(".{ext}"))
}

/// The host of a `\\host\…` or `//host/…` path.
#[must_use]
pub fn unc_host(path: &str) -> Option<&str> {
    let mut chars = path.chars();
    let separator = |c: Option<char>| matches!(c, Some('\\' | '/'));
    if !separator(chars.next()) || !separator(chars.next()) {
        return None;
    }
    let host = chars.as_str().split(['\\', '/']).next().unwrap_or_default();
    (!host.is_empty()).then_some(host)
}

/// Checks a path link's readings and picks how to open the first that
/// exists under `base_dirs`. A reading on a network host that is neither
/// listed under `unc_hosts` in the layout file in `ui_dir`, read afresh here,
/// nor behind a drive `opener` reports mapped refuses the link before any
/// reading is looked up. Touches the file system and the network, so it runs
/// off the UI thread.
#[must_use]
pub fn resolve_link(
    candidates: &[TerminalLinkCandidate],
    base_dirs: &[String],
    ui_dir: Option<&Path>,
    opener: &dyn Opener,
) -> Resolution {
    resolve_with(
        candidates,
        base_dirs,
        || ui_dir.map(load_unc_hosts).unwrap_or_default(),
        || opener.mapped_unc_hosts(),
        |reading| opener.existing(reading),
    )
}

/// [`resolve_link`] with the listed and mapped host lookups and the
/// existence check named.
fn resolve_with(
    candidates: &[TerminalLinkCandidate],
    base_dirs: &[String],
    listed_hosts: impl FnOnce() -> Vec<String>,
    mapped_hosts: impl FnOnce() -> Vec<String>,
    mut existing: impl FnMut(&Path) -> Option<Result<PathBuf, String>>,
) -> Resolution {
    let all: Vec<Vec<PathBuf>> = candidates
        .iter()
        .map(|candidate| readings(&candidate.path, base_dirs))
        .collect();
    if let Some(host) = refused_host(all.iter().flatten(), listed_hosts, mapped_hosts) {
        return Resolution::Refused { host };
    }
    let mut last_error = "no path candidates to open".to_owned();
    for (candidate, readings) in candidates.iter().zip(&all) {
        match first_existing(readings, &mut existing) {
            Some(Ok(path)) => return Resolution::Open(open_action(candidate, path)),
            Some(Err(err)) => last_error = err,
            None => last_error = format!("path does not exist: {}", candidate.path),
        }
    }
    Resolution::NotFound(last_error)
}

/// Every place `path` may be: itself when absolute, else joined to each of
/// `base_dirs` in turn. A relative path with no base dirs has none; it is
/// never read against the client's own working directory.
fn readings(path: &str, base_dirs: &[String]) -> Vec<PathBuf> {
    let requested = PathBuf::from(path);
    if requested.is_absolute() || unc_host(path).is_some() {
        return vec![requested];
    }
    base_dirs
        .iter()
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join(&requested))
        .collect()
}

/// The UNC allowlist: the first network host among `readings` that is
/// neither listed nor mapped. It runs on every reading, a base dir joined
/// with the link's path included, before any of them is looked up, so a
/// session folder on an unknown host refuses every relative reading under
/// it. The listed hosts are only read, and the mapped drives only asked
/// about, when a reading is on a network host.
///
/// Known limit: a symlink or junction under an allowed folder that points at
/// a network share is followed by the existence check without this check
/// seeing the host it leads to.
fn refused_host<'a>(
    readings: impl Iterator<Item = &'a PathBuf>,
    listed_hosts: impl FnOnce() -> Vec<String>,
    mapped_hosts: impl FnOnce() -> Vec<String>,
) -> Option<String> {
    let known = |hosts: &[String], host: &str| hosts.iter().any(|h| h.eq_ignore_ascii_case(host));
    let hosts: Vec<String> = readings
        .filter_map(|reading| unc_host(&reading.to_string_lossy()).map(str::to_owned))
        .collect();
    if hosts.is_empty() {
        return None;
    }
    let listed = listed_hosts();
    let unlisted: Vec<String> = hosts
        .into_iter()
        .filter(|host| !known(&listed, host))
        .collect();
    if unlisted.is_empty() {
        return None;
    }
    let mapped = mapped_hosts();
    unlisted.into_iter().find(|host| !known(&mapped, host))
}

/// The first of `readings` that `existing` finds, canonicalized; `None` when
/// none exists.
fn first_existing(
    readings: &[PathBuf],
    existing: &mut impl FnMut(&Path) -> Option<Result<PathBuf, String>>,
) -> Option<Result<PathBuf, String>> {
    readings.iter().find_map(|reading| existing(reading))
}

/// How to open `path`, the resolved reading of `candidate`.
fn open_action(candidate: &TerminalLinkCandidate, path: PathBuf) -> OpenAction {
    match candidate.line {
        Some(line) => OpenAction::VsCode {
            path,
            line,
            column: candidate.column,
        },
        None => OpenAction::DefaultApp(path),
    }
}

/// `reading` canonicalized when it exists on disk.
fn existing_on_disk(reading: &Path) -> Option<Result<PathBuf, String>> {
    if !reading.exists() {
        return None;
    }
    Some(
        reading
            .canonicalize()
            .map(|resolved| simplify_path(&resolved))
            .map_err(|err| format!("canonicalize {}: {err}", reading.display())),
    )
}

/// Strips the Windows verbatim (`\\?\`) prefix `canonicalize` puts on every
/// path, which VS Code and its language servers mishandle. The daemon has
/// its own copy in `crates/daemon/src/paths.rs`.
#[must_use]
pub fn simplify_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\")
            && rest.chars().nth(1) == Some(':')
        {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// The VS Code commands to try, in order: `code.cmd` then `code.exe` on
/// `PATH`, then the usual install folders.
fn vscode_commands() -> Vec<PathBuf> {
    let mut commands = Vec::new();
    #[cfg(windows)]
    {
        commands.extend(["code.cmd", "code.exe"].into_iter().filter_map(on_path));
        for (env_name, suffix) in [
            ("LOCALAPPDATA", r"Programs\Microsoft VS Code\Code.exe"),
            (
                "LOCALAPPDATA",
                r"Programs\Microsoft VS Code Insiders\Code - Insiders.exe",
            ),
            ("PROGRAMFILES", r"Microsoft VS Code\Code.exe"),
            ("PROGRAMFILES(X86)", r"Microsoft VS Code\Code.exe"),
        ] {
            if let Ok(root) = std::env::var(env_name)
                && !root.is_empty()
            {
                commands.push(PathBuf::from(root).join(suffix));
            }
        }
    }
    #[cfg(not(windows))]
    commands.push(PathBuf::from("code"));
    commands
}

/// `name` in the first folder on `PATH` that holds it.
#[cfg(windows)]
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Launches the first VS Code command that starts, at `path:line:column`.
/// A command that is not there is skipped; `code.cmd` goes through the
/// standard library's batch-file quoting, which refuses an argument it
/// cannot pass safely, and that refusal is a failure like any other.
fn spawn_vscode(path: &Path, line: u32, column: u32) -> Result<(), OpenFailure> {
    let target = format!("{}:{}:{}", path.display(), line.max(1), column.max(1));
    let mut failure = None;
    for command in vscode_commands() {
        let mut child = Command::new(&command);
        child.arg("-g").arg(&target);
        no_window(&mut child);
        match child.spawn() {
            Ok(_) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => failure = Some(format!("{}: {err}", command.display())),
        }
    }
    Err(failure.map_or(OpenFailure::VsCodeNotFound, |err| {
        OpenFailure::Failed(format!("failed to launch VS Code ({err})"))
    }))
}

/// Keeps a console child from flashing a console window.
#[cfg(windows)]
fn no_window(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_window(_: &mut Command) {}

#[cfg(windows)]
fn open_folder(path: &Path) -> Result<(), String> {
    let mut explorer = Command::new("explorer.exe");
    explorer.arg(path);
    no_window(&mut explorer);
    explorer
        .spawn()
        .map(drop)
        .map_err(|err| format!("open {} in Explorer: {err}", path.display()))
}

#[cfg(not(windows))]
fn open_folder(path: &Path) -> Result<(), String> {
    shell_open(path.as_os_str()).map_err(|err| format!("open {}: {err}", path.display()))
}

/// Opens Explorer on `path`'s folder with `path` selected.
#[cfg(windows)]
fn reveal_in_folder(path: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt as _;
    let mut explorer = Command::new("explorer.exe");
    // Explorer parses `/select,"<path>"` itself; the standard quoting would
    // wrap the whole switch in quotes, which it does not read as a switch.
    explorer.raw_arg(format!("/select,\"{}\"", path.display()));
    no_window(&mut explorer);
    explorer
        .spawn()
        .map(drop)
        .map_err(|err| format!("show {} in Explorer: {err}", path.display()))
}

#[cfg(not(windows))]
fn reveal_in_folder(path: &Path) -> Result<(), String> {
    let folder = path.parent().unwrap_or(path);
    shell_open(folder.as_os_str()).map_err(|err| format!("show {}: {err}", path.display()))
}

/// Holds this thread in a single-threaded COM apartment for a shell call,
/// as `ShellExecuteEx` asks of its callers, and leaves it on drop when it
/// entered one.
#[cfg(windows)]
struct ComApartment {
    entered: bool,
}

#[cfg(windows)]
impl ComApartment {
    fn enter() -> Self {
        use windows::Win32::System::Com::{
            COINIT, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
        };
        let flags = COINIT(COINIT_APARTMENTTHREADED.0 | COINIT_DISABLE_OLE1DDE.0);
        // SAFETY: the reserved argument is null and the flags are valid; a
        // success is balanced by `CoUninitialize` in `drop`.
        let result = unsafe { CoInitializeEx(None, flags) };
        if result.is_err() {
            tracing::debug!(
                ?result,
                "COM stays in the apartment this thread already has"
            );
        }
        Self {
            entered: result.is_ok(),
        }
    }
}

#[cfg(windows)]
impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.entered {
            // SAFETY: balances the successful `CoInitializeEx` in `enter`,
            // on the same thread.
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

/// `text` as a NUL-terminated wide string.
#[cfg(windows)]
fn wide(text: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// Whether Windows counts files of type `extension` (`.exe`) a risk to open,
/// by `AssocIsDangerous`: its own list, the type's `FTA_AlwaysUnsafe` flag
/// and whether Safer calls the type executable.
#[cfg(windows)]
fn dangerous_file_type(extension: &str) -> bool {
    use windows::Win32::UI::Shell::AssocIsDangerous;
    use windows::core::PCWSTR;
    let extension = wide(OsStr::new(extension));
    // SAFETY: a NUL-terminated wide string that outlives the call.
    unsafe { AssocIsDangerous(PCWSTR(extension.as_ptr())) }.as_bool()
}

#[cfg(not(windows))]
fn dangerous_file_type(_: &str) -> bool {
    false
}

/// Opens `target`, a file or a URL, through `ShellExecuteExW` with no verb,
/// which runs the type's default action and, for a type with none, the
/// "open with" picker. `SEE_MASK_NOASYNC` makes it finish the launch before
/// it returns, since this background thread leaves its COM apartment right
/// after and runs no message loop to finish it later.
#[cfg(windows)]
fn shell_open(target: &OsStr) -> Result<(), String> {
    use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;

    let file = wide(target);
    let size = u32::try_from(size_of::<SHELLEXECUTEINFOW>())
        .map_err(|err| format!("SHELLEXECUTEINFOW size: {err}"))?;
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size,
        fMask: SEE_MASK_NOASYNC,
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    let _apartment = ComApartment::enter();
    // SAFETY: `info` is sized and zeroed but for its file, a NUL-terminated
    // wide string that outlives the call; a null verb is the default action.
    unsafe { ShellExecuteExW(&raw mut info) }.map_err(|err| err.to_string())
}

#[cfg(not(windows))]
fn shell_open(target: &OsStr) -> Result<(), String> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(program)
        .arg(target)
        .spawn()
        .map(drop)
        .map_err(|err| err.to_string())
}

/// The host of every drive letter mapped to a network share right now.
#[cfg(windows)]
fn mapped_unc_hosts() -> Vec<String> {
    use windows::Win32::Foundation::NO_ERROR;
    use windows::Win32::NetworkManagement::WNet::WNetGetConnectionW;
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    use windows::core::{PCWSTR, PWSTR};
    const DRIVE_REMOTE: u32 = 4;

    // SAFETY: takes nothing; returns a bit mask of the drive letters.
    let drives = unsafe { GetLogicalDrives() };
    let mut hosts = Vec::new();
    for (bit, letter) in ('A'..='Z').enumerate() {
        if drives & (1 << bit) == 0 {
            continue;
        }
        let root = wide(OsStr::new(&format!("{letter}:\\")));
        // SAFETY: a NUL-terminated wide string that outlives the call.
        if unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) } != DRIVE_REMOTE {
            continue;
        }
        let local = wide(OsStr::new(&format!("{letter}:")));
        let mut remote = vec![0_u16; 1024];
        let mut len = u32::try_from(remote.len()).unwrap_or(0);
        // SAFETY: `remote` holds `len` wide characters, and it and `local`
        // outlive the call.
        let status = unsafe {
            WNetGetConnectionW(
                PCWSTR(local.as_ptr()),
                Some(PWSTR(remote.as_mut_ptr())),
                &raw mut len,
            )
        };
        if status != NO_ERROR {
            tracing::debug!(%letter, ?status, "network drive has no live connection");
            continue;
        }
        let end = remote.iter().position(|c| *c == 0).unwrap_or(remote.len());
        let unc = String::from_utf16_lossy(remote.get(..end).unwrap_or_default());
        if let Some(host) = unc_host(&unc) {
            hosts.push(host.to_owned());
        }
    }
    hosts
}

#[cfg(not(windows))]
fn mapped_unc_hosts() -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use protocol::SessionSnapshot;
    use serde_json::json;

    use super::{
        OpenAction, OpenFailure, Opener, Resolution, base_dirs, existing_on_disk, resolve_with,
        runs_code, simplify_path, unc_host, validate_http_url,
    };
    use crate::links::TerminalLinkCandidate;

    /// A fresh folder under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "rt-native-open-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            fs::create_dir_all(&path).expect("create the temp folder");
            Self(path)
        }

        fn base(&self) -> Vec<String> {
            vec![self.0.to_string_lossy().into_owned()]
        }

        /// Writes `name` under the folder; returns its resolved path.
        fn file(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            let parent = path.parent().expect("a file has a parent folder");
            fs::create_dir_all(parent).expect("create the file's folder");
            fs::write(&path, "x\n").expect("write the file");
            simplify_path(&path.canonicalize().expect("canonicalize the file"))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// An opener whose only answer is which file types Windows calls a
    /// risk; everything else is out of these tests' reach.
    struct DangerousTypes(&'static [&'static str]);

    impl Opener for DangerousTypes {
        fn url(&self, _: &str) -> Result<(), String> {
            Err("not in these tests".to_owned())
        }

        fn vscode(&self, _: &Path, _: u32, _: u32) -> Result<(), OpenFailure> {
            Err(OpenFailure::VsCodeNotFound)
        }

        fn default_app(&self, _: &Path) -> Result<(), String> {
            Err("not in these tests".to_owned())
        }

        fn reveal(&self, _: &Path) -> Result<(), String> {
            Err("not in these tests".to_owned())
        }

        fn mapped_unc_hosts(&self) -> Vec<String> {
            Vec::new()
        }

        fn is_dangerous_type(&self, extension: &str) -> bool {
            self.0
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        }

        fn existing(&self, _: &Path) -> Option<Result<PathBuf, String>> {
            None
        }
    }

    fn candidate(path: &str, line: Option<u32>, column: Option<u32>) -> TerminalLinkCandidate {
        TerminalLinkCandidate {
            path: path.to_owned(),
            line,
            column,
        }
    }

    fn hosts(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// An existence check that finds every reading, and records each one it
    /// was asked about.
    fn found(
        calls: &RefCell<Vec<PathBuf>>,
    ) -> impl FnMut(&Path) -> Option<Result<PathBuf, String>> {
        move |reading| {
            calls.borrow_mut().push(reading.to_path_buf());
            Some(Ok(reading.to_path_buf()))
        }
    }

    /// `candidates` resolved on the real disk, with no network host listed
    /// or mapped.
    fn on_disk(candidates: &[TerminalLinkCandidate], base_dirs: &[String]) -> Resolution {
        resolve_with(candidates, base_dirs, Vec::new, Vec::new, existing_on_disk)
    }

    /// `path` resolved on the real disk under `base_dirs`, as a file for its
    /// default app.
    fn file_on_disk(path: &str, base_dirs: &[String]) -> Result<PathBuf, String> {
        match on_disk(&[candidate(path, None, None)], base_dirs) {
            Resolution::Open(OpenAction::DefaultApp(path)) => Ok(path),
            other => Err(format!("{other:?}")),
        }
    }

    #[test]
    fn urls_other_than_http_and_https_are_refused() {
        assert!(validate_http_url("https://example.com/path").is_ok());
        assert!(validate_http_url("HTTP://example.com/path").is_ok());
        assert!(validate_http_url("file:///C:/temp/a.txt").is_err());
        assert!(validate_http_url("javascript:alert(1)").is_err());
        assert!(validate_http_url("https://example.com/a b").is_err());
        assert!(validate_http_url(" https://example.com").is_err());
        assert!(validate_http_url("https://example.com/\u{7}").is_err());
        assert!(validate_http_url("https://example.com/\n").is_err());
    }

    #[test]
    fn a_relative_path_resolves_under_a_base_dir() {
        let root = TempDir::new("base");
        let expected = root.file("src/main.rs");
        let resolved = file_on_disk("src/main.rs", &root.base()).expect("resolves");
        assert_eq!(resolved, expected);
        assert!(
            !resolved.to_string_lossy().starts_with(r"\\?\"),
            "the verbatim prefix is stripped: {}",
            resolved.display()
        );
    }

    #[test]
    fn a_later_base_dir_is_tried_when_the_first_lacks_the_path() {
        let empty = TempDir::new("empty");
        let root = TempDir::new("second");
        let expected = root.file("notes.md");
        let dirs = [String::new(), empty.base().concat(), root.base().concat()];
        assert_eq!(file_on_disk("notes.md", &dirs), Ok(expected));
    }

    #[test]
    fn a_missing_path_does_not_resolve() {
        let root = TempDir::new("missing");
        assert_eq!(
            on_disk(&[candidate("missing.rs", None, None)], &root.base()),
            Resolution::NotFound("path does not exist: missing.rs".to_owned())
        );
        assert!(matches!(
            on_disk(&[], &root.base()),
            Resolution::NotFound(_)
        ));
    }

    #[test]
    fn with_no_base_dirs_a_relative_path_resolves_to_nothing() {
        let calls = RefCell::new(Vec::new());
        let resolution = resolve_with(
            &[candidate("Cargo.toml", None, None)],
            &[],
            Vec::new,
            Vec::new,
            found(&calls),
        );
        assert!(
            matches!(resolution, Resolution::NotFound(_)),
            "{resolution:?}"
        );
        assert_eq!(calls.borrow().len(), 0, "no reading to look up");
        assert!(
            Path::new("Cargo.toml").exists(),
            "the test runs where the path would exist"
        );
        assert!(
            matches!(
                on_disk(&[candidate("Cargo.toml", None, None)], &[]),
                Resolution::NotFound(_)
            ),
            "never read against the client's own folder"
        );
    }

    #[test]
    fn the_stitched_candidate_wins_when_it_exists() {
        let root = TempDir::new("stitched");
        let expected = root.file("notes-file.txt");
        root.file("notes-file.");
        let resolution = on_disk(
            &[
                candidate("notes-file.txt", None, None),
                candidate("notes-file.", None, None),
            ],
            &root.base(),
        );
        assert_eq!(
            resolution,
            Resolution::Open(OpenAction::DefaultApp(expected))
        );
    }

    #[test]
    fn the_fragment_opens_when_the_stitch_does_not_exist() {
        let root = TempDir::new("fragment");
        let expected = root.file("notes-file.txt");
        let resolution = on_disk(
            &[
                candidate("notes-file.txtdone", None, None),
                candidate("notes-file.txt", None, None),
            ],
            &root.base(),
        );
        assert_eq!(
            resolution,
            Resolution::Open(OpenAction::DefaultApp(expected))
        );
    }

    #[test]
    fn a_line_ref_routes_to_vscode() {
        let root = TempDir::new("lineref");
        let expected = root.file("main.rs");
        let resolution = on_disk(&[candidate("main.rs", Some(42), Some(7))], &root.base());
        assert_eq!(
            resolution,
            Resolution::Open(OpenAction::VsCode {
                path: expected,
                line: 42,
                column: Some(7),
            })
        );
    }

    #[test]
    fn an_absolute_path_ignores_the_base_dirs() {
        let root = TempDir::new("absolute");
        let expected = root.file("abs.txt");
        let other = TempDir::new("other");
        let absolute = expected.to_string_lossy().into_owned();
        assert_eq!(file_on_disk(&absolute, &other.base()), Ok(expected));
    }

    #[test]
    fn base_dirs_keep_the_first_of_each_folder_in_order() {
        let session: SessionSnapshot = serde_json::from_value(json!({
            "id": "s",
            "label": "s",
            "kind": "workspace",
            "members": [
                { "repo_id": "a", "repo_name": "a", "branch": "b", "worktree_path": "C:/wt/a" },
                { "repo_id": "b", "repo_name": "b", "branch": "b", "worktree_path": "" },
            ],
            "status": "idle",
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": null },
            "recent_actions": [],
            "agent": "claude",
            "current_cwd": "C:/wt/a",
            "worktree_paths": ["C:/wt/c", "C:/wt/a"],
        }))
        .expect("session fixture");
        assert_eq!(base_dirs(&session), ["C:/wt/a", "C:/wt/c"]);
    }

    #[test]
    fn executables_are_known_by_extension_in_any_case() {
        let no_windows_list = DangerousTypes(&[]);
        for name in [
            "run.bat",
            "RUN.BAT",
            "setup.exe",
            "tool.Ps1",
            "app.appref-ms",
            "deploy.application",
            "link.lnk",
            "site.url",
            "script.py",
            "install.sh",
            "Package.MSIX",
            "setting.settingcontent-ms",
        ] {
            assert!(
                runs_code(Path::new(name), &no_windows_list),
                "{name} asks first"
            );
        }
        for name in [
            "notes.md",
            "notes.txt",
            "main.rs",
            "bat",
            "archive.zip",
            "script.bat.txt",
        ] {
            assert!(
                !runs_code(Path::new(name), &no_windows_list),
                "{name} opens at once"
            );
        }
    }

    #[test]
    fn a_type_windows_calls_dangerous_asks_first() {
        let windows = DangerousTypes(&[".foo"]);
        assert!(runs_code(Path::new("thing.FOO"), &windows));
        assert!(!runs_code(Path::new("notes.txt"), &windows));
        assert!(!runs_code(Path::new("Makefile"), &windows));
    }

    #[cfg(windows)]
    #[test]
    fn windows_calls_exe_dangerous_and_txt_not() {
        assert!(super::dangerous_file_type(".exe"));
        assert!(!super::dangerous_file_type(".txt"));
    }

    #[test]
    fn a_unc_host_is_read_from_either_slash() {
        assert_eq!(unc_host(r"\\files\share\a.txt"), Some("files"));
        assert_eq!(unc_host("//files/share/a.txt"), Some("files"));
        assert_eq!(unc_host(r"\\files"), Some("files"));
        assert_eq!(unc_host(r"\\\share"), None);
        assert_eq!(unc_host(r"C:\files\a.txt"), None);
        assert_eq!(unc_host("docs/a.txt"), None);
    }

    #[test]
    fn a_host_behind_a_mapped_drive_is_allowed() {
        let calls = RefCell::new(Vec::new());
        let resolution = resolve_with(
            &[candidate(r"\\Files\share\a.txt", None, None)],
            &[],
            Vec::new,
            || hosts(&["files"]),
            found(&calls),
        );
        assert_eq!(
            resolution,
            Resolution::Open(OpenAction::DefaultApp(PathBuf::from(
                r"\\Files\share\a.txt"
            )))
        );
        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn a_listed_host_is_allowed_without_asking_about_drives() {
        let calls = RefCell::new(Vec::new());
        let asked = Cell::new(false);
        let resolution = resolve_with(
            &[candidate("//nas/share/a.txt", None, None)],
            &[],
            || hosts(&["NAS"]),
            || {
                asked.set(true);
                Vec::new()
            },
            found(&calls),
        );
        assert!(matches!(resolution, Resolution::Open(_)), "{resolution:?}");
        assert_eq!(calls.borrow().len(), 1);
        assert!(!asked.get(), "a listed host needs no drive lookup");
    }

    #[test]
    fn an_unknown_host_is_refused_before_the_resolver() {
        let calls = RefCell::new(Vec::new());
        let resolution = resolve_with(
            &[
                candidate(r"\\evil\share\payload.txt", None, None),
                candidate(r"\\evil\share\pay", None, None),
            ],
            &[],
            || hosts(&["nas"]),
            || hosts(&["files"]),
            found(&calls),
        );
        assert_eq!(
            resolution,
            Resolution::Refused {
                host: "evil".to_owned()
            }
        );
        assert_eq!(
            calls.borrow().len(),
            0,
            "a refused host never reaches the resolver"
        );
    }

    #[test]
    fn a_relative_path_under_an_unknown_host_folder_is_refused() {
        let calls = RefCell::new(Vec::new());
        let resolution = resolve_with(
            &[candidate("docs/a.md", None, None)],
            &hosts(&[r"C:\work", r"\\evil\share"]),
            || hosts(&["nas"]),
            Vec::new,
            found(&calls),
        );
        assert_eq!(
            resolution,
            Resolution::Refused {
                host: "evil".to_owned()
            }
        );
        assert_eq!(
            calls.borrow().len(),
            0,
            "no reading is looked up, not even the local one"
        );
    }

    #[test]
    fn a_relative_path_under_an_allowed_host_folder_resolves() {
        let calls = RefCell::new(Vec::new());
        let resolution = resolve_with(
            &[candidate("docs/a.md", None, None)],
            &hosts(&[r"\\nas\share"]),
            || hosts(&["NAS"]),
            Vec::new,
            found(&calls),
        );
        let expected = PathBuf::from(r"\\nas\share").join("docs/a.md");
        assert_eq!(
            resolution,
            Resolution::Open(OpenAction::DefaultApp(expected.clone()))
        );
        assert_eq!(*calls.borrow(), [expected]);
    }

    #[test]
    fn a_local_path_needs_no_host_lookup() {
        let calls = RefCell::new(Vec::new());
        let listed = Cell::new(false);
        let mapped = Cell::new(false);
        let resolution = resolve_with(
            &[candidate("docs/a.md", None, None)],
            &hosts(&[r"C:\work"]),
            || {
                listed.set(true);
                Vec::new()
            },
            || {
                mapped.set(true);
                Vec::new()
            },
            found(&calls),
        );
        assert!(matches!(resolution, Resolution::Open(_)), "{resolution:?}");
        assert!(!listed.get(), "a local path reads no host list");
        assert!(!mapped.get(), "a local path needs no drive lookup");
    }

    #[cfg(windows)]
    #[test]
    fn simplify_path_strips_the_verbatim_prefixes() {
        assert_eq!(
            simplify_path(&PathBuf::from(r"\\?\C:\repo\src\main.rs")),
            PathBuf::from(r"C:\repo\src\main.rs")
        );
        assert_eq!(
            simplify_path(&PathBuf::from(r"\\?\UNC\server\share\file.txt")),
            PathBuf::from(r"\\server\share\file.txt")
        );
        assert_eq!(
            simplify_path(&PathBuf::from(r"\\?\Volume{abc}\file.txt")),
            PathBuf::from(r"\\?\Volume{abc}\file.txt")
        );
    }
}
