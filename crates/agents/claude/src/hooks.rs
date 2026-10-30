//! Claude's hooks: the `--settings` document an interactive spawn carries and
//! the argv that names it, and the mapping of the events those hooks report.

use crate::ClaudeOptions;
use agent_api::{HookFile, HookPlan, HookReport, HookShim, hook_vocabulary};
use std::path::Path;

/// What one of Claude's hook payloads says about its session: the shared
/// hook vocabulary's report, plus the permission mismatch a prompt reveals.
pub(crate) fn map_hook_event(
    body: &serde_json::Value,
    spawn: Option<&ClaudeOptions>,
    trusted: bool,
) -> Option<HookReport> {
    let mut report = hook_vocabulary::map(body)?;
    let details = hook_vocabulary::details(body);
    if details.event == "UserPromptSubmit" {
        report.permission_mismatch =
            mode_mismatch(spawn, trusted, details.permission_mode.as_deref());
    }
    Some(report)
}

/// The spawn's permission mode and the hook's report, when they differ.
///
/// Only a spawn that passed `--permission-mode` states an expected mode: a
/// spawn with no flag, or a trusted one running with
/// `--dangerously-skip-permissions`, has nothing to compare against. The
/// CLI's `"auto"` is a mode the daemon never passes, so it never mismatches.
fn mode_mismatch(
    spawn: Option<&ClaudeOptions>,
    trusted: bool,
    reported: Option<&str>,
) -> Option<(String, String)> {
    if trusted {
        return None;
    }
    let mode = spawn?.permission_mode?;
    let reported = reported?;
    if reported == "auto" || reported == mode.as_cli_arg() {
        return None;
    }
    Some((mode.as_cli_arg().to_owned(), reported.to_owned()))
}

/// The name the plan's settings file lands under inside the session's own
/// directory.
const HOOK_SETTINGS_FILE: &str = "hook-settings.json";

/// Claude's hook plan: one settings file holding the injected hooks, passed as
/// `--settings {file:hook-settings.json}` so the daemon writes it into the
/// session's own directory and substitutes its path.
pub(crate) fn hook_plan(shim: &HookShim<'_>) -> HookPlan {
    HookPlan {
        files: vec![HookFile {
            name: HOOK_SETTINGS_FILE.to_string(),
            contents: build_hook_settings(shim.tracer_exe, shim.session_id, shim.config_dir),
        }],
        args: vec![
            "--settings".to_string(),
            format!("{{file:{HOOK_SETTINGS_FILE}}}"),
        ],
    }
}

