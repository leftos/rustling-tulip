//! The hook files a spawn's plugin plans — for Claude, the `--settings`
//! document it passes — with writing, discarding and naming them.

use crate::paths::Dirs;
use crate::server::Hub;
use crate::{orphan, pty_state, tracer_client};
use agent_api::{AgentPlugin, HookFile, HookPlan, HookShim};
use protocol::{AgentId, OptionValues};
use std::path::{Component, Path, PathBuf};
use tracing::warn;

/// The hook files one spawn's plugin planned and the daemon wrote — an
/// interactive Claude session or a plain-shell recovery — and what it made on
/// disk.
pub(super) struct HookSettings {
    /// Every file the plan named, as written, in the plan's order; empty for
    /// a plan that writes no file.
    files: Vec<PathBuf>,
    /// The args the plan carries, each `{file:<name>}` replaced by the file's
    /// path: what the spawn appends to its argv.
    args: Vec<String>,
    /// The cached `rt-tracer` the plan's hook command names. This is the same
    /// path the spawn runs its tracer from, so the file and the supervisor
    /// cannot disagree about which copy runs the shim.
    pub(super) tracer_exe: PathBuf,
    /// True when writing the files created `sessions/<id>/`; a spawn that then
    /// fails removes only a directory it made.
    created_dir: bool,
}

impl HookSettings {
    /// Every file written, in the plan's order; empty for an agent whose
    /// hooks are argv alone. What a recovered shell's resume command names.
    pub(super) fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// The argv these hooks add to the spawn.
    pub(super) fn args(&self) -> &[String] {
        &self.args
    }

    /// Remove what this spawn wrote after the tracer spawn that would have run
    /// it failed: always the files, and the directory only when this spawn
    /// created it, so no sidecar-less `sessions/<id>/` is left behind for the
    /// orphan scan to skip forever.
    pub(super) fn discard(self, session_id: &str) {
        let dir = self
            .files
            .first()
            .and_then(|path| path.parent())
            .map(Path::to_path_buf);
        for path in self.files {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => warn!(
                    %session_id,
                    path = %path.display(),
                    error = %format!("{err:#}"),
                    "removing the hook settings file of a failed spawn failed"
                ),
            }
        }
        if self.created_dir
            && let Some(dir) = dir
        {
            remove_session_dir_if_empty(&dir, session_id);
        }
    }
}

