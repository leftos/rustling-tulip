use serde::{Deserialize, Serialize};
use std::path::PathBuf;
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
    Ok(path.and_then(|p| p.into_path().ok().map(|pb| pb.to_string_lossy().into_owned())))
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
            pick_directory
        ])
        .setup(|_app| {
            info!("rustling-tulip Tauri app starting");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
