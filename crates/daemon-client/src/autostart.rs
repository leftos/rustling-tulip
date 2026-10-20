//! "Start the daemon on login", so remote access is available without the client.
//!
//! The login launch starts the daemon inside the user's own interactive
//! session, where it sees that user's `~/.claude` credentials and PATH and
//! binds the tailnet listener that `remote.json` opts into — no window opens,
//! and no client has to be running. A SYSTEM service was rejected for that
//! reason: session 0 cannot read the user's credentials.
//!
//! - **Windows:** the per-user value `rustling-tulip-daemon` under
//!   `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, holding
//!   [`run_value`]: the quoted daemon path with `--detach`, so the login
//!   launch copies itself into the binaries cache and starts that copy instead
//!   of running the installed exe, which the next installer or rebuild would
//!   have to replace (see `daemon::detach`).
//! - **macOS:** a per-user `LaunchAgent` plist under `~/Library/LaunchAgents`,
//!   written in its final form; launchd loads it at login.
//!
//! [`refresh_registration`] runs at client startup and brings only an entry
//! that already names this install's daemon up to date: the same program
//! without `--detach` is rewritten in [`run_value`] form. A value naming any
//! other path — an older or moved install's — is left exactly as it is,
//! whether or not that path exists, and a missing entry is never created.
//!
//! Everywhere else [`set`] reports that autostart is unsupported and [`get`]
//! is `false`.

/// The Windows `Run` command for the daemon at `daemon_path`: quoted, so a
/// path with spaces survives the parse Windows does when it runs the value,
/// and with `--detach`, so the launch hands off to the cached copy.
#[must_use]
pub fn run_value(daemon_path: &str) -> String {
    format!("\"{daemon_path}\" --detach")
}

/// Whether the daemon is registered to start on login.
///
/// A registration that exists but cannot be read is logged and reported as
/// `false`; it is not this call's place to fail a startup. Always `false`
/// where autostart is unsupported.
#[must_use]
pub fn get() -> bool {
    match imp::get() {
        Ok(enabled) => enabled,
        Err(err) => {
            tracing::warn!("autostart: reading the login launch failed: {err:#}");
            false
        }
    }
}

/// Register or unregister the daemon to start on login. Enabling locates the
/// shipped daemon binary; disabling never does, so it works on a machine whose
/// daemon cannot be found.
///
/// # Errors
///
/// Fails when the platform has no supported login registration, when the
/// daemon binary cannot be located for an enable, or when the registration
/// cannot be written or removed.
pub fn set(enabled: bool) -> anyhow::Result<()> {
    imp::set(enabled)
}

/// Bring an existing Windows `Run` value up to date at client startup.
///
/// Only an entry that already names this install's daemon is touched, and only
/// when its form is stale — the same program written without `--detach`. It is
/// then rewritten as [`run_value`]. An entry naming any other path is left
/// exactly as it is, and an entry that does not exist is never created.
/// Failures are logged, never surfaced: a stale login launch is not worth
/// failing startup over.
#[cfg(windows)]
pub fn refresh_registration() {
    imp::refresh();
}

/// No-op where login registration is not the Windows `Run` value: the macOS
/// `LaunchAgent` is written in its final form.
#[cfg(not(windows))]
pub fn refresh_registration() {}

/// The shipped `rustling-tulipd` binary the login launch runs, resolved
/// exactly as the supervisor resolves it.
#[cfg(any(windows, target_os = "macos"))]
fn daemon_path() -> anyhow::Result<String> {
    crate::supervisor::locate_daemon_binary().map(|path| path.to_string_lossy().into_owned())
}

#[cfg(windows)]
mod imp {
    use anyhow::{Context as _, anyhow};
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    /// The `Run` value name the daemon is registered under.
    pub const VALUE_NAME: &str = "rustling-tulip-daemon";

