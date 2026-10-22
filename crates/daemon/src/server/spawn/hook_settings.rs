//! The hook settings file an interactive Claude spawn passes as `--settings`: writing, discarding and naming it, and the shell resume line that carries it.

use crate::paths::Dirs;
use crate::server::Hub;
use crate::{history, orphan, pty_state, tracer_client};
use protocol::Agent;
use std::path::{Path, PathBuf};
use tracing::warn;

/// The injected hook settings one spawn wrote — an interactive Claude session
/// or a plain-shell recovery — and what it made on disk.
pub(super) struct HookSettings {
    /// The `--settings` file's path.
    path: PathBuf,
    /// The cached `rt-tracer` the file's hook command names. This is the same
    /// path the spawn runs its tracer from, so the file and the supervisor
    /// cannot disagree about which copy runs the shim.
    pub(super) tracer_exe: PathBuf,
    /// True when writing the file created `sessions/<id>/`; a spawn that then
    /// fails removes only a directory it made.
    created_dir: bool,
}

impl HookSettings {
    /// The file the spawn passes as `--settings`.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Remove what this spawn wrote after the tracer spawn that would have run
    /// it failed: always the file, and the directory only when this spawn
    /// created it, so no sidecar-less `sessions/<id>/` is left behind for the
    /// orphan scan to skip forever.
    pub(super) fn discard(self, session_id: &str) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => warn!(
                %session_id,
                path = %self.path.display(),
                error = %format!("{err:#}"),
                "removing the hook settings file of a failed spawn failed"
            ),
        }
        if self.created_dir {
            remove_session_dir_if_empty(&self.path, session_id);
        }
    }
}

/// Remove the session directory `path` sits in, best-effort: only a directory
/// this daemon made for the failed spawn is a candidate, and a non-empty one
/// (another writer got there first) is left alone.
fn remove_session_dir_if_empty(path: &Path, session_id: &str) {
    let Some(dir) = path.parent() else {
        return;
    };
    match std::fs::remove_dir(dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => warn!(
            %session_id,
            path = %dir.display(),
            error = %format!("{err:#}"),
            "removing the session dir of a failed spawn failed"
        ),
    }
}

/// Remove a gracefully stopped session's hook settings file, then its session
/// directory when that file was all it still held. A natural exit keeps the
/// file (see [`write_hook_settings`]); a session that never got hooks — one
/// whose settings write failed — has no file to remove, which is not an error. A directory still holding `scrollback.bin` is left in place.
pub(in crate::server) fn remove_hook_settings(hub: &Hub, session_id: &str) {
    let path = hook_settings_path(&hub.dirs, session_id);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => warn!(
            %session_id,
            path = %path.display(),
            error = %format!("{err:#}"),
            "removing a stopped session's hook settings failed"
        ),
    }
    let Some(dir) = path.parent() else {
        return;
    };
    match std::fs::remove_dir(dir) {
        Ok(()) => {}
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(err) => warn!(
            %session_id,
            path = %dir.display(),
            error = %format!("{err:#}"),
            "removing a stopped session's empty directory failed"
        ),
    }
}

/// Render a recovered shell's typed command for `settings`: the settings
/// clause names the quoted path when the spawn wrote a file, and is dropped
/// entirely otherwise.
///
/// The path is always double-quoted, since the config dir can hold spaces. A
/// path holding a double quote cannot be quoted for cmd and POSIX shells at
/// once, so it renders the hookless form a missing file does: the shell parses
/// the command either way.
pub(crate) fn render_shell_resume(content: &str, settings: Option<&Path>) -> String {
    match settings {
        Some(path) if !path.to_string_lossy().contains('"') => content.replace(
            history::HOOK_SETTINGS_SLOT,
            &format!("\"{}\"", path.display()),
        ),
        Some(_) | None => {
            content.replace(&format!("--settings {} ", history::HOOK_SETTINGS_SLOT), "")
        }
    }
}

/// The file name a spawn's injected Claude hook settings land under, inside the
/// session's own directory.
const HOOK_SETTINGS_FILE: &str = "hook-settings.json";

/// `session_id`'s injected hook settings file, which a Claude spawn passes as
/// `--settings` and the hook shim reads.
pub(in crate::server) fn hook_settings_path(dirs: &Dirs, session_id: &str) -> PathBuf {
    dirs.sessions_dir.join(session_id).join(HOOK_SETTINGS_FILE)
}

