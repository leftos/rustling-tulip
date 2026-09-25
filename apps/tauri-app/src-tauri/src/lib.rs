use daemon_client::{ClientIdentity, config_dir};
use protocol::DaemonHandshake;
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use tauri::utils::config::Color;
use tauri::{Manager, Runtime, Theme, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::DialogExt as _;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod autostart;
mod remote;

const APP_BACKGROUND_COLOR: Color = Color(8, 9, 11, 255);

/// Parse a boolean-ish env var. Set + non-empty + not literally "0" counts as
/// true; everything else is false. Used for opt-in harness toggles where the
/// presence of any meaningful value should enable the flag.
fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0")
}

fn apply_e2e_window_options<R, M>(
    builder: WebviewWindowBuilder<'_, R, M>,
) -> WebviewWindowBuilder<'_, R, M>
where
    R: Runtime,
    M: Manager<R>,
{
    if env_flag("RUSTLING_TULIP_OFFSCREEN_WINDOW") {
        builder
            .visible(false)
            .focused(false)
            .position(-32_000.0, -32_000.0)
            .skip_taskbar(true)
    } else {
        builder
    }
}

fn apply_window_appearance<R, M>(
    builder: WebviewWindowBuilder<'_, R, M>,
) -> WebviewWindowBuilder<'_, R, M>
where
    R: Runtime,
    M: Manager<R>,
{
    builder
        .theme(Some(Theme::Dark))
        .background_color(APP_BACKGROUND_COLOR)
}

/// Render a `daemon_client` error as the string the frontend sees, with its
/// full context chain.
#[expect(
    clippy::needless_pass_by_value,
    reason = "used as a map_err adapter, which hands the error over by value"
)]
fn error_text(err: anyhow::Error) -> String {
    format!("{err:#}")
}

#[tauri::command]
async fn ensure_daemon_started() -> Result<DaemonHandshake, String> {
    daemon_client::ensure_running(daemon_client::RetirePolicy::RetireStale)
        .await
        .map_err(error_text)
}

#[tauri::command]
async fn pick_directory(
    app: tauri::AppHandle,
    default_path: Option<String>,
) -> Result<Option<String>, String> {
    let mut builder = app.dialog().file();
    if let Some(p) = default_path {
        builder = builder.set_directory(p);
    }
    let path = builder.blocking_pick_folder();
    Ok(path.and_then(|p| {
        p.into_path()
            .ok()
            .map(|pb| pb.to_string_lossy().into_owned())
    }))
}

#[tauri::command]
async fn pick_file(
    app: tauri::AppHandle,
    default_path: Option<String>,
    extensions: Option<Vec<String>>,
    filter_name: Option<String>,
) -> Result<Option<String>, String> {
    let mut builder = app.dialog().file();
    if let Some(p) = default_path {
        builder = builder.set_directory(p);
    }
    if let Some(exts) = extensions
        && !exts.is_empty()
    {
        let name = filter_name.as_deref().unwrap_or("Files");
        let refs: Vec<&str> = exts.iter().map(String::as_str).collect();
        builder = builder.add_filter(name, &refs);
    }
    let path = builder.blocking_pick_file();
    Ok(path.and_then(|p| {
        p.into_path()
            .ok()
            .map(|pb| pb.to_string_lossy().into_owned())
    }))
}

/// Resolve the per-user app log directory and ensure it exists. Returns the
/// path to `app.log` under it. Errors surface as `Result<_, String>` so they
/// flow back through Tauri's invoke pipeline.
fn app_log_path() -> Result<PathBuf, String> {
    let log_dir = config_dir().map_err(error_text)?.join("logs");
    std::fs::create_dir_all(&log_dir).map_err(|e| e.to_string())?;
    Ok(log_dir.join("app.log"))
}

