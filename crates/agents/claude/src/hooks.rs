//! Claude's hook plan: the `--settings` document an interactive spawn carries
//! and the argv that names it.

use agent_api::{HookFile, HookPlan, HookShim};
use std::path::Path;

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
