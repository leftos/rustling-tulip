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

    pub fn set(enabled: bool, daemon_path: &str) -> Result<(), String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (run, _) = hkcu
            .create_subkey(RUN_PATH)
            .map_err(|e| format!("opening HKCU Run key: {e}"))?;
        if enabled {
            // Quote the path so spaces (e.g. Program Files) survive the
            // shell parse Windows does when it executes the Run value.
            let quoted = format!("\"{daemon_path}\"");
            run.set_value(VALUE_NAME, &quoted)
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
        let path = crate::daemon_supervisor::locate_daemon_binary()?;
        imp::set(enabled, &path.to_string_lossy())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = enabled;
        Err("starting the daemon on login is only supported on Windows and macOS".to_string())
    }
}
