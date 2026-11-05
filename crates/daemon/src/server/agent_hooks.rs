//! An agent's user-settings hooks: reading, installing and removing the
//! daemon's hook entries in the agent's user settings file
//! ([`ClientMessage::GetAgentUserHooks`], [`ClientMessage::SetAgentUserHooks`]),
//! and the start-up refresh of an install that is on.
//!
//! The plugin computes the file's new text; every read and write of the file
//! is here. A write is atomic and compare-and-swap against the text the edit
//! was computed from, the file is backed up once before the first write, and a
//! file the plugin cannot parse is refused and left alone.

use super::Hub;
use super::connection::ConnCtx;
use crate::binary_cache;
use crate::paths::Dirs;
use agent_api::{AgentError, AgentPlugin, UserHooksState};
use protocol::{ClientMessage, DaemonMessage, UserHooksWire};
use std::ffi::OsString;
use std::fs;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// The error a Set gets from an isolated daemon for a settings file outside
/// any `.tmp` folder.
const ISOLATED_REFUSAL: &str = "an isolated daemon only edits a settings file under a .tmp folder";

/// The variable whose non-empty value overrides the daemon's config dir and
/// makes it an isolated daemon.
const CONFIG_DIR_VAR: &str = "RUSTLING_TULIP_CONFIG_DIR";

/// Answer a Get or Set of an agent's user hooks to the asking connection
/// alone. The file work runs off the async workers.
pub(super) async fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) -> anyhow::Result<()> {
    let ConnCtx { out_tx, .. } = *ctx;
    let (agent, request, request_id) = match msg {
        ClientMessage::GetAgentUserHooks { agent, request_id } => (agent, Request::Get, request_id),
        ClientMessage::SetAgentUserHooks {
            agent,
            enabled,
            request_id,
        } => (agent, Request::Set(enabled), request_id),
        _ => {
            warn!("agent_hooks::handle: dispatch routed a message this module does not handle");
            return Ok(());
        }
    };
    let dirs = hub.dirs.clone();
    let elevated = hub.integrity.is_elevated();
    let target = agent.clone();
    let reply = tokio::task::spawn_blocking(move || {
        let plugin = agent_registry::plugin(&target);
        let settings = plugin.and_then(AgentPlugin::user_settings_path);
        with_site(&dirs, elevated, |site| match site {
            Some(site) => answer(plugin, settings.as_deref(), request, site),
            None => Reply::unsupported(),
        })
    })
    .await
    .unwrap_or_else(|err| {
        warn!(?err, "agent_hooks: the settings task did not finish");
        Reply {
            error: Some("the settings request did not finish".to_owned()),
            ..Reply::unsupported()
        }
    });
    let _ = out_tx.send(DaemonMessage::AgentUserHooks {
        agent,
        state: reply.state,
        path: reply.path,
        error: reply.error,
        request_id,
    });
    Ok(())
}

/// A request on an agent's user hooks.
#[derive(Debug, Clone, Copy)]
pub(super) enum Request {
    /// Read the state.
    Get,
    /// Install (`true`) or remove (`false`) the daemon's entries.
    Set(bool),
}

/// What a [`DaemonMessage::AgentUserHooks`] answers.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Reply {
    pub(super) state: UserHooksWire,
    pub(super) path: Option<String>,
    pub(super) error: Option<String>,
}

impl Reply {
    /// The answer for an agent this daemon does not edit the settings of.
    fn unsupported() -> Self {
        Self {
            state: UserHooksWire::Unsupported,
            path: None,
            error: None,
        }
    }

    /// `self` with `error` in place of whatever it carried.
    fn with_error(self, error: String) -> Self {
        Self {
            error: Some(error),
            ..self
        }
    }
}

/// Where this daemon's hook entries point and what it may edit, fixed for
/// one request or one start-up refresh.
pub(super) struct Site<'a> {
    /// The fixed hook shim path the entries run.
    pub(super) shim: PathBuf,
    /// The daemon is elevated: every agent reads unsupported.
    pub(super) elevated: bool,
    /// The daemon's config dir is overridden: a Set edits only a settings
    /// file under a `.tmp` folder.
    pub(super) isolated: bool,
    /// Writes the current tracer's bytes to [`Self::shim`].
    pub(super) ensure_shim: &'a dyn Fn() -> anyhow::Result<()>,
}