    /// The per-user key login launches are read from.
    pub const RUN_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    /// Whether the daemon is registered to start on login.
    ///
    /// # Errors
    ///
    /// Fails when the registration exists but cannot be read.
    pub fn get() -> anyhow::Result<bool> {
        get_named(VALUE_NAME)
    }

    /// Register or unregister the daemon, locating the shipped binary only
    /// when enabling.
    ///
    /// # Errors
    ///
    /// As [`set_named`], plus a failure to locate the daemon binary.
    pub fn set(enabled: bool) -> anyhow::Result<()> {
        set_named(VALUE_NAME, enabled, super::daemon_path)
    }

    /// Whether the `Run` value `name` is present.
    ///
    /// # Errors
    ///
    /// Fails when the key or value exists but cannot be read for a reason
    /// other than being missing.
    pub fn get_named(name: &str) -> anyhow::Result<bool> {
        Ok(stored_value_named(name)?.is_some())
    }

    /// The `Run` value `name` as it stands on this machine, or `None` when the
    /// key or the value is missing.
    ///
    /// # Errors
    ///
    /// Fails when the key or value exists but cannot be read: an unreadable
    /// registration is not an absent one.
    pub fn stored_value_named(name: &str) -> anyhow::Result<Option<String>> {
        let run = match RegKey::predef(HKEY_CURRENT_USER).open_subkey(RUN_PATH) {
            Ok(run) => run,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(anyhow!(err).context("opening the HKCU Run key")),
        };
        match run.get_value::<String, _>(name) {
            Ok(value) => Ok(Some(value)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(anyhow!(err).context("reading the autostart value")),
        }
    }

    /// Register or unregister `name`. `resolve` is called only when enabling,
    /// so turning autostart off works where the daemon binary cannot be
    /// located.
    ///
    /// # Errors
    ///
    /// Fails when the daemon path cannot be resolved for an enable, or when
    /// the value cannot be written or removed.
    pub fn set_named(
        name: &str,
        enabled: bool,
        resolve: impl FnOnce() -> anyhow::Result<String>,
    ) -> anyhow::Result<()> {
        let daemon_path = if enabled { Some(resolve()?) } else { None };
        let (run, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey(RUN_PATH)
            .context("opening the HKCU Run key")?;
        if let Some(path) = daemon_path {
            run.set_value(name, &super::run_value(&path))
                .context("writing the autostart value")?;
            return Ok(());
        }
        match run.delete_value(name) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(anyhow!(err).context("removing the autostart value")),
        }
    }

    /// Bring `name` up to date for the daemon at `daemon_path`: rewrite it in
    /// [`super::run_value`] form only when it names that same program and its
    /// form differs. An entry naming another program is left exactly as it
    /// is, and a missing one is never created.
    pub fn refresh_named(name: &str, daemon_path: &str) {
        let stored = match stored_value_named(name) {
            Ok(Some(stored)) => stored,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!("autostart: could not read the login launch: {err:#}");
                return;
            }
        };
        if !program_path(&stored).eq_ignore_ascii_case(daemon_path) {
            return;
        }
        let desired = super::run_value(daemon_path);
        if stored == desired {
            return;
        }
        match set_named(name, true, || Ok(daemon_path.to_owned())) {
            Ok(()) => tracing::info!("autostart: rewrote the login launch as {desired}"),
            Err(err) => tracing::warn!("autostart: could not update the login launch: {err:#}"),
        }
    }

    pub fn refresh() {
        let registered = match get() {
            Ok(registered) => registered,
            Err(err) => {
                tracing::warn!("autostart: could not read the login launch: {err:#}");
                return;
            }
        };
        if !registered {
            return;
        }
        match super::daemon_path() {
            Ok(path) => refresh_named(VALUE_NAME, &path),
            Err(err) => tracing::warn!("autostart: cannot locate the daemon binary: {err:#}"),
        }
    }