/// Rotate `app.log` to `app.log.old` and start a fresh, empty `app.log`.
/// Called once on app startup so each launch logs to a clean file while the
/// previous launch's log survives one more generation — the record of what a
/// misbehaving app instance did (a failed reconnect, an exit-dialog click) is
/// only ever needed AFTER the user has restarted the app, which is exactly
/// when truncate-on-start used to destroy it. The daemon mirrors this on its
/// side for `daemon.log`.
fn rotate_app_log() -> Result<(), String> {
    let path = app_log_path()?;
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
        let old = path.with_extension("log.old");
        // Windows rename fails when the target exists; drop the older
        // generation first (best-effort — rename reports the definitive
        // error).
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&path, &old)
            .map_err(|e| format!("rotate {} -> {}: {e}", path.display(), old.display()))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| format!("truncate {}: {e}", path.display()))?;
    Ok(())
}

/// Append a single timestamped line to `app.log`. Frontend code invokes this
/// from key paths (especially shutdown) so we have something to look at when
/// the UI hangs. The file is shared with the daemon's logs in the same
/// directory but kept separate so each side can be inspected in isolation.
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri invoke handlers must own their args — JSON deserializes into String"
)]
#[tauri::command]
fn log_message(level: String, message: String) -> Result<(), String> {
    let path = app_log_path()?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    writeln!(file, "{ts} {level} {message}").map_err(|e| e.to_string())?;
    Ok(())
}

/// Reads the OS clipboard text directly via arboard (the Rust side), bypassing
/// the `WebView2` clipboard bridge. The terminal paste path uses this as the
/// paste source: `WebView2`'s `clipboardData.getData` can intermittently return
/// truncated text for large or delayed-render payloads (dropping the middle of
/// a paste), and a native read goes straight to the Win32 clipboard. The same
/// value feeds the paste-fidelity logging so the sources stay comparable. An
/// empty or non-text clipboard yields an empty string rather than an error.
#[tauri::command]
fn read_clipboard_text() -> Result<String, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    match clipboard.get_text() {
        Ok(text) => Ok(text),
        Err(arboard::Error::ContentNotAvailable) => Ok(String::new()),
        Err(err) => Err(err.to_string()),
    }
}

/// Paths the daemon-status footer + troubleshooting flyout exposes to the
/// user (open log, reveal config dir, copy handshake path, etc). All four
/// derive from the same `config_dir()` so we return them as one struct
/// rather than minting four invoke commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonPaths {
    pub config_dir: String,
    pub daemon_log: String,
    pub app_log: String,
    pub handshake_file: String,
}

/// Stable per-install client identity for per-client tab layouts; see
/// [`daemon_client::client_identity`].
#[tauri::command]
fn get_client_identity() -> Result<ClientIdentity, String> {
    daemon_client::client_identity("client-id").map_err(error_text)
}

#[tauri::command]
fn daemon_paths() -> Result<DaemonPaths, String> {
    let cfg = config_dir().map_err(error_text)?;
    let handshake = daemon_client::handshake_file().map_err(error_text)?;
    let logs = cfg.join("logs");
    Ok(DaemonPaths {
        config_dir: cfg.to_string_lossy().into_owned(),
        daemon_log: logs.join("daemon.log").to_string_lossy().into_owned(),
        app_log: logs.join("app.log").to_string_lossy().into_owned(),
        handshake_file: handshake.to_string_lossy().into_owned(),
    })
}

/// Force-stop the running daemon. Used by the footer's "Stop daemon" action,
/// which may fire while the WS is already closed (e.g. the footer surfaced
/// "connecting…" and the user gave up waiting); see [`daemon_client::stop`].
#[tauri::command]
async fn stop_daemon() -> Result<(), String> {
    daemon_client::stop().await.map_err(error_text)
}

/// Reveal a directory in the OS file manager (Explorer on Windows, Finder on
/// macOS, xdg-open on Linux). The path is validated to exist before any
/// shell process is spawned so we don't dispatch on attacker-controlled
/// strings — only paths that the daemon already vetted as repo roots reach
/// this command via the sidebar UI.
#[tauri::command]
async fn reveal_in_explorer(path: String) -> Result<(), String> {
    let pb = PathBuf::from(&path);
    if !pb.exists() {
        return Err(format!("path does not exist: {path}"));
    }
    open_dir_in_file_manager(&pb)
}

