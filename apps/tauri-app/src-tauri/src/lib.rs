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

/// Parse a boolean-ish env var. Set + non-empty + not literally "0" counts as
/// true; everything else is false. Used for opt-in harness toggles where the
/// presence of any meaningful value should enable the flag.
fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0")
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
async fn kill_pid(pid: u32) -> Result<(), String> {
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
async fn kill_pid(pid: u32) -> Result<(), String> {
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
        // Tauri 2 defaults to true, which makes the OS file-drop layer
        // intercept HTML5 drag-and-drop events inside the WebView — every
        // intra-app drag gesture (session leaves between tabs, the pane
        // ⠿ handle between panes) immediately shows the "forbidden" cursor
        // because the OS thinks no drop target accepts it. We don't use
        // OS file drops anywhere, so flip it off everywhere.
        .disable_drag_drop_handler()
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
        // See open_session_window for the rationale on disabling OS-level
        // file-drop interception.
        .disable_drag_drop_handler()
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
    // any e2e-mode window adjustments (offscreen position, taskbar opt-out)
    // are baked into the initial WindowConfig and the window is created at
    // the right place. Setting position post-hoc in `setup` left a frame
    // visible on screen during boot — by the time setup ran the window had
    // already painted at its config default. See task #56.
    let mut context = tauri::generate_context!();
    let offscreen = env_flag("RUSTLING_TULIP_OFFSCREEN_WINDOW");
    if offscreen {
        for window in &mut context.config_mut().app.windows {
            window.x = Some(-32_000.0);
            window.y = Some(-32_000.0);
            window.skip_taskbar = true;
        }
    }

    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_shell::init());
    // Skip window-state restoration in offscreen e2e mode. The plugin
    // auto-restores from disk on window creation, which would override the
    // -32_000 offscreen coords we just baked into the context; worse, it
    // would also persist those offscreen coords back to the real user
    // state file at shutdown and strand the production app off-screen on
    // the next launch. Production runs get the plugin; tests run without.
    if !offscreen {
        builder = builder.plugin(tauri_plugin_window_state::Builder::default().build());
    }

    builder
        .invoke_handler(tauri::generate_handler![
            ensure_daemon_started,
            daemon_paths,
            stop_daemon,
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