/// Run `f` with this daemon's [`Site`] over `dirs`, or with `None` when the
/// binaries dir has no parent to hold the shim.
fn with_site<R>(dirs: &Dirs, elevated: bool, f: impl FnOnce(Option<&Site<'_>>) -> R) -> R {
    let Some(data_dir) = binary_cache::hook_shim_data_dir(dirs) else {
        return f(None);
    };
    let ensure = || -> anyhow::Result<()> {
        let tracer = crate::tracer_client::cached_tracer_exe(dirs)?;
        binary_cache::ensure_hook_shim(&tracer, data_dir)?;
        Ok(())
    };
    let site = Site {
        shim: binary_cache::hook_shim_path(data_dir),
        elevated,
        isolated: is_isolated(std::env::var_os(CONFIG_DIR_VAR)),
        ensure_shim: &ensure,
    };
    f(Some(&site))
}

/// Whether [`CONFIG_DIR_VAR`]'s `value` makes this an isolated daemon: set
/// and non-empty.
fn is_isolated(value: Option<OsString>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// Answer `request` for `plugin` (`None` for an unknown agent) whose user
/// settings file is `settings` (`None` when it has none).
pub(super) fn answer(
    plugin: Option<&dyn AgentPlugin>,
    settings: Option<&Path>,
    request: Request,
    site: &Site<'_>,
) -> Reply {
    let (Some(plugin), Some(settings), false) = (plugin, settings, site.elevated) else {
        return Reply::unsupported();
    };
    match request {
        Request::Get => read_state(plugin, settings, &site.shim),
        Request::Set(enabled) => set(plugin, settings, enabled, site),
    }
}

/// The state `settings` holds, read from the file.
fn read_state(plugin: &dyn AgentPlugin, settings: &Path, shim: &Path) -> Reply {
    let path = Some(settings.display().to_string());
    let read = read_settings(settings).and_then(|text| {
        plugin
            .user_hooks_state(text.as_deref(), shim)
            .map_err(|err| named(settings, &err))
    });
    match read {
        Ok(state) => Reply {
            state: wire_state(state),
            path,
            error: None,
        },
        Err(error) => Reply {
            state: UserHooksWire::Off,
            path,
            error: Some(error),
        },
    }
}

/// Install or remove the entries in `settings`, then answer the file's state;
/// on a failure the state as the file now holds it, with the error.
fn set(plugin: &dyn AgentPlugin, settings: &Path, enabled: bool, site: &Site<'_>) -> Reply {
    if site.isolated && !daemon_client::isolation::under_tmp(settings) {
        return read_state(plugin, settings, &site.shim).with_error(ISOLATED_REFUSAL.to_owned());
    }
    if enabled && let Err(err) = (site.ensure_shim)() {
        return read_state(plugin, settings, &site.shim)
            .with_error(format!("{}: {err:#}", settings.display()));
    }
    let edited = edit_settings(settings, |text| match (enabled, text) {
        (true, text) => plugin.install_user_hooks(text, &site.shim).map(Some),
        (false, Some(text)) => plugin.remove_user_hooks(text, &site.shim).map(Some),
        (false, None) => Ok(None),
    });
    let reply = read_state(plugin, settings, &site.shim);
    match edited {
        Ok(()) => {
            info!(
                settings = %settings.display(),
                enabled,
                "agent_hooks: user settings hooks set"
            );
            reply
        }
        Err(error) => {
            warn!(%error, enabled, "agent_hooks: user settings hooks not set");
            reply.with_error(error)
        }
    }
}

/// Apply `compute` to `settings`' text (`None` for no file) and write what it
/// returns; `Ok(None)` or text equal to the file's writes nothing.
///
/// A settings path that is a symbolic link is refused before it is read: the
/// atomic rename would replace the link with a plain file. Before the first
/// write the file is copied to `<settings>.rt-backup` unless that exists, and
/// a backup this call made is removed again when nothing gets written. The
/// write is compare-and-swap: when the file no longer holds the text the edit
/// was computed from, the edit is computed once more from the new text, and a
/// second change refuses the edit. The write is atomic, creating the parent
/// folder for a new file.
///
/// # Errors
///
/// A message naming the file and the cause: a link, a read, the plugin's
/// refusal, the backup, the write, or a file that kept changing.
pub(super) fn edit_settings(
    settings: &Path,
    mut compute: impl FnMut(Option<&str>) -> Result<Option<String>, AgentError>,
) -> Result<(), String> {
    if fs::symlink_metadata(settings).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(format!(
            "{} is a link; edit the file it points to, or replace the link with a file",
            settings.display()
        ));
    }
    let mut made_backup = false;
    let outcome = swap_settings(settings, &mut compute, &mut made_backup);
    if outcome.is_err() && made_backup {
        // Nothing of ours was written, so the copy is not a pre-edit backup.
        let _ = fs::remove_file(backup_path(settings));
    }
    outcome
}

/// [`edit_settings`]' compare-and-swap; `made_backup` is set once this call
/// creates the backup.
fn swap_settings(
    settings: &Path,
    compute: &mut impl FnMut(Option<&str>) -> Result<Option<String>, AgentError>,
    made_backup: &mut bool,
) -> Result<(), String> {
    let mut base = read_settings(settings)?;
    for _ in 0..2 {
        let Some(next) = compute(base.as_deref()).map_err(|err| named(settings, &err))? else {
            return Ok(());
        };
        if base.as_deref() == Some(next.as_str()) {
            return Ok(());
        }
        *made_backup |= back_up_once(settings)?;
        let now = read_settings(settings)?;
        if now == base {
            return write_settings(settings, &next);
        }
        base = now;
    }
    Err(format!(
        "{} changed while it was being edited; try again",
        settings.display()
    ))
}

/// `settings`' text, `None` when there is no file.
fn read_settings(settings: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(settings) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(named(settings, &err)),
    }
}