    /// The program a `Run` command line names: the quoted string when the
    /// value starts with a quote, else the first whitespace-separated token.
    fn program_path(stored: &str) -> &str {
        let trimmed = stored.trim();
        if let Some(quoted) = trimmed.strip_prefix('"') {
            return quoted.find('"').map_or(quoted, |end| &quoted[..end]);
        }
        trimmed
            .find(char::is_whitespace)
            .map_or(trimmed, |end| &trimmed[..end])
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use anyhow::{Context as _, anyhow};
    use std::path::{Path, PathBuf};

    const LABEL: &str = "dev.leftos.rustling-tulip.daemon";

    fn plist_path() -> anyhow::Result<PathBuf> {
        let base = directories::BaseDirs::new()
            .ok_or_else(|| anyhow!("could not resolve the home directory"))?;
        Ok(base
            .home_dir()
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    pub fn get() -> anyhow::Result<bool> {
        Ok(plist_path()?.exists())
    }

    pub fn set(enabled: bool) -> anyhow::Result<()> {
        set_at(&plist_path()?, enabled, super::daemon_path)
    }

    /// Register or unregister the `LaunchAgent` kept at `path`. `resolve` is
    /// called only when enabling.
    pub fn set_at(
        path: &Path,
        enabled: bool,
        resolve: impl FnOnce() -> anyhow::Result<String>,
    ) -> anyhow::Result<()> {
        if !enabled {
            return match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(anyhow!(err).context(format!("removing {}", path.display()))),
            };
        }
        let daemon_path = resolve()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(path, plist_xml(LABEL, &daemon_path))
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Emit the `LaunchAgent` plist. Writing the file is enough: launchd scans
    /// `~/Library/LaunchAgents` at login, and loading it now would start a
    /// second daemon beside the one the client supervises. `RunAtLoad` starts
    /// the daemon when launchd loads the agent.
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

    fn xml_escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use anyhow::bail;

    pub fn get() -> anyhow::Result<bool> {
        Ok(false)
    }

    pub fn set(_enabled: bool) -> anyhow::Result<()> {
        bail!("starting the daemon on login is only supported on Windows and macOS")
    }
}

#[cfg(test)]
mod tests {
    use super::run_value;

    #[test]
    fn run_value_quotes_the_path_and_detaches() {
        assert_eq!(
            run_value(r"C:\Program Files\rustling-tulip\rustling-tulipd.exe"),
            "\"C:\\Program Files\\rustling-tulip\\rustling-tulipd.exe\" --detach"
        );
    }
}

#[cfg(all(test, windows))]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod registry_tests {
    use super::imp;
    use anyhow::anyhow;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    /// A path that cannot exist, so a throwaway value a failing test leaves in
    /// the registry can never launch anything or replace a real install.
    const TEST_DAEMON: &str = r"C:\rustling-tulip-test-nonexistent\rustling-tulipd.exe";

    /// Another path that cannot exist, standing in for a moved install.
    const OTHER_DAEMON: &str = r"C:\rustling-tulip-test-nonexistent-other\rustling-tulipd.exe";

    /// A space-free path for the unquoted command-line form.
    const BARE_DAEMON: &str = r"C:\rustling-tulip-test-nonexistent\bin\rustling-tulipd.exe";

    /// A throwaway `Run` value name, removed by [`Drop`] whether the test
    /// passed, failed or panicked. The real `rustling-tulip-daemon` value is
    /// never touched.
    struct ThrowawayValue(String);

    impl ThrowawayValue {
        fn new() -> Self {
            Self(format!(
                "rustling-tulip-daemon-test-{}",
                uuid::Uuid::new_v4().simple()
            ))
        }

        fn name(&self) -> &str {
            &self.0
        }
    }

    impl Drop for ThrowawayValue {
        fn drop(&mut self) {
            let _ = imp::set_named(self.name(), false, || Ok(String::new()));
        }
    }

    /// Write `value` straight into the `Run` key, the way an older build or
    /// another tool would have left it.
    fn write_raw(name: &str, value: &str) {
        let (run, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey(imp::RUN_PATH)
            .expect("open the HKCU Run key");
        run.set_value(name, &value).expect("write the raw value");
    }

    fn stored(name: &str) -> Option<String> {
        imp::stored_value_named(name).expect("read the throwaway value")
    }

    #[test]
    fn set_writes_the_quoted_value_and_unset_removes_it() {
        let value = ThrowawayValue::new();

        imp::set_named(value.name(), true, || Ok(TEST_DAEMON.to_owned()))
            .expect("write the throwaway Run value");

        assert!(
            imp::get_named(value.name()).expect("read the throwaway value"),
            "value missing after set(true)"
        );
        let expected = super::run_value(TEST_DAEMON);
        assert_eq!(stored(value.name()).as_deref(), Some(expected.as_str()));

        imp::set_named(value.name(), false, || Ok(String::new()))
            .expect("remove the throwaway Run value");

        assert!(
            !imp::get_named(value.name()).expect("read the throwaway value"),
            "value survived set(false)"
        );
        assert_eq!(stored(value.name()), None);
    }

    #[test]
    fn unsetting_needs_no_daemon_path() {
        let value = ThrowawayValue::new();
        imp::set_named(value.name(), true, || Ok(TEST_DAEMON.to_owned())).expect("write the value");

        imp::set_named(value.name(), false, || {
            Err(anyhow!("no daemon binary to locate"))
        })
        .expect("set(false) must not resolve the daemon path");

        assert_eq!(stored(value.name()), None);
    }

    #[test]
    fn refresh_upgrades_a_quoted_value_without_detach() {
        let value = ThrowawayValue::new();
        write_raw(value.name(), &format!("\"{TEST_DAEMON}\""));

        imp::refresh_named(value.name(), TEST_DAEMON);

        let expected = super::run_value(TEST_DAEMON);
        assert_eq!(
            stored(value.name()).as_deref(),
            Some(expected.as_str()),
            "refresh left a same-path value without --detach in place"
        );
    }

    #[test]
    fn refresh_upgrades_a_bare_value_without_detach() {
        let value = ThrowawayValue::new();
        write_raw(value.name(), BARE_DAEMON);

        imp::refresh_named(value.name(), BARE_DAEMON);

        let expected = super::run_value(BARE_DAEMON);
        assert_eq!(
            stored(value.name()).as_deref(),
            Some(expected.as_str()),
            "refresh left a same-path bare value in place"
        );
    }

    #[test]
    fn refresh_leaves_an_entry_for_another_path_alone() {
        let value = ThrowawayValue::new();
        let other = format!("\"{OTHER_DAEMON}\" --detach");
        write_raw(value.name(), &other);

        imp::refresh_named(value.name(), TEST_DAEMON);

        assert_eq!(
            stored(value.name()).as_deref(),
            Some(other.as_str()),
            "refresh repointed an entry that names another install"
        );
    }

    #[test]
    fn refresh_leaves_an_existing_path_for_another_program_alone() {
        let value = ThrowawayValue::new();
        // A real executable that is not the daemon: never rewritten, never run.
        let other = r"C:\Windows\System32\cmd.exe /c exit";
        write_raw(value.name(), other);

        imp::refresh_named(value.name(), TEST_DAEMON);

        assert_eq!(stored(value.name()).as_deref(), Some(other));
    }

    #[test]
    fn refresh_leaves_an_already_current_value_alone() {
        let value = ThrowawayValue::new();
        let current = super::run_value(TEST_DAEMON);
        write_raw(value.name(), &current);

        imp::refresh_named(value.name(), TEST_DAEMON);

        assert_eq!(stored(value.name()).as_deref(), Some(current.as_str()));
    }

    #[test]
    fn refresh_never_creates_a_value() {
        let value = ThrowawayValue::new();

        imp::refresh_named(value.name(), TEST_DAEMON);

        assert!(
            !imp::get_named(value.name()).expect("read the throwaway value"),
            "refresh created a Run value with autostart off"
        );
    }
}