/// Hand a directory to the OS file manager. Shared by `reveal_in_explorer` and
/// by the terminal-link opener, which uses it instead of the shell's default
/// verb because `SHOpenFolderAndSelectItems` reveals a folder rather than
/// opening it.
fn open_dir_in_file_manager(pb: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("explorer.exe")
            .arg(pb)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(pb)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(pb)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
async fn open_url(url: String) -> Result<(), String> {
    let validated = validate_http_url(&url)?;
    open_with_system_handler(validated)
}

/// One reading of a path a terminal link was detected in. A link stitched
/// across a hard row break sends the merged path first and the fragments that
/// were actually on screen after it.
#[derive(Debug, serde::Deserialize)]
struct TerminalPathCandidate {
    path: String,
    line: Option<u32>,
    column: Option<u32>,
}

/// How a resolved terminal path gets opened.
#[derive(Debug, PartialEq, Eq)]
enum TerminalOpenAction {
    /// The link carried a `:line[:col]` reference. No OS handler can honor
    /// one, so the suffix is itself the request for an editor.
    VsCode {
        path: PathBuf,
        line: u32,
        column: Option<u32>,
    },
    /// A plain path: the OS picks the app, including its own "how do you want
    /// to open this file?" picker for an unassociated type.
    DefaultApp(PathBuf),
}

#[tauri::command]
async fn open_terminal_path(
    candidates: Vec<TerminalPathCandidate>,
    base_dirs: Vec<String>,
) -> Result<(), String> {
    match select_terminal_open(&candidates, &base_dirs)? {
        TerminalOpenAction::VsCode { path, line, column } => {
            spawn_vscode(&path, Some(line), column)
        }
        TerminalOpenAction::DefaultApp(path) => open_with_default_app(&path),
    }
}

/// Take the first candidate that exists on disk — the longest reading wins,
/// so an over-eager wrap stitch degrades to the fragment that was on screen.
fn select_terminal_open(
    candidates: &[TerminalPathCandidate],
    base_dirs: &[String],
) -> Result<TerminalOpenAction, String> {
    let mut last_error = "no path candidates to open".to_string();
    for candidate in candidates {
        match resolve_existing_terminal_path(&candidate.path, base_dirs) {
            Ok(resolved) => {
                return Ok(match candidate.line {
                    Some(line) => TerminalOpenAction::VsCode {
                        path: resolved,
                        line,
                        column: candidate.column,
                    },
                    None => TerminalOpenAction::DefaultApp(resolved),
                });
            }
            Err(err) => last_error = err,
        }
    }
    Err(last_error)
}

/// Open a path with whatever the OS has registered for it. Files go through
/// the opener plugin, which is a real `ShellExecuteExW` with the default verb
/// on Windows; directories go to the file manager.
fn open_with_default_app(path: &Path) -> Result<(), String> {
    if path.is_dir() {
        return open_dir_in_file_manager(path);
    }
    tauri_plugin_opener::open_path(path, None::<&str>)
        .map_err(|e| format!("open {}: {e}", path.display()))
}

/// Open one or more folders/files in a single VS Code window. The first path
/// is the primary target; remaining paths are added via `--add`, which gives
/// the user a multi-root workspace window when called with multiple repo
/// paths (the workspace context-menu fallback when no `.code-workspace`
/// file is linked).
#[tauri::command]
async fn open_folders_in_vscode(paths: Vec<String>) -> Result<(), String> {
    if paths.is_empty() {
        return Err("no paths to open".to_string());
    }
    let mut resolved = Vec::with_capacity(paths.len());
    for p in &paths {
        let pb = PathBuf::from(p);
        if !pb.exists() {
            return Err(format!("path does not exist: {p}"));
        }
        resolved.push(
            pb.canonicalize()
                .map(|p| simplify_path(&p))
                .map_err(|e| format!("canonicalize {p}: {e}"))?,
        );
    }
    spawn_vscode_multi(&resolved)
}

fn validate_http_url(url: &str) -> Result<&str, String> {
    if url.trim() != url || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("URL contains whitespace or control characters".to_string());
    }
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err("only http:// and https:// URLs can be opened".to_string());
    }
    Ok(url)
}

fn open_with_system_handler(target: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        Command::new("rundll32.exe")
            .arg("url.dll,FileProtocolHandler")
            .arg(target)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("open URL: {e}"))?;
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(target)
            .spawn()
            .map_err(|e| format!("open URL: {e}"))?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(target)
            .spawn()
            .map_err(|e| format!("open URL: {e}"))?;
    }
    Ok(())
}