/// `<settings>.rt-backup`.
pub(super) fn backup_path(settings: &Path) -> PathBuf {
    let mut name = OsString::from(settings.as_os_str());
    name.push(".rt-backup");
    PathBuf::from(name)
}

/// Copy `settings` byte for byte to its backup, unless the file is missing or
/// the backup exists; an existing backup is never overwritten. `true` when
/// this call created the backup.
fn back_up_once(settings: &Path) -> Result<bool, String> {
    let backup = backup_path(settings);
    let bytes = match fs::read(settings) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(named(settings, &err)),
    };
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup)
    {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::AlreadyExists => return Ok(false),
        Err(err) => return Err(named(&backup, &err)),
    };
    if let Err(err) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        drop(file);
        // A torn backup would never be replaced; leave none instead.
        let _ = fs::remove_file(&backup);
        return Err(named(&backup, &err));
    }
    info!(backup = %backup.display(), "agent_hooks: user settings backed up");
    Ok(true)
}

/// Write `text` to `settings` atomically, creating its folder.
fn write_settings(settings: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = settings.parent() {
        fs::create_dir_all(parent).map_err(|err| named(parent, &err))?;
    }
    crate::atomic_file::write_atomic(settings, text.as_bytes())
        .map_err(|err| format!("{}: {err:#}", settings.display()))
}

/// `cause` prefixed with the path it happened at.
fn named(path: &Path, cause: &dyn std::fmt::Display) -> String {
    format!("{}: {cause}", path.display())
}

/// Refresh, once at start, each registered agent's user-hooks install that is
/// on, unless the daemon is elevated: see [`refresh_one`]. Runs off the async
/// workers; a failure is logged and never stops the daemon.
pub(super) fn spawn_startup_refresh(hub: &Hub) {
    if hub.integrity.is_elevated() {
        return;
    }
    let dirs = hub.dirs.clone();
    tokio::task::spawn_blocking(move || {
        with_site(&dirs, false, |site| {
            let Some(site) = site else {
                warn!("agent_hooks: the binaries dir has no parent folder for the hook shim");
                return;
            };
            for plugin in agent_registry::all() {
                if let Some(settings) = plugin.user_settings_path() {
                    refresh_one(*plugin, &settings, site);
                }
            }
        });
    });
}