/// Write the new session's injected hook settings file and return what to pass
/// as `--settings`, or `None` when this spawn gets no hooks.
///
/// Only an interactive Claude spawn and a plain-shell recovery (a spawn
/// carrying a [`protocol::InjectorStartup::ShellPrompt`] injector) get hooks.
/// A cached tracer binary that cannot be resolved, or a failed write, turns
/// hooks off for this spawn with a `warn!` and never refuses it: a session
/// without them falls back to the terminal heuristic. The file is written
/// eagerly, before whatever names it (an interactive spawn's argv, a
/// recovery's typed command), into the session's own `sessions/<id>/`,
/// creating that directory before `meta.json`'s writer would. A graceful stop
/// deletes the sidecar and this file, and the directory itself when that
/// leaves it empty; a natural exit keeps the file. A spawn that then fails
/// removes what this call made (see [`HookSettings::discard`]).
pub(super) fn write_hook_settings(
    hub: &Hub,
    session_id: &str,
    agent: Agent,
) -> Option<HookSettings> {
    if agent != Agent::Claude {
        return None;
    }
    let tracer_exe = match tracer_client::cached_tracer_exe(&hub.dirs) {
        Ok(path) => path,
        Err(err) => {
            warn!(
                %session_id,
                error = %format!("{err:#}"),
                "resolving the cached tracer for the hook settings failed; spawning without hooks"
            );
            return None;
        }
    };
    let dir = hub.dirs.sessions_dir.join(session_id);
    let path = dir.join(HOOK_SETTINGS_FILE);
    let created_dir = !dir.exists();
    let settings = crate::agents::build_hook_settings(&tracer_exe, session_id, &hub.dirs.config);
    let written = std::fs::create_dir_all(&dir)
        .map_err(anyhow::Error::from)
        .and_then(|()| orphan::write_atomic(&path, settings.as_bytes()));
    if let Err(err) = written {
        warn!(
            %session_id,
            path = %path.display(),
            error = %format!("{err:#}"),
            "writing the hook settings file failed; spawning without hooks"
        );
        // The spawn continues without hooks, so release a directory this call
        // made; a later success on this path can create it again.
        if created_dir {
            remove_session_dir_if_empty(&path, session_id);
        }
        return None;
    }
    Some(HookSettings {
        path,
        tracer_exe,
        created_dir,
    })
}

/// Announce a replayed prompt's attention, once its session is published and
/// only when the record still holds the prompt after the whole replay.
pub(in crate::server) fn announce_replayed_prompt(hub: &Hub, session_id: &str, awaiting: bool) {
    if awaiting {
        pty_state::announce_awaiting_input(session_id, &hub.attention_tx);
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::server::test_support::{hook_settings_file, hook_test_hub, spawnless_test_hub};
    #[test]
    fn hook_settings_are_off_without_a_cached_tracer() {
        let (hub, _scratch) = spawnless_test_hub("hook-settings-no-tracer");
        assert!(
            write_hook_settings(&hub, "no-tracer", Agent::Claude).is_none(),
            "hooks are off when the cached tracer cannot be resolved"
        );
        let file = hook_settings_file(&hub, "no-tracer");
        assert_eq!(
            file.file_name().and_then(|name| name.to_str()),
            Some("hook-settings.json")
        );
        assert!(!file.exists(), "no settings file was written");
        assert!(
            !hub.dirs.sessions_dir.join("no-tracer").exists(),
            "no session dir was created either"
        );
    }

    #[test]
    fn hook_settings_name_the_cached_tracer_for_an_interactive_claude() {
        let (hub, _scratch) = hook_test_hub("hook-settings-claude");
        let settings = write_hook_settings(&hub, "claude-session", Agent::Claude)
            .expect("a cached tracer resolves");
        assert!(settings.created_dir, "the session dir did not exist before");
        let file = hook_settings_file(&hub, "claude-session");
        assert_eq!(
            settings.path(),
            file,
            "the spawn passes the file under its literal name"
        );
        let cached = crate::tracer_client::cached_tracer_exe(&hub.dirs).expect("resolves");
        let text = std::fs::read_to_string(&file).expect("the settings file is readable");
        let document: serde_json::Value =
            serde_json::from_str(&text).expect("the settings file is JSON");
        assert_eq!(
            document
                .as_object()
                .expect("a JSON object")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["hooks"],
            "the file holds only a hooks key"
        );
        let hook = &document["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(
            hook["command"].as_str(),
            cached.to_str(),
            "the hook command names the cached tracer the spawn runs"
        );
        assert_eq!(hook["args"][1], "--session");
        assert_eq!(hook["args"][2], "claude-session");
        assert_eq!(hook["args"][3], "--config-dir");
        assert_eq!(
            hook["args"][4],
            hub.dirs.config.to_string_lossy().as_ref(),
            "the shim reads daemon.json from the daemon's config dir"
        );
    }

    #[test]
    fn discarding_hook_settings_keeps_a_session_dir_it_did_not_create() {
        let (hub, _scratch) = hook_test_hub("hook-settings-keep-dir");
        let dir = hub.dirs.sessions_dir.join("pre-made");
        std::fs::create_dir_all(&dir).expect("pre-create the session dir");
        let settings = write_hook_settings(&hub, "pre-made", Agent::Claude).expect("writes");
        assert!(!settings.created_dir, "the dir was already there");
        assert!(hook_settings_file(&hub, "pre-made").exists());
        settings.discard("pre-made");
        assert!(
            !hook_settings_file(&hub, "pre-made").exists(),
            "the file is removed"
        );
        assert!(
            dir.exists(),
            "a directory this spawn did not create is kept"
        );
    }
}