/// Remove the session directory `dir`, best-effort: only a directory this
/// daemon made for the failed spawn is a candidate, and a non-empty one
/// (another writer got there first) is left alone.
fn remove_session_dir_if_empty(dir: &Path, session_id: &str) {
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

/// The file name a spawn's injected Claude hook settings land under, inside the
/// session's own directory.
const HOOK_SETTINGS_FILE: &str = "hook-settings.json";

/// `session_id`'s injected hook settings file, which a Claude spawn passes as
/// `--settings` and the hook shim reads.
pub(in crate::server) fn hook_settings_path(dirs: &Dirs, session_id: &str) -> PathBuf {
    dirs.sessions_dir.join(session_id).join(HOOK_SETTINGS_FILE)
}

/// Write the new session's hook files from its plugin's plan and answer what
/// to pass on its argv, or `None` when this spawn gets no hooks.
///
/// The plugin decides: [`AgentPlugin::hook_plan`] answers the files to write
/// and the args to append, and a plugin that takes no hooks plans neither. A
/// cached tracer binary that cannot be resolved, a plan that does not decode,
/// a plan naming no files, a file name that could name a path, or a failed
/// write turns hooks off for this spawn with a `warn!` and never refuses it: a
/// session without them falls back to the terminal heuristic. The files are
/// written eagerly, before whatever names them (an interactive spawn's argv, a
/// recovered shell's resume command), into the session's own `sessions/<id>/`,
/// creating that directory before `meta.json`'s writer would. A graceful stop
/// deletes the sidecar and this file, and the directory itself when that
/// leaves it empty; a natural exit keeps the file. A spawn that then fails
/// removes what this call made (see [`HookSettings::discard`]).
pub(super) fn write_hook_settings(
    hub: &Hub,
    session_id: &str,
    agent: &AgentId,
    options: &OptionValues,
    trusted: bool,
) -> Option<HookSettings> {
    let Some(plugin) = agent_registry::plugin(agent) else {
        warn!(
            %session_id,
            "no agent plugin `{}` is registered; the session gets no hook files",
            agent.0
        );
        return None;
    };
    write_hook_files(hub, session_id, plugin, options, trusted)
}

/// [`write_hook_settings`] with `plugin`'s hook plan.
pub(super) fn write_hook_files(
    hub: &Hub,
    session_id: &str,
    plugin: &dyn AgentPlugin,
    options: &OptionValues,
    trusted: bool,
) -> Option<HookSettings> {
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
    let shim = HookShim {
        tracer_exe: &tracer_exe,
        session_id,
        config_dir: &hub.dirs.config,
    };
    let plan = match plugin.hook_plan(options, trusted, &shim) {
        Ok(plan) => plan,
        Err(err) => {
            warn!(
                %session_id,
                agent = %plugin.descriptor().label,
                error = %format!("{err:#}"),
                "the agent's hook plan could not be read; spawning without hooks"
            );
            return None;
        }
    };
    if plan.files.is_empty() && plan.args.is_empty() {
        return None;
    }
    let dir = hub.dirs.sessions_dir.join(session_id);
    // A plan that writes no file never creates the session directory, so an
    // argv-only agent leaves nothing on disk to discard.
    let writes_files = !plan.files.is_empty();
    let created_dir = writes_files && !dir.exists();
    let written = if writes_files {
        std::fs::create_dir_all(&dir)
            .map_err(anyhow::Error::from)
            .and_then(|()| write_plan(&dir, &plan, tracer_exe, created_dir))
    } else {
        write_plan(&dir, &plan, tracer_exe, created_dir)
    };
    match written {
        Ok(settings) => Some(settings),
        Err(err) => {
            warn!(
                %session_id,
                path = %dir.display(),
                error = %format!("{err:#}"),
                "writing the hook settings file failed; spawning without hooks"
            );
            // The spawn continues without hooks, so release a directory this
            // call made; a later success on this path can create it again.
            if created_dir {
                remove_session_dir_if_empty(&dir, session_id);
            }
            None
        }
    }
}

/// Write every file `plan` names into `dir` and answer the settings the spawn
/// carries: the files it wrote and the plan's args with each `{file:<name>}`
/// replaced by the written path.
///
/// `dir` must already exist when the plan names a file; a plan naming none
/// writes nothing and is never refused. A name that could name a path is
/// refused before anything is written, and a failed write removes the files
/// already written, so a refused plan leaves nothing behind.
fn write_plan(
    dir: &Path,
    plan: &HookPlan,
    tracer_exe: PathBuf,
    created_dir: bool,
) -> anyhow::Result<HookSettings> {
    if let Some(bad) = plan
        .files
        .iter()
        .find(|file| !is_plain_json_name(&file.name))
    {
        anyhow::bail!(
            "the hook file name {:?} is not a plain .json file name",
            bad.name
        );
    }
    let mut paths: Vec<PathBuf> = Vec::with_capacity(plan.files.len());
    for file in &plan.files {
        let path = dir.join(&file.name);
        if let Err(err) = orphan::write_atomic(&path, file.contents.as_bytes()) {
            for written in &paths {
                if let Err(err) = std::fs::remove_file(written) {
                    warn!(
                        path = %written.display(),
                        error = %format!("{err:#}"),
                        "removing a partially written hook file failed"
                    );
                }
            }
            return Err(err);
        }
        paths.push(path);
    }
    let args = plan
        .args
        .iter()
        .map(|arg| substitute(arg, &plan.files, &paths))
        .collect();
    Ok(HookSettings {
        files: paths,
        args,
        tracer_exe,
        created_dir,
    })
}

/// Whether `name` is a plain `.json` file name a plan may write into the
/// session's directory: one file-name component, never a path, a parent hop or
/// a drive-relative name that would escape the directory.
fn is_plain_json_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("..")
        && Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        && matches!(
            Path::new(name).components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(_)]
        )
}