fn resolve_existing_terminal_path(path: &str, base_dirs: &[String]) -> Result<PathBuf, String> {
    let requested = PathBuf::from(path);
    let candidates = if requested.is_absolute() {
        vec![requested]
    } else {
        let mut out: Vec<PathBuf> = base_dirs
            .iter()
            .filter(|dir| !dir.is_empty())
            .map(|dir| PathBuf::from(dir).join(&requested))
            .collect();
        if out.is_empty()
            && let Ok(cwd) = std::env::current_dir()
        {
            out.push(cwd.join(&requested));
        }
        out
    };

    for candidate in candidates {
        if candidate.exists() {
            return candidate
                .canonicalize()
                .map(|p| simplify_path(&p))
                .map_err(|e| format!("canonicalize {}: {e}", candidate.display()));
        }
    }
    Err(format!("path does not exist: {path}"))
}

/// Strip the Windows verbatim (`\\?\`) prefix from a canonicalized path.
/// `std::fs::canonicalize` on Windows always returns the verbatim form, which
/// the filesystem accepts but most other tooling does not — VS Code opens the
/// workspace under that literal path, and language-server clients (notably
/// Ruff) then build `file://` URIs by URL-encoding the `?`, producing
/// `file://%3F/...` which their URI parsers reject as an invalid IDN.
///
/// The daemon crate has the authoritative copy in `crates/daemon/src/paths.rs`;
/// this is duplicated here because the Tauri-app crate intentionally stays
/// decoupled from the daemon crate (it's a thin client). Keep the two in sync.
#[must_use]
fn simplify_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && rest.chars().nth(1) == Some(':')
        {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

fn spawn_vscode(path: &Path, line: Option<u32>, column: Option<u32>) -> Result<(), String> {
    let target = if let Some(line) = line {
        let column = column.unwrap_or(1).max(1);
        format!("{}:{}:{column}", path.display(), line.max(1))
    } else {
        path.to_string_lossy().into_owned()
    };
    let mut last_error = None;
    for command in vscode_commands() {
        let mut child = Command::new(&command);
        if line.is_some() {
            child.arg("-g");
        }
        child.arg(&target);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            child.creation_flags(CREATE_NO_WINDOW);
        }
        match child.spawn() {
            Ok(_) => return Ok(()),
            Err(err) => last_error = Some(format!("{}: {err}", command.display())),
        }
    }
    Err(format!(
        "failed to launch VS Code{}",
        last_error.map_or_else(String::new, |err| format!(" ({err})"))
    ))
}

fn spawn_vscode_multi(paths: &[PathBuf]) -> Result<(), String> {
    let Some((first, rest)) = paths.split_first() else {
        return Err("no paths to open".to_string());
    };
    let mut last_error = None;
    for command in vscode_commands() {
        let mut child = Command::new(&command);
        child.arg(first);
        for extra in rest {
            child.arg("--add").arg(extra);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            child.creation_flags(CREATE_NO_WINDOW);
        }
        match child.spawn() {
            Ok(_) => return Ok(()),
            Err(err) => last_error = Some(format!("{}: {err}", command.display())),
        }
    }
    Err(format!(
        "failed to launch VS Code{}",
        last_error.map_or_else(String::new, |err| format!(" ({err})"))
    ))
}

