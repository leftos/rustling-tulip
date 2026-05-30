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
mod daemon_supervisor;
mod remote;

const APP_BACKGROUND_COLOR: Color = Color(8, 9, 11, 255);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonHandshake {
    pub protocol_version: u32,
    pub port: u16,
    pub auth_token: String,
    pub pid: u32,
}

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

/// Resolve the per-user config directory. Mirrors
/// `daemon::paths::Dirs::ensure`'s resolution: honors
/// `RUSTLING_TULIP_CONFIG_DIR` if set (used by the e2e harness to isolate
/// test runs to a tmpdir), otherwise falls back to
/// `ProjectDirs::from("dev", "leftos", "rustling-tulip").config_dir()`.
fn config_dir() -> Result<PathBuf, String> {
    if let Ok(value) = std::env::var("RUSTLING_TULIP_CONFIG_DIR")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let pd = directories::ProjectDirs::from("dev", "leftos", "rustling-tulip")
        .ok_or_else(|| "could not resolve config directory".to_string())?;
    Ok(pd.config_dir().to_path_buf())
}

/// Returns the path that the daemon writes its handshake to. Mirrors
/// `daemon::paths::Dirs::ensure().handshake_file`.
pub fn handshake_file() -> Result<PathBuf, String> {
    Ok(config_dir()?.join("daemon.json"))
}

#[tauri::command]
async fn ensure_daemon_started(app: tauri::AppHandle) -> Result<DaemonHandshake, String> {
    daemon_supervisor::ensure_running(&app).await
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
    let log_dir = config_dir()?.join("logs");
    std::fs::create_dir_all(&log_dir).map_err(|e| e.to_string())?;
    Ok(log_dir.join("app.log"))
}

/// Truncate `app.log` (creating it if missing). Called once on app startup so
/// each launch produces a clean log file rather than ever-growing append. The
/// daemon mirrors this on its side with truncate-on-start for `daemon.log`.
fn truncate_app_log() -> Result<(), String> {
    let path = app_log_path()?;
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

/// Stable per-install client identity for per-client tab layouts. `client_id`
/// is generated once and persisted in the config dir so a window and its
/// pop-outs (same install) share one layout; `client_name` is the machine
/// hostname, shown when another client offers to clone this layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientIdentity {
    pub client_id: String,
    pub client_name: Option<String>,
}

#[tauri::command]
fn get_client_identity() -> Result<ClientIdentity, String> {
    let dir = config_dir()?;
    let path = dir.join("client-id");
    let client_id = match std::fs::read_to_string(&path) {
        Ok(existing) if !existing.trim().is_empty() => existing.trim().to_string(),
        _ => {
            let id = uuid::Uuid::new_v4().to_string();
            std::fs::create_dir_all(&dir).map_err(|e| format!("create config dir: {e}"))?;
            std::fs::write(&path, &id).map_err(|e| format!("write client-id: {e}"))?;
            id
        }
    };
    Ok(ClientIdentity {
        client_id,
        client_name: sysinfo::System::host_name(),
    })
}

#[tauri::command]
fn daemon_paths() -> Result<DaemonPaths, String> {
    let cfg = config_dir()?;
    let logs = cfg.join("logs");
    Ok(DaemonPaths {
        config_dir: cfg.to_string_lossy().into_owned(),
        daemon_log: logs.join("daemon.log").to_string_lossy().into_owned(),
        app_log: logs.join("app.log").to_string_lossy().into_owned(),
        handshake_file: cfg.join("daemon.json").to_string_lossy().into_owned(),
    })
}

/// Force-stop the running daemon by reading its pid from `daemon.json` and
/// killing the process. Used by the footer's "Stop daemon" action. We kill
/// by pid rather than send a `Shutdown` WS message because the WS may
/// already be closed (e.g. when the footer surfaced "connecting…" and the
/// user gave up waiting), and pid-kill works in both states. The daemon's
/// drop guard would normally remove `daemon.json` on graceful exit; we
/// remove it here too so a subsequent `ensure_daemon_started` doesn't
/// mistake a stale handshake for a live daemon.
#[tauri::command]
async fn stop_daemon() -> Result<(), String> {
    let path = handshake_file()?;
    if !path.exists() {
        return Err("no daemon handshake on disk — daemon may not be running".to_string());
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let parsed: DaemonHandshake =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse handshake: {e}"))?;
    kill_pid(parsed.pid).await?;
    // Best-effort cleanup; absence will be detected on next ensure_running
    // regardless. Don't error if the daemon's drop guard beat us to it.
    let _ = tokio::fs::remove_file(&path).await;
    Ok(())
}

#[cfg(windows)]
pub(crate) async fn kill_pid(pid: u32) -> Result<(), String> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let status = tokio::process::Command::new("taskkill")
        .arg("/PID")
        .arg(pid.to_string())
        .arg("/F")
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .await
        .map_err(|e| format!("taskkill: {e}"))?;
    if !status.success() {
        return Err(format!("taskkill exited with {status}"));
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) async fn kill_pid(pid: u32) -> Result<(), String> {
    let status = tokio::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await
        .map_err(|e| format!("kill: {e}"))?;
    if !status.success() {
        return Err(format!("kill exited with {status}"));
    }
    Ok(())
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
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("explorer.exe")
            .arg(&pb)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&pb)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(&pb)
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

#[tauri::command]
async fn open_path_in_vscode(
    path: String,
    base_dirs: Vec<String>,
    line: Option<u32>,
    column: Option<u32>,
) -> Result<(), String> {
    let resolved = resolve_existing_terminal_path(&path, &base_dirs)?;
    spawn_vscode(&resolved, line, column)
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
    use super::{resolve_existing_terminal_path, simplify_path, validate_http_url};
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
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,rustling_tulip_app_lib=debug"));
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
            open_path_in_vscode,
            open_folders_in_vscode,
            log_message,
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
            if let Err(err) = truncate_app_log() {
                tracing::warn!(err, "failed to truncate app.log on boot");
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