/// Keep one agent's install current: an install that is on or partial gets
/// the shim refreshed, a stale one is also repointed at the current shim
/// through the same path a Set takes. An install that is off, or no file,
/// is left alone, and no shim is made for it.
pub(super) fn refresh_one(plugin: &dyn AgentPlugin, settings: &Path, site: &Site<'_>) {
    let agent = &plugin.descriptor().id.0;
    let read = read_settings(settings).and_then(|text| {
        text.map(|text| {
            plugin
                .user_hooks_state(Some(&text), &site.shim)
                .map_err(|err| named(settings, &err))
        })
        .transpose()
    });
    let state = match read {
        Ok(Some(state)) => state,
        Ok(None) => return,
        Err(error) => {
            warn!(%agent, %error, "agent_hooks: user settings unreadable at start");
            return;
        }
    };
    match state {
        UserHooksState::Off => {}
        UserHooksState::On | UserHooksState::Partial => match (site.ensure_shim)() {
            Ok(()) => info!(
                %agent,
                ?state,
                shim = %site.shim.display(),
                "agent_hooks: hook shim refreshed at start"
            ),
            Err(err) => warn!(%agent, ?err, "agent_hooks: hook shim not refreshed at start"),
        },
        UserHooksState::Stale => match set(plugin, settings, true, site).error {
            None => info!(
                %agent,
                settings = %settings.display(),
                shim = %site.shim.display(),
                "agent_hooks: stale user settings hooks repointed at start"
            ),
            Some(error) => warn!(
                %agent,
                %error,
                "agent_hooks: stale user settings hooks not repointed at start"
            ),
        },
    }
}

