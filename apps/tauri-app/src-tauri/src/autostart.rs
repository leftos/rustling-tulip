//! Host-side "start the daemon on login" for remote LAN access.
//!
//! Registers the daemon binary (not the GUI app) under the per-user HKCU `Run`
//! key, so after a reboot the daemon comes up in the user's interactive session
//! — with full `~/.claude` + PATH access — and binds its opt-in LAN listener
//! from the persisted `lan.json` without anyone opening the app. A SYSTEM
//! service was rejected: session 0 can't see the user's claude credentials.
//!
//! Windows-only. On other platforms the commands report "unsupported" and the
//! settings toggle stays hidden.

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

/// Whether the daemon is registered to start on login. Always `false` off
/// Windows (autostart is unsupported there).
#[tauri::command]
pub async fn get_autostart() -> Result<bool, String> {
    #[cfg(windows)]
    {
        Ok(imp::get())
    }
    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

/// Register or unregister the daemon to start on login.
#[tauri::command]
pub async fn set_autostart(enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        let path = crate::daemon_supervisor::locate_daemon_binary()?;
        imp::set(enabled, &path.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Err("starting the daemon on login is only supported on Windows".to_string())
    }
}