/// `arg` with every `{file:<name>}` replaced by that file's written path.
///
/// The replacement is plain string replacement: the daemon adds no quoting, so
/// a plugin whose argv needs a quoted path writes the quotes around its own
/// placeholder.
fn substitute(arg: &str, files: &[HookFile], paths: &[PathBuf]) -> String {
    let mut out = arg.to_string();
    for (file, path) in files.iter().zip(paths) {
        out = out.replace(&format!("{{file:{}}}", file.name), &path.to_string_lossy());
    }
    out
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

    /// Claude's options as a spawn request names them.
    fn claude_options() -> OptionValues {
        OptionValues::from([
            ("permission_mode".to_owned(), protocol::OptionValue::Null),
            (
                "provider".to_owned(),
                protocol::OptionValue::Text("anthropic".to_owned()),
            ),
            ("model".to_owned(), protocol::OptionValue::Null),
        ])
    }

    #[test]
    fn a_plan_file_path_replaces_its_placeholder_in_the_args() {
        let (hub, _scratch) = spawnless_test_hub("hook-plan-substitute");
        let dir = hub.dirs.sessions_dir.join("plan");
        std::fs::create_dir_all(&dir).expect("create the plan dir");
        let plan = HookPlan {
            files: vec![HookFile {
                name: "a.json".to_string(),
                contents: "{}".to_string(),
            }],
            args: vec!["--x".to_string(), "{file:a.json}".to_string()],
        };
        let written =
            write_plan(&dir, &plan, PathBuf::new(), false).expect("a plain file name writes");
        assert_eq!(
            written.args(),
            [
                "--x".to_string(),
                dir.join("a.json").to_string_lossy().into_owned(),
            ],
            "the placeholder becomes the written path, unquoted"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("a.json")).expect("the file is readable"),
            "{}"
        );
    }

    #[test]
    fn a_plan_file_name_with_a_separator_is_refused() {
        let (hub, _scratch) = spawnless_test_hub("hook-plan-refuse");
        let dir = hub.dirs.sessions_dir.join("plan");
        std::fs::create_dir_all(&dir).expect("create the plan dir");
        for name in [r"..\a.json", "../a.json", "..a.json", ""] {
            let plan = HookPlan {
                files: vec![HookFile {
                    name: name.to_string(),
                    contents: "{}".to_string(),
                }],
                args: vec!["{file:a.json}".to_string()],
            };
            assert!(
                write_plan(&dir, &plan, PathBuf::new(), false).is_err(),
                "{name:?} names a path and is refused"
            );
        }
        assert_eq!(
            std::fs::read_dir(&dir)
                .expect("the plan dir is readable")
                .count(),
            0,
            "a refused plan writes no file"
        );
    }

    #[test]
    fn a_plan_file_name_with_a_drive_prefix_is_refused() {
        let (hub, _scratch) = spawnless_test_hub("hook-plan-drive");
        let dir = hub.dirs.sessions_dir.join("plan");
        std::fs::create_dir_all(&dir).expect("create the plan dir");
        let plan = HookPlan {
            files: vec![HookFile {
                name: "C:hooks.json".to_string(),
                contents: "{}".to_string(),
            }],
            args: vec!["--settings".to_string(), "{file:C:hooks.json}".to_string()],
        };
        assert!(
            write_plan(&dir, &plan, PathBuf::new(), false).is_err(),
            "a drive-relative name escapes the session dir and is refused"
        );
        assert_eq!(
            std::fs::read_dir(&dir)
                .expect("the plan dir is readable")
                .count(),
            0,
            "no file and no hook args"
        );
    }

    #[test]
    fn an_args_only_plan_passes_its_args_and_writes_no_file() {
        let (hub, _scratch) = spawnless_test_hub("hook-plan-args-only");
        let dir = hub.dirs.sessions_dir.join("plan");
        std::fs::create_dir_all(&dir).expect("create the plan dir");
        let plan = HookPlan {
            files: Vec::new(),
            args: vec!["--hooks".to_string(), "x".to_string()],
        };
        let settings = write_plan(&dir, &plan, PathBuf::new(), false)
            .expect("a plan that writes no file is not refused");
        assert_eq!(
            settings.args(),
            ["--hooks".to_string(), "x".to_string()],
            "the args pass through with no placeholder to replace"
        );
        assert!(
            settings.files().is_empty(),
            "an args-only plan writes no settings file"
        );
        assert_eq!(
            std::fs::read_dir(&dir)
                .expect("the plan dir is readable")
                .count(),
            0,
            "nothing was written"
        );
    }

    #[test]
    fn hook_settings_are_off_without_a_cached_tracer() {
        let (hub, _scratch) = spawnless_test_hub("hook-settings-no-tracer");
        assert!(
            write_hook_settings(
                &hub,
                "no-tracer",
                &agent_registry::default_agent(),
                &claude_options(),
                false
            )
            .is_none(),
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
        let settings = write_hook_settings(
            &hub,
            "claude-session",
            &agent_registry::default_agent(),
            &claude_options(),
            false,
        )
        .expect("a cached tracer resolves");
        assert!(settings.created_dir, "the session dir did not exist before");
        let file = hook_settings_file(&hub, "claude-session");
        assert_eq!(
            settings.files(),
            std::slice::from_ref(&file),
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
        let settings = write_hook_settings(
            &hub,
            "pre-made",
            &agent_registry::default_agent(),
            &claude_options(),
            false,
        )
        .expect("writes");
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
