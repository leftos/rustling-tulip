use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::PathBuf;
use tauri::{Manager as _, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::DialogExt as _;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod daemon_supervisor;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonHandshake {
    pub protocol_version: u32,
    pub port: u16,
    pub auth_token: String,
    pub pid: u32,
}

/// Returns the path that the daemon writes its handshake to. Mirrors
/// `daemon::paths::Dirs::ensure().handshake_file`.
pub fn handshake_file() -> Result<PathBuf, String> {
    let pd = directories::ProjectDirs::from("dev", "leftos", "rustling-tulip")
        .ok_or_else(|| "could not resolve config directory".to_string())?;
    let dir = pd.config_dir().to_path_buf();
    Ok(dir.join("daemon.json"))
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
    let pd = directories::ProjectDirs::from("dev", "leftos", "rustling-tulip")
        .ok_or_else(|| "could not resolve config directory".to_string())?;
    let log_dir = pd.config_dir().join("logs");
    std::fs::create_dir_all(&log_dir).map_err(|e| e.to_string())?;
    Ok(log_dir.join("app.log"))
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
    WebviewWindowBuilder::new(&app, &label, WebviewUrl::App(url.into()))
        .title(format!("Session — {session_id}"))
        .inner_size(1100.0, 720.0)
        .min_inner_size(700.0, 400.0)
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
    WebviewWindowBuilder::new(&app, &label, WebviewUrl::App(url.into()))
        .title(format!("Tab — {tab_id}"))
        .inner_size(1100.0, 720.0)
        .min_inner_size(700.0, 400.0)
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

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![
            ensure_daemon_started,
            pick_directory,
            pick_file,
            open_session_window,
            open_tab_window,
            reveal_in_explorer,
            log_message,
            quit_app
        ])
        .setup(|_app| {
            info!("rustling-tulip Tauri app starting");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
