//! Host-side "start the daemon on login" for remote LAN access.
//!
//! Registers the daemon binary (not the GUI app) so after a reboot the daemon
//! comes up in the user's interactive session — with full `~/.claude` + PATH
//! access — and binds its opt-in LAN listener from the persisted `lan.json`
//! without anyone opening the app. A SYSTEM/SYSTEM-service approach was
//! rejected: session 0 (or a system `LaunchDaemon`) can't see the user's claude
//! credentials.
//!
//! - **Windows:** per-user HKCU `Run` key.
//! - **macOS:** a per-user `LaunchAgent` plist under `~/Library/LaunchAgents`.
//!
//! On other platforms the commands report "unsupported".

#[cfg(windows)]
mod imp {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    const RUN_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "rustling-tulip-daemon";

    pub fn get() -> bool {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        // A missing Run key (unusual) means "not registered".
        let Ok(run) = hkcu.open_subkey(RUN_PATH) else {
            return false;
        };
        run.get_value::<String, _>(VALUE_NAME).is_ok()
    }

    /// The `Run` command for `daemon_path`: quoted, so spaces (e.g. Program
    /// Files) survive the shell parse Windows does when it runs the value, and
    /// with `--detach`, so the login launch copies itself into the binaries
    /// cache and starts the cached copy instead of running the installed exe
    /// (which the next installer or rebuild would have to replace).
    pub fn run_value(daemon_path: &str) -> String {
        format!("\"{daemon_path}\" --detach")
    }

    /// The `Run` value as it stands on this machine, or `None` when the daemon
    /// is not registered.
    pub fn stored_value() -> Option<String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let run = hkcu.open_subkey(RUN_PATH).ok()?;
        run.get_value::<String, _>(VALUE_NAME).ok()
    }

    pub fn set(enabled: bool, daemon_path: &str) -> Result<(), String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (run, _) = hkcu
            .create_subkey(RUN_PATH)
            .map_err(|e| format!("opening HKCU Run key: {e}"))?;
        if enabled {
            run.set_value(VALUE_NAME, &run_value(daemon_path))
                .map_err(|e| format!("writing autostart value: {e}"))?;
        } else {
            match run.delete_value(VALUE_NAME) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("removing autostart value: {e}")),
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::PathBuf;

    const LABEL: &str = "dev.leftos.rustling-tulip.daemon";

    fn plist_path() -> Result<PathBuf, String> {
        let base = directories::BaseDirs::new()
            .ok_or_else(|| "could not resolve home directory".to_string())?;
        Ok(base
            .home_dir()
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    pub fn get() -> bool {
        plist_path().is_ok_and(|p| p.exists())
    }

    pub fn set(enabled: bool, daemon_path: &str) -> Result<(), String> {
        let path = plist_path()?;
        if enabled {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("creating LaunchAgents dir: {e}"))?;
            }
            std::fs::write(&path, plist_xml(LABEL, daemon_path))
                .map_err(|e| format!("writing LaunchAgent plist: {e}"))?;
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("removing LaunchAgent plist: {e}")),
            }
        }
        Ok(())
    }

    /// Emit the LaunchAgent plist. We deliberately rely on launchd's login-time
    /// scan of `~/Library/LaunchAgents` rather than `launchctl load` on toggle:
    /// loading immediately would start a second daemon while the app's
    /// supervisor already runs one. Writing the file alone gives next-login
    /// parity with the Windows HKCU `Run` key. `RunAtLoad` starts the daemon
    /// when launchd loads the agent at login.
    fn plist_xml(label: &str, daemon_path: &str) -> String {
        const TEMPLATE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>__LABEL__</string>
    <key>ProgramArguments</key>
    <array>
        <string>__PROGRAM__</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#;
        TEMPLATE
            .replace("__LABEL__", label)
            .replace("__PROGRAM__", &xml_escape(daemon_path))
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }
}

/// Whether the daemon is registered to start on login. Always `false` on
/// platforms where autostart is unsupported (currently everything but Windows
/// and macOS).
#[tauri::command]
pub async fn get_autostart() -> Result<bool, String> {
    #[cfg(any(windows, target_os = "macos"))]
    {
        Ok(imp::get())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Ok(false)
    }
}

/// Register or unregister the daemon to start on login.
#[tauri::command]
pub async fn set_autostart(enabled: bool) -> Result<(), String> {
    #[cfg(any(windows, target_os = "macos"))]
    {
        let path = daemon_client::locate_daemon_binary().map_err(|e| format!("{e:#}"))?;
        imp::set(enabled, &path.to_string_lossy())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = enabled;
        Err("starting the daemon on login is only supported on Windows and macOS".to_string())
    }
}

/// Bring an existing Windows `Run` entry up to date: rewrite it when its path
/// no longer matches the installed daemon, or when it predates `--detach`.
/// Does nothing when autostart is off, and on platforms that don't use the
/// `Run` key (the macOS `LaunchAgent` is written in its final form). Failures
/// are logged and swallowed — a stale entry is not worth failing app startup
/// over.
#[cfg(windows)]
pub fn refresh_registration() {
    if !imp::get() {
        return;
    }
    let path = match daemon_client::locate_daemon_binary() {
        Ok(path) => path,
        Err(err) => {
            tracing::warn!("autostart: cannot locate the daemon binary: {err:#}");
            return;
        }
    };
    let daemon_path = path.to_string_lossy();
    let desired = imp::run_value(&daemon_path);
    if imp::stored_value().as_deref() == Some(desired.as_str()) {
        return;
    }
    if let Err(err) = imp::set(true, &daemon_path) {
        tracing::warn!("autostart: could not update the Run value: {err}");
    } else {
        tracing::info!("autostart: rewrote the Run value as {desired}");
    }
}

/// No-op on platforms whose login registration is not the Windows `Run` key.
#[cfg(not(windows))]
pub fn refresh_registration() {}

#[cfg(all(test, windows))]
mod tests {
    use super::imp;

    #[test]
    fn run_value_quotes_the_path_and_detaches() {
        assert_eq!(
            imp::run_value(r"C:\Program Files\rustling-tulip\rustling-tulipd.exe"),
            "\"C:\\Program Files\\rustling-tulip\\rustling-tulipd.exe\" --detach"
        );
    }
}