fn vscode_commands() -> Vec<PathBuf> {
    let mut commands = Vec::new();
    #[cfg(windows)]
    {
        commands.push(PathBuf::from("code.exe"));
        commands.push(PathBuf::from("Code.exe"));
        for (env_name, suffix) in [
            ("LOCALAPPDATA", "Programs\\Microsoft VS Code\\Code.exe"),
            (
                "LOCALAPPDATA",
                "Programs\\Microsoft VS Code Insiders\\Code - Insiders.exe",
            ),
            ("PROGRAMFILES", "Microsoft VS Code\\Code.exe"),
            ("PROGRAMFILES(X86)", "Microsoft VS Code\\Code.exe"),
        ] {
            if let Ok(root) = std::env::var(env_name)
                && !root.is_empty()
            {
                commands.push(PathBuf::from(root).join(suffix));
            }
        }
    }
    #[cfg(not(windows))]
    {
        commands.push(PathBuf::from("code"));
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::{
        TerminalOpenAction, TerminalPathCandidate, resolve_existing_terminal_path,
        select_terminal_open, simplify_path, validate_http_url,
    };
    use std::fs;
    #[cfg(windows)]
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn validate_http_url_accepts_http_and_https_only() {
        assert!(validate_http_url("https://example.com/path").is_ok());
        assert!(validate_http_url("http://example.com/path").is_ok());
        assert!(validate_http_url("file:///C:/temp/a.txt").is_err());
        assert!(validate_http_url("https://example.com/a b").is_err());
    }

    #[test]
    fn resolve_existing_terminal_path_uses_base_directory() -> Result<(), String> {
        let root = unique_temp_dir("base")?;
        let nested = root.join("src");
        let file = nested.join("main.rs");
        fs::create_dir_all(&nested).map_err(|e| e.to_string())?;
        fs::write(&file, "fn main() {}\n").map_err(|e| e.to_string())?;

        let resolved =
            resolve_existing_terminal_path("src/main.rs", &[root.to_string_lossy().into_owned()])?;

        let expected = simplify_path(&file.canonicalize().map_err(|e| e.to_string())?);
        fs::remove_dir_all(&root).map_err(|e| e.to_string())?;
        assert_eq!(resolved, expected);
        #[cfg(windows)]
        {
            // VS Code launched with a `\\?\…` path produces `file://%3F/…`
            // URIs that break LSP clients (Ruff in particular). The resolved
            // path must be in normal form before it hits the command line.
            assert!(
                !resolved.to_string_lossy().starts_with(r"\\?\"),
                "resolved path retains verbatim prefix: {}",
                resolved.display()
            );
        }
        Ok(())
    }

    #[test]
    fn resolve_existing_terminal_path_rejects_missing_paths() {
        let result = resolve_existing_terminal_path("missing.rs", &[]);
        assert!(result.is_err());
    }

    #[test]
    fn select_terminal_open_takes_the_stitched_candidate_when_it_exists() -> Result<(), String> {
        let root = unique_temp_dir("stitched")?;
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let file = root.join("notes-file.txt");
        fs::write(&file, "merged\n").map_err(|e| e.to_string())?;
        let base_dirs = [root.to_string_lossy().into_owned()];

        let action = select_terminal_open(
            &[
                candidate("notes-file.txt", None, None),
                candidate("notes-file.", None, None),
            ],
            &base_dirs,
        );

        let expected = simplify_path(&file.canonicalize().map_err(|e| e.to_string())?);
        fs::remove_dir_all(&root).map_err(|e| e.to_string())?;
        assert_eq!(action?, TerminalOpenAction::DefaultApp(expected));
        Ok(())
    }

    #[test]
    fn select_terminal_open_falls_back_to_the_fragment() -> Result<(), String> {
        let root = unique_temp_dir("fragment")?;
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let file = root.join("notes-file.txt");
        fs::write(&file, "fragment\n").map_err(|e| e.to_string())?;
        let base_dirs = [root.to_string_lossy().into_owned()];

        let action = select_terminal_open(
            &[
                candidate("notes-file.txtdone", None, None),
                candidate("notes-file.txt", None, None),
            ],
            &base_dirs,
        );

        let expected = simplify_path(&file.canonicalize().map_err(|e| e.to_string())?);
        fs::remove_dir_all(&root).map_err(|e| e.to_string())?;
        assert_eq!(action?, TerminalOpenAction::DefaultApp(expected));
        Ok(())
    }

    #[test]
    fn select_terminal_open_routes_a_line_ref_to_vscode() -> Result<(), String> {
        let root = unique_temp_dir("lineref")?;
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let file = root.join("main.rs");
        fs::write(&file, "fn main() {}\n").map_err(|e| e.to_string())?;
        let base_dirs = [root.to_string_lossy().into_owned()];

        let action = select_terminal_open(&[candidate("main.rs", Some(42), Some(7))], &base_dirs);

        let expected = simplify_path(&file.canonicalize().map_err(|e| e.to_string())?);
        fs::remove_dir_all(&root).map_err(|e| e.to_string())?;
        assert_eq!(
            action?,
            TerminalOpenAction::VsCode {
                path: expected,
                line: 42,
                column: Some(7),
            }
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn simplify_path_strips_drive_verbatim_prefix() {
        let out = simplify_path(Path::new(r"\\?\C:\Users\foo\repo"));
        assert_eq!(out, PathBuf::from(r"C:\Users\foo\repo"));
    }

    #[cfg(windows)]
    #[test]
    fn simplify_path_converts_unc_verbatim() {
        let out = simplify_path(Path::new(r"\\?\UNC\server\share\dir"));
        assert_eq!(out, PathBuf::from(r"\\server\share\dir"));
    }

    #[cfg(windows)]
    #[test]
    fn simplify_path_leaves_normal_paths_alone() {
        let out = simplify_path(Path::new(r"C:\Users\foo\repo"));
        assert_eq!(out, PathBuf::from(r"C:\Users\foo\repo"));
    }

    fn candidate(path: &str, line: Option<u32>, column: Option<u32>) -> TerminalPathCandidate {
        TerminalPathCandidate {
            path: path.to_string(),
            line,
            column,
        }
    }

    fn unique_temp_dir(label: &str) -> Result<std::path::PathBuf, String> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rustling-tulip-terminal-link-{label}-{}-{stamp}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        Ok(path)
    }
}

/// Terminate the Tauri app process. Used by the exit flow instead of
/// `WebviewWindow::destroy()`, which in Tauri v2 can deadlock when invoked
/// from inside the webview's own event loop (the IPC round-trip needed to
/// complete `destroy()` never gets serviced because the loop is awaiting
/// it). `AppHandle::exit` does not have that problem — it tears down every
/// window from the host side and returns control to the OS.
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri invoke handlers must own their args"
)]
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    info!("quit_app: invoking AppHandle::exit(0)");
    app.exit(0);
}