/// The wire form of a plugin's state.
fn wire_state(state: UserHooksState) -> UserHooksWire {
    match state {
        UserHooksState::Off => UserHooksWire::Off,
        UserHooksState::On => UserHooksWire::On,
        UserHooksState::Partial => UserHooksWire::Partial,
        UserHooksState::Stale => UserHooksWire::Stale,
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::server::test_support::ScratchDir;
    use agent_api::test_agent::TestAgent;
    use std::cell::Cell;
    use std::time::{Duration, SystemTime};

    /// The registered plugin that supports user hooks.
    fn hooks_plugin() -> &'static dyn AgentPlugin {
        agent_registry::all()
            .iter()
            .copied()
            .find(|plugin| plugin.descriptor().caps.user_hooks)
            .expect("a registered agent supports user hooks")
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "it stands in for Site::ensure_shim, which returns a Result"
    )]
    fn no_shim() -> anyhow::Result<()> {
        Ok(())
    }

    /// An ordinary, non-isolated site whose entries run `shim`.
    fn site(shim: PathBuf) -> Site<'static> {
        Site {
            shim,
            elevated: false,
            isolated: false,
            ensure_shim: &no_shim,
        }
    }

    fn display(path: &Path) -> String {
        path.display().to_string()
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read the settings file")
    }

    #[test]
    fn get_reads_off_for_a_missing_file() {
        let scratch = ScratchDir::new("hooks-get-missing");
        let settings = scratch.path().join("settings.json");
        let site = site(scratch.path().join("hook-shim").join("rt-hook"));
        let reply = answer(Some(hooks_plugin()), Some(&settings), Request::Get, &site);
        assert_eq!(
            reply,
            Reply {
                state: UserHooksWire::Off,
                path: Some(display(&settings)),
                error: None,
            }
        );
        assert!(!settings.exists(), "a Get writes nothing");
    }

    #[test]
    fn set_on_creates_the_file_and_reads_on() {
        let scratch = ScratchDir::new("hooks-set-on");
        let settings = scratch.path().join("home").join("settings.json");
        let calls = Cell::new(0);
        let ensure = || {
            calls.set(calls.get() + 1);
            Ok(())
        };
        let site = Site {
            ensure_shim: &ensure,
            ..site(scratch.path().join("hook-shim").join("rt-hook"))
        };
        let reply = answer(
            Some(hooks_plugin()),
            Some(&settings),
            Request::Set(true),
            &site,
        );
        assert_eq!(reply.error, None);
        assert_eq!(reply.state, UserHooksWire::On);
        assert_eq!(reply.path, Some(display(&settings)));
        assert_eq!(calls.get(), 1, "the shim is refreshed before the install");
        assert!(settings.is_file(), "the file and its folder are created");
        let get = answer(Some(hooks_plugin()), Some(&settings), Request::Get, &site);
        assert_eq!(get.state, UserHooksWire::On);
        assert!(
            !backup_path(&settings).exists(),
            "a new file has nothing to back up"
        );
    }

    #[test]
    fn set_on_backs_up_an_existing_file_once() {
        let scratch = ScratchDir::new("hooks-backup");
        let settings = scratch.path().join("settings.json");
        let original = "{\r\n    \"theme\": \"dark\"\r\n}\r\n";
        std::fs::write(&settings, original).expect("write the user's file");
        let site = site(scratch.path().join("hook-shim").join("rt-hook"));
        let plugin = Some(hooks_plugin());

        let on = answer(plugin, Some(&settings), Request::Set(true), &site);
        assert_eq!((on.state, on.error), (UserHooksWire::On, None));
        let backup = backup_path(&settings);
        assert_eq!(
            std::fs::read(&backup).expect("the backup exists"),
            original.as_bytes()
        );

        let off = answer(plugin, Some(&settings), Request::Set(false), &site);
        assert_eq!((off.state, off.error), (UserHooksWire::Off, None));
        std::fs::write(&settings, "{\"theme\": \"light\"}\n").expect("the user edits the file");
        let again = answer(plugin, Some(&settings), Request::Set(true), &site);
        assert_eq!((again.state, again.error), (UserHooksWire::On, None));
        assert_eq!(
            std::fs::read(&backup).expect("the backup still exists"),
            original.as_bytes(),
            "an existing backup is never overwritten"
        );
    }

    #[test]
    fn set_on_twice_writes_once() {
        let scratch = ScratchDir::new("hooks-twice");
        let settings = scratch.path().join("settings.json");
        let site = site(scratch.path().join("hook-shim").join("rt-hook"));
        let plugin = Some(hooks_plugin());
        let first = answer(plugin, Some(&settings), Request::Set(true), &site);
        assert_eq!((first.state, first.error), (UserHooksWire::On, None));
        let bytes = std::fs::read(&settings).expect("read the installed file");
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        std::fs::File::options()
            .write(true)
            .open(&settings)
            .and_then(|file| file.set_modified(old))
            .expect("age the file");

        let second = answer(plugin, Some(&settings), Request::Set(true), &site);
        assert_eq!((second.state, second.error), (UserHooksWire::On, None));
        assert_eq!(std::fs::read(&settings).expect("read again"), bytes);
        let modified = std::fs::metadata(&settings)
            .and_then(|meta| meta.modified())
            .expect("read the mtime");
        assert_eq!(modified, old, "an unchanged edit writes nothing");
    }

    #[test]
    fn set_off_removes_only_ours() {
        let scratch = ScratchDir::new("hooks-off");
        let settings = scratch.path().join("settings.json");
        let user = r#"{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "my-tool"
          }
        ]
      }
    ]
  }
}
"#;
        std::fs::write(&settings, user).expect("write the user's file");
        let shim = scratch.path().join("hook-shim").join("rt-hook");
        let site = site(shim.clone());
        let plugin = Some(hooks_plugin());
        let on = answer(plugin, Some(&settings), Request::Set(true), &site);
        assert_eq!((on.state, on.error), (UserHooksWire::On, None));

        let off = answer(plugin, Some(&settings), Request::Set(false), &site);
        assert_eq!((off.state, off.error), (UserHooksWire::Off, None));
        let text = read(&settings);
        let left: serde_json::Value = serde_json::from_str(&text).expect("still JSON");
        let want: serde_json::Value = serde_json::from_str(user).expect("fixture JSON");
        assert_eq!(left, want, "the user's own hook stays: {text}");
        assert!(
            !text.contains("rt-hook"),
            "no entry of ours is left: {text}"
        );
    }

    #[test]
    fn set_on_refuses_a_file_that_does_not_parse_and_leaves_it() {
        let scratch = ScratchDir::new("hooks-unparsed");
        let settings = scratch.path().join("settings.json");
        let broken = "{\n  \"theme\": \"dark\", // mine\n}\n";
        std::fs::write(&settings, broken).expect("write the user's file");
        let calls = Cell::new(0);
        let ensure = || {
            calls.set(calls.get() + 1);
            Ok(())
        };
        let site = Site {
            ensure_shim: &ensure,
            ..site(scratch.path().join("hook-shim").join("rt-hook"))
        };
        let reply = answer(
            Some(hooks_plugin()),
            Some(&settings),
            Request::Set(true),
            &site,
        );
        assert_eq!(reply.state, UserHooksWire::Off);
        let error = reply.error.expect("the refusal is reported");
        assert!(
            error.starts_with(&settings.display().to_string()),
            "the error names the file: {error}"
        );
        assert_eq!(read(&settings), broken, "the file is left as it was");
        assert!(!backup_path(&settings).exists(), "nothing is backed up");

        let get = answer(Some(hooks_plugin()), Some(&settings), Request::Get, &site);
        assert_eq!(get.state, UserHooksWire::Off);
        assert!(
            get.error
                .is_some_and(|error| error.contains(&settings.display().to_string())),
            "a Get reports the parse error too"
        );
    }

    #[test]
    fn a_file_changed_during_the_edit_is_recomputed() {
        let scratch = ScratchDir::new("hooks-cas");
        let settings = scratch.path().join("settings.json");
        std::fs::write(&settings, "{\"theme\": \"dark\"}\n").expect("write the user's file");
        let shim = scratch.path().join("hook-shim").join("rt-hook");
        let plugin = hooks_plugin();
        let calls = Cell::new(0);
        edit_settings(&settings, |text| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                // The user's editor saves while the edit is being computed.
                std::fs::write(&settings, "{\"theme\": \"light\"}\n").expect("the user saves");
            }
            plugin.install_user_hooks(text, &shim).map(Some)
        })
        .expect("the edit is recomputed from the new text");
        assert_eq!(calls.get(), 2, "the edit is computed again, once");
        let text = read(&settings);
        assert!(text.contains("light"), "the user's save is kept: {text}");
        assert_eq!(
            plugin.user_hooks_state(Some(&text), &shim).expect("state"),
            agent_api::UserHooksState::On
        );

        std::fs::write(&settings, "{}\n").expect("the user clears the file");
        let calls = Cell::new(0);
        let error = edit_settings(&settings, |text| {
            calls.set(calls.get() + 1);
            std::fs::write(&settings, format!("{{\"n\": {}}}\n", calls.get()))
                .expect("the user saves again");
            plugin.install_user_hooks(text, &shim).map(Some)
        })
        .expect_err("a file that keeps changing is refused");
        assert_eq!(
            error,
            format!(
                "{} changed while it was being edited; try again",
                settings.display()
            )
        );
    }

    #[test]
    fn a_refused_write_removes_the_backup_it_made() {
        let scratch = ScratchDir::new("hooks-backup-refused");
        let settings = scratch.path().join("settings.json");
        let shim = scratch.path().join("hook-shim").join("rt-hook");
        let plugin = hooks_plugin();
        let backup = backup_path(&settings);
        // The user's editor saves on every read, so the swap is refused.
        let keeps_changing = |settings: &Path| {
            let calls = Cell::new(0);
            edit_settings(settings, |text| {
                calls.set(calls.get() + 1);
                std::fs::write(settings, format!("{{\"n\": {}}}\n", calls.get()))
                    .expect("the user saves");
                plugin.install_user_hooks(text, &shim).map(Some)
            })
            .expect_err("a file that keeps changing is refused")
        };

        std::fs::write(&settings, "{}\n").expect("write the user's file");
        keeps_changing(&settings);
        assert!(
            !backup.exists(),
            "a backup this call made is removed when nothing was written"
        );

        std::fs::write(&backup, "the first backup").expect("an earlier backup");
        std::fs::write(&settings, "{}\n").expect("write the user's file");
        keeps_changing(&settings);
        assert_eq!(
            std::fs::read_to_string(&backup).expect("the earlier backup stays"),
            "the first backup",
            "a backup that existed before the call is untouched"
        );
    }

    #[test]
    fn a_symlinked_settings_file_is_refused() {
        let scratch = ScratchDir::new("hooks-symlink");
        let target = scratch.path().join("real-settings.json");
        let user = "{\"theme\": \"dark\"}\n";
        std::fs::write(&target, user).expect("write the linked file");
        let settings = scratch.path().join("settings.json");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &settings).expect("make the link");
        #[cfg(windows)]
        match std::os::windows::fs::symlink_file(&target, &settings) {
            // Without Developer Mode or the symlink privilege
            // (ERROR_PRIVILEGE_NOT_HELD) Windows refuses to make the link;
            // there is nothing to test then.
            Err(err)
                if err.kind() == std::io::ErrorKind::PermissionDenied
                    || err.raw_os_error() == Some(1314) =>
            {
                return;
            }
            made => made.expect("make the link"),
        }
        let site = site(scratch.path().join("hook-shim").join("rt-hook"));
        let plugin = Some(hooks_plugin());

        let refused = answer(plugin, Some(&settings), Request::Set(true), &site);
        assert_eq!(
            refused.error,
            Some(format!(
                "{} is a link; edit the file it points to, or replace the link with a file",
                settings.display()
            ))
        );
        assert_eq!(refused.state, UserHooksWire::Off);
        assert!(
            std::fs::symlink_metadata(&settings)
                .expect("the link is still there")
                .file_type()
                .is_symlink(),
            "the link stays a link"
        );
        assert_eq!(read(&target), user, "the linked file is untouched");
        assert!(!backup_path(&settings).exists(), "nothing is backed up");

        let get = answer(plugin, Some(&settings), Request::Get, &site);
        assert_eq!((get.state, get.error), (UserHooksWire::Off, None));
    }

    #[test]
    fn isolated_when_the_config_dir_is_overridden() {
        assert!(is_isolated(Some(OsString::from("X:/x/.tmp/c"))));
    }

    #[test]
    fn not_isolated_when_unset_or_empty() {
        assert!(!is_isolated(None));
        assert!(!is_isolated(Some(OsString::new())));
    }

    #[test]
    fn an_isolated_daemon_refuses_a_settings_file_outside_tmp() {
        let root = if cfg!(windows) { "C:\\" } else { "/" };
        let outside = PathBuf::from(root)
            .join(format!("rt-hooks-outside-{}", std::process::id()))
            .join("settings.json");
        let scratch = ScratchDir::new("hooks-isolated");
        let site = Site {
            isolated: true,
            ..site(scratch.path().join("hook-shim").join("rt-hook"))
        };
        let plugin = Some(hooks_plugin());
        let refused = answer(plugin, Some(&outside), Request::Set(true), &site);
        assert_eq!(
            refused,
            Reply {
                state: UserHooksWire::Off,
                path: Some(display(&outside)),
                error: Some(ISOLATED_REFUSAL.to_owned()),
            }
        );
        assert!(!outside.exists(), "nothing is written outside .tmp");
        let get = answer(plugin, Some(&outside), Request::Get, &site);
        assert_eq!((get.state, get.error), (UserHooksWire::Off, None));

        let inside = scratch.path().join(".tmp").join("settings.json");
        let allowed = answer(plugin, Some(&inside), Request::Set(true), &site);
        assert_eq!((allowed.state, allowed.error), (UserHooksWire::On, None));
    }

    #[test]
    fn an_unsupported_agent_reads_unsupported() {
        let scratch = ScratchDir::new("hooks-unsupported");
        let site = site(scratch.path().join("hook-shim").join("rt-hook"));
        let agent = TestAgent::default();
        let plugin: &dyn AgentPlugin = &agent;
        let settings = plugin.user_settings_path();
        for request in [Request::Get, Request::Set(true), Request::Set(false)] {
            assert_eq!(
                answer(Some(plugin), settings.as_deref(), request, &site),
                Reply::unsupported()
            );
        }
        let unknown = scratch.path().join("settings.json");
        assert_eq!(
            answer(None, Some(&unknown), Request::Set(true), &site),
            Reply::unsupported(),
            "an unknown agent reads unsupported"
        );
        assert!(!unknown.exists());
    }

    #[tokio::test]
    async fn dispatch_routes_get_agent_user_hooks() {
        let (hub, _scratch) = crate::server::test_support::test_hub("hooks-dispatch");
        let reply = crate::server::test_support::dispatch_one(
            &hub,
            ClientMessage::GetAgentUserHooks {
                agent: protocol::AgentId("no-such-agent".to_owned()),
                request_id: Some("r9".to_owned()),
            },
        )
        .await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::AgentUserHooks {
                    agent,
                    state: UserHooksWire::Unsupported,
                    path: None,
                    error: None,
                    request_id: Some(request_id),
                } if agent.0 == "no-such-agent" && request_id == "r9"
            ),
            "{reply:?}"
        );
    }

    #[test]
    fn startup_refresh_repoints_a_stale_install() {
        let scratch = ScratchDir::new("hooks-refresh-stale");
        let settings = scratch.path().join("settings.json");
        let plugin = hooks_plugin();
        let old = scratch.path().join("old-data").join("rt-hook");
        let installed = plugin
            .install_user_hooks(Some("{\"theme\": \"dark\"}\n"), &old)
            .expect("install with the old shim");
        std::fs::write(&settings, installed).expect("write the stale install");
        let shim = scratch.path().join("hook-shim").join("rt-hook");
        assert_eq!(
            plugin
                .user_hooks_state(Some(&read(&settings)), &shim)
                .expect("state"),
            agent_api::UserHooksState::Stale
        );
        let calls = Cell::new(0);
        let ensure = || {
            calls.set(calls.get() + 1);
            Ok(())
        };
        let site = Site {
            ensure_shim: &ensure,
            ..site(shim.clone())
        };

        refresh_one(plugin, &settings, &site);

        assert!(calls.get() >= 1, "the shim is refreshed");
        let text = read(&settings);
        assert_eq!(
            plugin.user_hooks_state(Some(&text), &shim).expect("state"),
            agent_api::UserHooksState::On,
            "the entries now run the current shim: {text}"
        );
        assert!(text.contains("dark"), "the user's settings stay: {text}");
    }

    #[test]
    fn startup_refresh_leaves_an_off_file_alone() {
        let scratch = ScratchDir::new("hooks-refresh-off");
        let settings = scratch.path().join("settings.json");
        let user = "{\n  \"theme\": \"dark\"\n}\n";
        std::fs::write(&settings, user).expect("write the user's file");
        let calls = Cell::new(0);
        let ensure = || {
            calls.set(calls.get() + 1);
            Ok(())
        };
        let site = Site {
            ensure_shim: &ensure,
            ..site(scratch.path().join("hook-shim").join("rt-hook"))
        };

        refresh_one(hooks_plugin(), &settings, &site);
        let missing = scratch.path().join("none").join("settings.json");
        refresh_one(hooks_plugin(), &missing, &site);

        assert_eq!(calls.get(), 0, "no shim for a user who never turned it on");
        assert_eq!(read(&settings), user, "the file is untouched");
        assert!(!backup_path(&settings).exists(), "nothing is backed up");
        assert!(!missing.exists(), "a missing file is not created");
    }

    #[test]
    fn an_elevated_daemon_reads_unsupported() {
        let scratch = ScratchDir::new("hooks-elevated");
        let settings = scratch.path().join("settings.json");
        let site = Site {
            elevated: true,
            ..site(scratch.path().join("hook-shim").join("rt-hook"))
        };
        for request in [Request::Get, Request::Set(true)] {
            assert_eq!(
                answer(Some(hooks_plugin()), Some(&settings), request, &site),
                Reply::unsupported()
            );
        }
        assert!(!settings.exists(), "an elevated daemon writes nothing");
    }
}