/// The contents of the `--settings` file an interactive Claude spawn gets: a
/// JSON object holding only a `hooks` key, so every other settings key keeps
/// the user's value and the injected hooks add to the user's and the repo's
/// hooks instead of replacing them.
///
/// Every event of the design's table gets one command hook in exec form
/// (`command` plus `args`, never a shell string) running the session's cached
/// tracer's `hook` shim, marked `async` so a hook never blocks the agent's
/// turn. `tracer_exe` is the cached `rt-tracer.exe` the tracer itself runs from,
/// which stays valid for the session's lifetime, and `config_dir` is the
/// daemon's config dir, where the shim reads `daemon.json` for the current port
/// and auth token.
#[must_use]
fn build_hook_settings(tracer_exe: &Path, session_id: &str, config_dir: &Path) -> String {
    let command = tracer_exe.to_string_lossy().into_owned();
    let config_dir = config_dir.to_string_lossy().into_owned();
    let hook = serde_json::json!({
        "type": "command",
        "command": command,
        "args": ["hook", "--session", session_id, "--config-dir", config_dir],
        "async": true,
    });
    // Only the two matcher-scoped events name a matcher; the rest run for
    // every event of their kind.
    let mut entries: Vec<(String, serde_json::Value)> = [
        "SessionStart",
        "UserPromptSubmit",
        "PostToolUse",
        "PostToolUseFailure",
        "PermissionRequest",
        "Elicitation",
        "Stop",
        "StopFailure",
        "SessionEnd",
        "MessageDisplay",
    ]
    .into_iter()
    .map(|event| {
        (
            event.to_string(),
            serde_json::json!([{ "hooks": [hook.clone()] }]),
        )
    })
    .collect();
    entries.extend([
        (
            "PreToolUse".to_string(),
            serde_json::json!([{
                "matcher": "AskUserQuestion|ExitPlanMode",
                "hooks": [hook.clone()],
            }]),
        ),
        (
            "Notification".to_string(),
            serde_json::json!([{
                "matcher": "permission_prompt|elicitation_dialog|elicitation_url_dialog|idle_prompt",
                "hooks": [hook.clone()],
            }]),
        ),
    ]);
    let hooks: serde_json::Map<String, serde_json::Value> = entries.into_iter().collect();
    let document = serde_json::json!({ "hooks": hooks });
    serde_json::to_string_pretty(&document).unwrap_or_else(|_| document.to_string())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::options::PermissionMode;
    use serde_json::{Value, json};

    /// A spawn's options and whether it was trusted.
    struct Spawn {
        opts: ClaudeOptions,
        trusted: bool,
    }

    /// A spawn that passed `mode` as `--permission-mode`, or none; `yolo`
    /// spawns it trusted.
    fn spawn_with(mode: Option<PermissionMode>, yolo: bool) -> Spawn {
        Spawn {
            opts: ClaudeOptions {
                permission_mode: mode,
                ..ClaudeOptions::default()
            },
            trusted: yolo,
        }
    }

    /// The mismatch `spawn` and a reported mode make.
    fn permission_mismatch(spawn: &Spawn, reported: Option<&str>) -> Option<(String, String)> {
        mode_mismatch(Some(&spawn.opts), spawn.trusted, reported)
    }

    /// The hook payload `body`, as the route hands it to the plugin.
    fn details(body: &Value) -> Value {
        body.clone()
    }

    /// The permission mismatch the report for `body` carries, for a session
    /// spawned as `spawn`.
    fn apply_details(spawn: &mut Spawn, body: &Value) -> Option<(String, String)> {
        map_hook_event(body, Some(&spawn.opts), spawn.trusted)
            .and_then(|report| report.permission_mismatch)
    }

    /// The prompt route answers the pair `apply_signal` warns about, from the
    /// payload and the spawn's flag.
    #[test]
    fn user_prompt_submit_answers_the_permission_mismatch() {
        let mut rec = spawn_with(Some(PermissionMode::Plan), false);
        let differing = json!({
            "hook_event_name": "UserPromptSubmit",
            "permission_mode": "default",
        });
        assert_eq!(
            apply_details(&mut rec, &details(&differing)),
            Some(("plan".to_owned(), "default".to_owned()))
        );
        let matching = json!({
            "hook_event_name": "UserPromptSubmit",
            "permission_mode": "plan",
        });
        assert_eq!(apply_details(&mut rec, &details(&matching)), None);
    }

    #[test]
    fn permission_mismatch_reports_a_different_mode() {
        let spawn = spawn_with(Some(PermissionMode::Plan), false);
        assert_eq!(
            permission_mismatch(&spawn, Some("default")),
            Some(("plan".to_owned(), "default".to_owned()))
        );
        assert_eq!(permission_mismatch(&spawn, Some("plan")), None);
    }

    #[test]
    fn permission_mismatch_ignores_auto_and_missing_reports() {
        let spawn = spawn_with(Some(PermissionMode::Plan), false);
        assert_eq!(permission_mismatch(&spawn, Some("auto")), None);
        assert_eq!(permission_mismatch(&spawn, None), None);
    }

    #[test]
    fn permission_mismatch_needs_a_flagged_spawn() {
        assert_eq!(
            permission_mismatch(&spawn_with(None, false), Some("plan")),
            None,
            "no --permission-mode means nothing to compare against"
        );
        assert_eq!(
            permission_mismatch(
                &spawn_with(Some(PermissionMode::Plan), true),
                Some("default")
            ),
            None,
            "yolo passes no flag either"
        );
    }

    const SESSION_ID: &str = "0b6f4c7e-1111-4222-8333-944455556666";
    const TRACER_EXE: &str = "C:/rt/binaries/rt-tracer-0123456789abcdef.exe";
    const CONFIG_DIR: &str = "C:/Users/lefto/AppData/Roaming/leftos/rustling-tulip/config";

    fn shim() -> HookShim<'static> {
        HookShim {
            tracer_exe: Path::new(TRACER_EXE),
            session_id: SESSION_ID,
            config_dir: Path::new(CONFIG_DIR),
        }
    }

    /// The parsed file the plan's one file holds for [`SESSION_ID`].
    fn hook_document() -> serde_json::Value {
        let json = hook_plan(&shim()).files[0].contents.clone();
        serde_json::from_str(&json).expect("the hook settings file is JSON")
    }

    /// One event's array of matcher groups in `document`.
    fn groups<'a>(document: &'a serde_json::Value, event: &str) -> &'a Vec<serde_json::Value> {
        document["hooks"][event]
            .as_array()
            .expect("an event's hooks are an array")
    }

    #[test]
    fn hook_plan_writes_one_settings_file_and_passes_it_by_name() {
        let plan = hook_plan(&shim());
        assert_eq!(plan.files.len(), 1, "one settings file");
        assert_eq!(plan.files[0].name, "hook-settings.json");
        assert_eq!(
            plan.args,
            vec![
                "--settings".to_string(),
                "{file:hook-settings.json}".to_string(),
            ],
            "the argv names the file by its placeholder"
        );
        assert_eq!(
            plan.files[0].contents,
            build_hook_settings(Path::new(TRACER_EXE), SESSION_ID, Path::new(CONFIG_DIR)),
            "the file holds the injected hooks document"
        );
    }

    #[test]
    fn hook_settings_list_every_event_and_matcher() {
        let document = hook_document();
        let hooks = document["hooks"].as_object().expect("a hooks object");
        let mut events: Vec<&str> = hooks.keys().map(String::as_str).collect();
        events.sort_unstable();
        assert_eq!(
            events,
            [
                "Elicitation",
                "MessageDisplay",
                "Notification",
                "PermissionRequest",
                "PostToolUse",
                "PostToolUseFailure",
                "PreToolUse",
                "SessionEnd",
                "SessionStart",
                "Stop",
                "StopFailure",
                "UserPromptSubmit",
            ]
        );
        assert_eq!(
            groups(&document, "PreToolUse")[0]["matcher"].as_str(),
            Some("AskUserQuestion|ExitPlanMode")
        );
        assert_eq!(
            groups(&document, "Notification")[0]["matcher"].as_str(),
            Some("permission_prompt|elicitation_dialog|elicitation_url_dialog|idle_prompt")
        );
        for event in [
            "SessionStart",
            "UserPromptSubmit",
            "PostToolUse",
            "PostToolUseFailure",
            "PermissionRequest",
            "Elicitation",
            "Stop",
            "StopFailure",
            "SessionEnd",
            "MessageDisplay",
        ] {
            assert!(
                groups(&document, event)[0].get("matcher").is_none(),
                "{event} runs for every input and names no matcher"
            );
        }
    }

    #[test]
    fn hook_settings_run_the_cached_tracer_in_exec_form() {
        let document = hook_document();
        let hooks = document["hooks"].as_object().expect("a hooks object");
        for event in hooks.keys() {
            let event_groups = groups(&document, event);
            assert_eq!(event_groups.len(), 1, "{event} has one matcher group");
            let entries = event_groups[0]["hooks"]
                .as_array()
                .expect("a group's hooks are an array");
            assert_eq!(entries.len(), 1, "{event} has one hook");
            let hook = &entries[0];
            assert_eq!(hook["type"], "command", "{event}");
            assert_eq!(hook["command"].as_str(), Some(TRACER_EXE), "{event}");
            assert_eq!(
                hook["args"],
                serde_json::json!(["hook", "--session", SESSION_ID, "--config-dir", CONFIG_DIR,]),
                "{event}"
            );
            // Exec form: the command is the exe alone, never a shell string
            // with the arguments folded into it.
            assert!(
                !hook["command"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("--session"),
                "{event} must not build a shell command string"
            );
            assert!(hook.get("shell").is_none(), "{event} must not name a shell");
        }
    }

    #[test]
    fn hook_settings_are_async() {
        let document = hook_document();
        let hooks = document["hooks"].as_object().expect("a hooks object");
        for event in hooks.keys() {
            assert_eq!(
                groups(&document, event)[0]["hooks"][0]["async"],
                serde_json::json!(true),
                "{event} must run in the background, not block the turn"
            );
        }
    }

    /// The `MessageDisplay` body Claude Code 2.1.314 sent for a three-line
    /// answer, its transcript path's user folder redacted.
    const MESSAGE_DISPLAY: &str = include_str!("../tests/fixtures/hooks/message_display.json");

    fn message_display_body() -> Value {
        serde_json::from_str(MESSAGE_DISPLAY).expect("the fixture is JSON")
    }

    #[test]
    fn captured_message_display_maps_to_a_live_line() {
        let line = agent_api::Agent::map_live_line(&crate::ClaudePlugin, &message_display_body())
            .expect("the captured body carries a live line");
        assert_eq!(
            line,
            agent_api::LiveLine {
                text: "Red — the colour of fire and ripe strawberries\n\
                       Yellow — bright like a summer sun\n\
                       Blue — calm as a clear sky"
                    .to_owned(),
                index: 0,
                message_key: Some("ea5975fb-ea6c-4b0c-a588-87e760f6e3cc".to_owned()),
                prompt_key: Some("7e18acba-1e68-409c-9232-4b0d7764938a".to_owned()),
            }
        );
    }

    #[test]
    fn captured_message_display_reports_no_status() {
        assert_eq!(map_hook_event(&message_display_body(), None, false), None);
    }

    #[test]
    fn hook_settings_name_only_a_hooks_object() {
        let document = hook_document();
        let object = document.as_object().expect("a JSON object");
        assert_eq!(
            object.keys().map(String::as_str).collect::<Vec<_>>(),
            ["hooks"],
            "every other settings key must keep the user's value"
        );
    }
}