/// Open (or surface) a focused window for a single session. Subsequent calls
/// for the same session id are no-ops — the existing window is brought to
/// the front. The pop-out window loads the same React bundle with a
/// `?session=<id>` query parameter so `App.tsx` can render only the
/// `SessionWindow` component for that session.
#[tauri::command]
async fn open_session_window(app: tauri::AppHandle, session_id: String) -> Result<(), String> {
    let label = format!("session-{session_id}");
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.set_focus();
        return Ok(());
    }
    let url = format!("index.html?session={session_id}");
    let builder = WebviewWindowBuilder::new(&app, &label, WebviewUrl::App(url.into()))
        .title(format!("Session — {session_id}"))
        .inner_size(1100.0, 720.0)
        .min_inner_size(700.0, 400.0)
        // Tauri 2 defaults to true, which makes the OS file-drop layer
        // intercept HTML5 drag-and-drop events inside the WebView — every
        // intra-app drag gesture (session leaves between tabs, the pane
        // ⠿ handle between panes) immediately shows the "forbidden" cursor
        // because the OS thinks no drop target accepts it. We don't use
        // OS file drops anywhere, so flip it off everywhere.
        .disable_drag_drop_handler();
    apply_e2e_window_options(apply_window_appearance(builder))
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Open (or surface) a focused window for a single grid pane. The pane keeps
/// its slot in the source tab so the user can dock the window back later; the
/// pop-out itself reloads the same React bundle with `?pane=<id>`.
#[tauri::command]
async fn open_pane_window(app: tauri::AppHandle, pane_id: String) -> Result<(), String> {
    let label = format!("pane-{pane_id}");
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.set_focus();
        return Ok(());
    }
    let url = format!("index.html?pane={pane_id}");
    let builder = WebviewWindowBuilder::new(&app, &label, WebviewUrl::App(url.into()))
        .title(format!("Pane — {pane_id}"))
        .inner_size(1100.0, 720.0)
        .min_inner_size(700.0, 400.0)
        .disable_drag_drop_handler();
    apply_e2e_window_options(apply_window_appearance(builder))
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Open (or surface) a focused window for a single tab and its grid. Same
/// label-dedup behavior as [`open_session_window`]: re-calls focus the
/// existing window. The popped-out window loads `index.html?tab=<id>` so
/// `App.tsx` renders only the `TabWindow` for that tab.
#[tauri::command]
async fn open_tab_window(app: tauri::AppHandle, tab_id: String) -> Result<(), String> {
    let label = format!("tab-{tab_id}");
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.set_focus();
        return Ok(());
    }
    let url = format!("index.html?tab={tab_id}");
    let builder = WebviewWindowBuilder::new(&app, &label, WebviewUrl::App(url.into()))
        .title(format!("Tab — {tab_id}"))
        .inner_size(1100.0, 720.0)
        .min_inner_size(700.0, 400.0)
        // See open_session_window for the rationale on disabling OS-level
        // file-drop interception.
        .disable_drag_drop_handler();
    apply_e2e_window_options(apply_window_appearance(builder))
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[expect(
    clippy::missing_panics_doc,
    clippy::expect_used,
    reason = "Tauri builder errors are programmer errors; the canonical pattern is .expect()"
)]
pub fn run() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,rustling_tulip_app_lib=debug,daemon_client=debug")
    });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .compact()
        .try_init();

    // Mutate the bundled context BEFORE handing it to Builder::run so that
    // any e2e-mode window adjustments (hidden, unfocused, taskbar opt-out)
    // are baked into the initial WindowConfig and the window is created at
    // the right place. Setting position post-hoc in `setup` left a frame
    // visible on screen during boot — by the time setup ran the window had
    // already painted at its config default.
    let mut context = tauri::generate_context!();
    let offscreen = env_flag("RUSTLING_TULIP_OFFSCREEN_WINDOW");
    if offscreen {
        for window in &mut context.config_mut().app.windows {
            window.x = Some(-32_000.0);
            window.y = Some(-32_000.0);
            window.visible = false;
            window.focus = false;
            window.skip_taskbar = true;
        }
    }

    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init());
    // Skip window-state restoration in offscreen e2e mode. The plugin
    // auto-restores from disk on window creation, which would override the
    // hidden/offscreen e2e config we just baked into the context; worse, it
    // would also persist those coordinates back to the real user state file at
    // shutdown and strand the production app off-screen on
    // the next launch. Production runs get the plugin; tests run without.
    if !offscreen {
        builder = builder.plugin(tauri_plugin_window_state::Builder::default().build());
    }

    builder
        .manage(remote::RemoteState::default())
        .invoke_handler(tauri::generate_handler![
            ensure_daemon_started,
            daemon_paths,
            get_client_identity,
            stop_daemon,
            pick_directory,
            pick_file,
            open_session_window,
            open_pane_window,
            open_tab_window,
            reveal_in_explorer,
            open_url,
            open_terminal_path,
            open_folders_in_vscode,
            log_message,
            read_clipboard_text,
            quit_app,
            remote::connect_remote,
            remote::disconnect_remote,
            remote::decode_connection_code,
            remote::list_remote_profiles,
            remote::save_remote_profile,
            remote::delete_remote_profile,
            remote::discover_lan_hosts,
            remote::pair_with_host,
            autostart::get_autostart,
            autostart::set_autostart
        ])
        .setup(|_app| {
            info!("rustling-tulip Tauri app starting");
            if let Err(err) = rotate_app_log() {
                tracing::warn!(err, "failed to rotate app.log on boot");
            }
            let rt_claude = std::env::var("RUSTLING_TULIP_CLAUDE")
                .unwrap_or_else(|_| "(unset)".to_string());
            let rt_config_dir = std::env::var("RUSTLING_TULIP_CONFIG_DIR")
                .unwrap_or_else(|_| "(unset)".to_string());
            if let Err(err) = log_message(
                "INFO".to_string(),
                format!(
                    "tauri env RUSTLING_TULIP_CLAUDE={rt_claude} RUSTLING_TULIP_CONFIG_DIR={rt_config_dir}"
                ),
            ) {
                tracing::warn!(err, "failed to write env status to app.log");
            }
            Ok(())
        })
        .run(context)
        .expect("error while running tauri application");
}
