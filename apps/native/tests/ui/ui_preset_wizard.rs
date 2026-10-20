//! Preset wizard specs: a preset row of the container menu opens it on the
//! Source stage; the allowed sources switch, a folder starts at its default
//! and a pick replaces it, a file pick offers the prompts filter, an issue
//! spec is summed up; required variables hold Next with a `Fill in:`
//! tooltip; a file variable is picked and cleared; a preset without
//! prompted variables skips that stage; the preview lists the prompts
//! grouped by tab and the max-panes field regroups them or, invalid,
//! disables Launch; Launch runs the scripts first, shows their error inline
//! and retries, then sends the launch and closes; this window's job shows a
//! sticky progress toast that a completion replaces, a failure raises one
//! toast and another client's job none; Cancel, Esc (also from a text
//! field), a new `Welcome` and a lost connection close it.

use crate::support;

use std::path::PathBuf;

use gpui::TestAppContext;
use protocol::{
    ClientMessage, DaemonMessage, PresetEntry, PresetLaunchJobSnapshot, PresetLaunchJobStatus,
    PresetTarget,
};
use rustling_tulip_native::{FileFilter, LaunchButton, NextButton, Toast, ToastKind};
use serde_json::json;
use support::{Fixture, Harness, PROTOCOL, TestDir, repo, session};

/// Repos `r1` and `r2`, one session of `r1`.
fn two_repos() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
    fixture
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "each spec builds its JSON inline at the call"
)]
fn preset(sources: serde_json::Value, variables: serde_json::Value) -> PresetEntry {
    serde_json::from_value(json!({
        "id": "triage", "name": "Triage", "source_repo_id": "r1",
        "description": "Free-form bug prompts",
        "prompt_sources": sources, "variables": variables,
        "prompt_template": "{prompt}", "branch_template": "tmp/{index}",
        "tab_grouping": {"kind": "none"},
        "injector": {"startup_delay_ms": 0, "pre_input": [], "post_input": []}
    }))
    .expect("preset fixture")
}

fn all_sources() -> serde_json::Value {
    json!([
        {"kind": "file"},
        {"kind": "folder", "relative_path": "docs/prompts"},
        {"kind": "inline"},
        {"kind": "github_issues"}
    ])
}

/// Opens r1's menu, answers it with `entry` and presses its row.
fn open_wizard(h: &mut Harness<'_>, entry: PresetEntry) {
    h.right_click_on("container-repo:r1");
    h.send(DaemonMessage::Presets {
        target: PresetTarget::Repo {
            repo_id: "r1".to_owned(),
        },
        entries: vec![entry],
    });
    h.click_on("container-menu-launch-preset-0");
    assert!(
        h.root(|root, _| root.preset_wizard_open()),
        "the wizard opened"
    );
}

fn next(h: &mut Harness<'_>) -> Option<NextButton> {
    h.root(|root, _| root.preset_wizard_next())
}

fn picked_source(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.preset_wizard_sources())
        .into_iter()
        .filter(|(_, _, picked)| *picked)
        .map(|(selector, _, _)| selector)
        .collect()
}

/// Types `text` into the field that has the keyboard, one key per char.
fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
}

#[gpui::test]
fn a_preset_row_opens_the_wizard_on_the_source_stage(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open_wizard(&mut h, preset(all_sources(), json!([])));

    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "the menu closed"
    );
    assert_eq!(
        h.root(|root, _| root.preset_wizard_title()).as_deref(),
        Some("Launch preset · Triage")
    );
    assert_eq!(h.root(|root, _| root.preset_wizard_stage()), Some("source"));
    assert_eq!(
        h.root(|root, _| root.preset_wizard_sources()),
        [
            (
                "preset-source-kind-file".to_owned(),
                "From file".to_owned(),
                true
            ),
            (
                "preset-source-kind-folder".to_owned(),
                "From folder (docs/prompts)".to_owned(),
                false
            ),
            (
                "preset-source-kind-inline".to_owned(),
                "Inline".to_owned(),
                false
            ),
            (
                "preset-source-kind-github_issues".to_owned(),
                "From GitHub issues".to_owned(),
                false
            ),
        ],
        "every allowed source, the first picked"
    );
    assert_eq!(
        next(&mut h),
        Some(NextButton {
            enabled: false,
            tip: None
        }),
        "Next waits for a file"
    );

    h.click_on("preset-cancel");
    assert!(
        !h.root(|root, _| root.preset_wizard_open()),
        "Cancel closes it"
    );
}

#[gpui::test]
fn sources_switch_and_an_issue_spec_is_summed_up(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open_wizard(&mut h, preset(all_sources(), json!([])));

    h.click_on("preset-source-kind-folder");
    assert_eq!(picked_source(&mut h), ["preset-source-kind-folder"]);
    assert_eq!(
        h.root(|root, _| root.preset_wizard_folder_path())
            .as_deref(),
        Some("D:/src/r1\\docs\\prompts"),
        "the folder starts at its default under the source repo"
    );

    h.click_on("preset-source-kind-github_issues");
    assert_eq!(picked_source(&mut h), ["preset-source-kind-github_issues"]);
    assert_eq!(
        h.root(|root, _| root.preset_wizard_issues_line())
            .as_deref(),
        Some("Comma-separated issue numbers; ranges like 123-127 are inclusive.")
    );
    h.click_on("preset-source-github-issues-spec");
    type_text(&mut h, "42");
    assert_eq!(
        h.root(|root, _| root.preset_wizard_issues_line())
            .as_deref(),
        Some("1 issue → session: #42")
    );
    h.click_on("preset-next");
    assert_eq!(
        h.root(|root, _| root.preset_wizard_stage()),
        Some("preview"),
        "no prompted variable skips their stage"
    );
    assert_eq!(
        preview_groups(&mut h),
        [vec!["[1] GitHub issue #42".to_owned()]],
        "issues preview without asking the daemon"
    );
    assert!(
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::PreviewPreset { .. })),
        "no preview request went out"
    );
}

#[gpui::test]
fn picks_fill_the_folder_and_the_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open_wizard(&mut h, preset(all_sources(), json!([])));

    h.set_picked_file(Some("E:/prompts/bugs.md"));
    h.click_on("preset-source-file-pick");
    assert_eq!(
        h.root(|root, _| root.preset_wizard_file_path()).as_deref(),
        Some(
            PathBuf::from("E:/prompts/bugs.md")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert_eq!(
        h.file_filters(),
        [FileFilter::new("Prompts", &["txt", "md"])],
        "the prompts file picker offers .txt and .md"
    );

    h.click_on("preset-source-kind-folder");
    h.set_picked_folder(Some("E:/prompts"));
    h.click_on("preset-source-folder-pick");
    assert_eq!(
        h.folder_starts(),
        [Some(PathBuf::from("D:/src/r1\\docs\\prompts"))],
        "the folder picker opens at the folder shown"
    );
    assert_eq!(
        h.root(|root, _| root.preset_wizard_folder_path())
            .as_deref(),
        Some(PathBuf::from("E:/prompts").to_string_lossy().as_ref())
    );
}

#[gpui::test]
fn required_variables_hold_next_with_their_labels(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let variables = json!([
        {"name": "ticket", "label": "Ticket", "kind": {"kind": "text"}, "prompt_at_launch": true},
        {"name": "logs", "label": "Attach logs", "kind": {"kind": "toggle", "value": "x"},
         "prompt_at_launch": true, "default": "false"}
    ]);
    open_wizard(&mut h, preset(json!([{"kind": "inline"}]), variables));

    h.click_on("preset-source-inline-text");
    type_text(&mut h, "fix");
    assert_eq!(
        next(&mut h),
        Some(NextButton {
            enabled: true,
            tip: None
        })
    );
    h.click_on("preset-next");
    assert_eq!(
        h.root(|root, _| root.preset_wizard_stage()),
        Some("variables")
    );
    assert_eq!(
        next(&mut h),
        Some(NextButton {
            enabled: false,
            tip: Some("Fill in: Ticket".to_owned())
        }),
        "the toggle is never required"
    );

    h.click_on("preset-variable-logs");
    assert_eq!(
        h.root(|root, _| root.preset_wizard_value("logs"))
            .as_deref(),
        Some("true")
    );
    h.click_on("preset-variable-ticket");
    type_text(&mut h, "RT21");
    assert_eq!(
        next(&mut h),
        Some(NextButton {
            enabled: true,
            tip: None
        }),
        "filled in, Next moves on"
    );

    h.click_on("preset-back");
    assert_eq!(h.root(|root, _| root.preset_wizard_stage()), Some("source"));
}

#[gpui::test]
fn escape_and_a_lost_connection_close_the_wizard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open_wizard(&mut h, preset(all_sources(), json!([])));
    h.keys("escape");
    assert!(
        !h.root(|root, _| root.preset_wizard_open()),
        "Esc closes it"
    );

    open_wizard(&mut h, preset(all_sources(), json!([])));
    h.lose_connection();
    assert!(
        !h.root(|root, _| root.preset_wizard_open()),
        "the connection overlay closes it"
    );
}

#[gpui::test]
fn escape_in_a_text_field_closes_the_wizard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open_wizard(&mut h, preset(json!([{"kind": "inline"}]), json!([])));
    h.click_on("preset-source-inline-text");
    type_text(&mut h, "fix");
    h.keys("escape");
    assert!(
        !h.root(|root, _| root.preset_wizard_open()),
        "Esc from the field closes the wizard"
    );
}

#[gpui::test]
fn a_new_welcome_closes_the_wizard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open_wizard(&mut h, preset(all_sources(), json!([])));
    h.send(support::welcome(PROTOCOL));
    assert!(
        !h.root(|root, _| root.preset_wizard_open()),
        "a new connection closes the wizard"
    );
}

#[gpui::test]
fn a_file_variable_is_picked_and_cleared(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let variables = json!([
        {"name": "log", "label": "Log file", "kind": {"kind": "file_path", "extensions": ["log"]},
         "prompt_at_launch": true}
    ]);
    open_wizard(&mut h, preset(json!([{"kind": "inline"}]), variables));
    h.click_on("preset-source-inline-text");
    type_text(&mut h, "fix");
    h.click_on("preset-next");
    assert_eq!(
        next(&mut h),
        Some(NextButton {
            enabled: false,
            tip: Some("Fill in: Log file".to_owned())
        })
    );

    h.set_picked_file(Some("E:/logs/run.log"));
    h.click_on("preset-variable-log-pick");
    let picked = PathBuf::from("E:/logs/run.log")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        h.root(|root, _| root.preset_wizard_value("log")),
        Some(picked)
    );
    assert_eq!(
        h.file_filters(),
        [FileFilter::new("Log file", &["log"])],
        "the picker offers the variable's extensions"
    );
    assert_eq!(
        next(&mut h),
        Some(NextButton {
            enabled: true,
            tip: None
        }),
        "the pick fills the required variable"
    );

    h.click_on("preset-variable-log-clear");
    assert_eq!(
        h.root(|root, _| root.preset_wizard_value("log")).as_deref(),
        Some("")
    );
    assert_eq!(
        next(&mut h),
        Some(NextButton {
            enabled: false,
            tip: Some("Fill in: Log file".to_owned())
        }),
        "Clear empties it again"
    );
}

/// `entry` packing its sessions `cap` to a tab.
fn grouped(mut entry: PresetEntry, cap: Option<u32>) -> PresetEntry {
    entry.tab_grouping = serde_json::from_value(json!({
        "kind": "new_tab", "layout": "auto_grid", "max_panes_per_tab": cap
    }))
    .expect("tab grouping fixture");
    entry
}

/// A preset whose one script variable `ticket` runs `fetch`.
fn scripted() -> PresetEntry {
    preset(
        json!([{"kind": "github_issues"}]),
        json!([{"name": "ticket", "label": "Fetch ticket",
                "kind": {"kind": "script", "cmd": "fetch", "args": ["--id", "7"]}}]),
    )
}

/// The preview rows as drawn, by tab.
fn preview_groups(h: &mut Harness<'_>) -> Vec<Vec<String>> {
    h.root(|root, _| root.preset_wizard_preview_groups())
}

fn preview_line(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.preset_wizard_preview_line())
}

fn launch(h: &mut Harness<'_>) -> Option<LaunchButton> {
    h.root(|root, _| root.preset_wizard_launch())
}

fn toasts(h: &mut Harness<'_>) -> Vec<Toast> {
    h.root(|root, _| root.toasts().to_vec())
}

/// Opens `entry` on its issue source, types `spec` and moves to the
/// preview.
fn preview_issues(h: &mut Harness<'_>, entry: PresetEntry, spec: &str) {
    open_wizard(h, entry);
    h.click_on("preset-source-kind-github_issues");
    h.click_on("preset-source-github-issues-spec");
    type_text(h, spec);
    h.click_on("preset-next");
    assert_eq!(
        h.root(|root, _| root.preset_wizard_stage()),
        Some("preview")
    );
}

/// The job id of the launch the client sent.
fn launched_job(h: &mut Harness<'_>) -> String {
    h.sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::LaunchPreset { job_id, .. } => Some(job_id),
            _ => None,
        })
        .expect("a launch went out")
}

/// The id of the script run the client sent.
fn scripts_request(h: &mut Harness<'_>) -> String {
    h.sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::ResolvePresetScripts { id, .. } => Some(id),
            _ => None,
        })
        .expect("a script run went out")
}

fn job(job_id: &str, status: PresetLaunchJobStatus, launched: u32, tabs: &[&str]) -> DaemonMessage {
    let error = (status == PresetLaunchJobStatus::Failed).then(|| "boom".to_owned());
    DaemonMessage::PresetLaunchJobUpdated {
        job: PresetLaunchJobSnapshot {
            job_id: job_id.to_owned(),
            preset_id: "triage".to_owned(),
            target: PresetTarget::Repo {
                repo_id: "r1".to_owned(),
            },
            total: 3,
            launched,
            created_session_ids: Vec::new(),
            created_tab_ids: tabs.iter().map(|tab| (*tab).to_owned()).collect(),
            status,
            error,
            current_tab_id: None,
        },
    }
}

/// Launches three issue prompts packed two to a tab; returns the job id.
fn launch_three(h: &mut Harness<'_>) -> String {
    preview_issues(
        h,
        grouped(preset(all_sources(), json!([])), Some(2)),
        "1,2,3",
    );
    h.click_on("preset-launch");
    launched_job(h)
}

#[gpui::test]
fn preview_lists_prompts_grouped_by_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open_wizard(&mut h, grouped(preset(all_sources(), json!([])), Some(2)));
    h.set_picked_file(Some("E:/prompts/bugs.md"));
    h.click_on("preset-source-file-pick");
    h.sent();
    h.click_on("preset-next");

    let id = h
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::PreviewPreset { id, .. } => Some(id),
            _ => None,
        })
        .expect("the file preview is asked of the daemon");
    assert_eq!(preview_line(&mut h).as_deref(), Some("Resolving prompts…"));
    h.send(DaemonMessage::PresetPreview {
        id: "stale".to_owned(),
        prompts: vec!["old".to_owned()],
    });
    assert_eq!(
        preview_line(&mut h).as_deref(),
        Some("Resolving prompts…"),
        "an answer to an older request is dropped"
    );
    h.send(DaemonMessage::PresetPreview {
        id: id.clone(),
        prompts: vec![
            "fix a\nmore".to_owned(),
            "fix b".to_owned(),
            "fix c".to_owned(),
        ],
    });
    assert_eq!(
        preview_line(&mut h).as_deref(),
        Some("3 prompts ready to spawn. → 2 tabs.")
    );
    assert_eq!(
        preview_groups(&mut h),
        [
            vec!["[1] fix a".to_owned(), "[2] fix b".to_owned()],
            vec!["[3] fix c".to_owned()]
        ]
    );

    h.click_on("preset-back");
    h.click_on("preset-next");
    let again = h.sent().into_iter().find_map(|msg| match msg {
        ClientMessage::PreviewPreset { id, .. } => Some(id),
        _ => None,
    });
    assert!(
        again.is_some_and(|again| again != id),
        "entering the preview again asks again"
    );
    h.send(DaemonMessage::PresetPreviewError {
        id,
        error: "late".to_owned(),
    });
    assert_eq!(preview_line(&mut h).as_deref(), Some("Resolving prompts…"));
}

#[gpui::test]
fn max_panes_regroups_the_preview(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    preview_issues(
        &mut h,
        grouped(preset(all_sources(), json!([])), Some(3)),
        "1,2,3,4",
    );
    let sizes =
        |h: &mut Harness<'_>| -> Vec<usize> { preview_groups(h).iter().map(Vec::len).collect() };
    assert_eq!(sizes(&mut h), [3, 1], "the preset's cap to start");

    h.click_on("preset-max-per-tab");
    h.keys("ctrl-a backspace 2");
    assert_eq!(sizes(&mut h), [2, 2]);
    assert_eq!(
        preview_line(&mut h).as_deref(),
        Some("4 prompts ready to spawn. → 2 tabs.")
    );
    h.keys("ctrl-a backspace");
    assert_eq!(sizes(&mut h), [3, 1], "blank is the preset default");

    h.keys("9");
    h.click_on("preset-launch");
    let overrides = h.sent().into_iter().find_map(|msg| match msg {
        ClientMessage::LaunchPreset {
            max_panes_per_tab_override,
            use_worktree_override,
            ..
        } => Some((max_panes_per_tab_override, use_worktree_override)),
        _ => None,
    });
    assert_eq!(
        overrides,
        Some((Some(9), None)),
        "the typed cap goes; the worktree choice stays the preset's"
    );
}

#[gpui::test]
fn invalid_max_panes_disables_launch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    preview_issues(&mut h, grouped(preset(all_sources(), json!([])), None), "5");
    assert_eq!(
        launch(&mut h),
        Some(LaunchButton {
            label: "Launch 1 session".to_owned(),
            enabled: true,
            tip: None
        })
    );
    h.click_on("preset-max-per-tab");
    h.keys("0");
    assert_eq!(
        launch(&mut h),
        Some(LaunchButton {
            label: "Launch 1 session".to_owned(),
            enabled: false,
            tip: Some("Max panes per tab must be a whole number of 1 or more".to_owned())
        })
    );
    h.click_on("preset-launch");
    assert!(
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::LaunchPreset { .. })),
        "a disabled Launch sends nothing"
    );
    assert!(h.root(|root, _| root.preset_wizard_open()));
}

#[gpui::test]
fn launch_runs_scripts_then_launches_and_closes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    preview_issues(&mut h, grouped(scripted(), Some(2)), "7");
    assert_eq!(
        h.root(|root, _| root.preset_wizard_scripts()),
        [("Fetch ticket".to_owned(), "fetch --id 7".to_owned())],
        "the command shows before it runs"
    );
    assert_eq!(
        launch(&mut h).map(|button| button.label),
        Some("Run script & launch 1 session".to_owned())
    );

    h.click_on("preset-launch");
    let id = scripts_request(&mut h);
    assert_eq!(
        launch(&mut h),
        Some(LaunchButton {
            label: "Running scripts…".to_owned(),
            enabled: false,
            tip: None
        })
    );
    h.click_on("preset-max-per-tab");
    h.keys("ctrl-a backspace 0");
    h.send(DaemonMessage::PresetScriptsResolved {
        id,
        values: vec![("ticket".to_owned(), "T-7".to_owned())],
        executed_commands: Vec::new(),
    });
    let launched = h.sent().into_iter().find_map(|msg| match msg {
        ClientMessage::LaunchPreset {
            job_id,
            variable_values,
            max_panes_per_tab_override,
            ..
        } => Some((
            job_id.is_empty(),
            variable_values,
            max_panes_per_tab_override,
        )),
        _ => None,
    });
    assert_eq!(
        launched,
        Some((
            false,
            vec![("ticket".to_owned(), "T-7".to_owned())],
            Some(2)
        )),
        "the launch follows the scripts under the client's job id, with the resolved values as they came \
         and the cap the field held, which takes no edit while the scripts run"
    );
    assert!(
        !h.root(|root, _| root.preset_wizard_open()),
        "Launch closes the wizard"
    );
}

#[gpui::test]
fn script_error_shows_inline_and_retry_relaunches(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    preview_issues(&mut h, scripted(), "7");
    h.click_on("preset-launch");
    let first = scripts_request(&mut h);
    h.send(DaemonMessage::PresetScriptsError {
        id: first.clone(),
        error: "exit 1".to_owned(),
        variable_name: None,
    });
    assert_eq!(
        h.root(|root, _| root.preset_wizard_script_error())
            .as_deref(),
        Some("Script resolution failed: exit 1")
    );
    assert_eq!(
        launch(&mut h),
        Some(LaunchButton {
            label: "Retry script & launch 1 session".to_owned(),
            enabled: true,
            tip: None
        })
    );
    assert!(h.root(|root, _| root.preset_wizard_open()), "still open");

    h.click_on("preset-launch");
    let retry = scripts_request(&mut h);
    assert_ne!(retry, first, "the retry is a new request");
    h.send(DaemonMessage::PresetScriptsResolved {
        id: retry,
        values: Vec::new(),
        executed_commands: Vec::new(),
    });
    launched_job(&mut h);
    assert!(!h.root(|root, _| root.preset_wizard_open()));
}

#[gpui::test]
fn progress_toast_tracks_the_job(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    assert!(!h.root(|root, _| root.preset_wizard_open()));
    let shown = toasts(&mut h);
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].title, "Launching Triage");
    assert_eq!(shown[0].detail, None);
    assert!(shown[0].sticky);

    h.send(job(&job_id, PresetLaunchJobStatus::Spawning, 0, &[]));
    h.send(DaemonMessage::PresetLaunchProgress {
        job_id: job_id.clone(),
        preset_id: "triage".to_owned(),
        total: 3,
        launched: 1,
        current_tab_id: Some("t1".to_owned()),
        tab_ids: vec!["t1".to_owned()],
    });
    let shown = toasts(&mut h);
    assert_eq!(shown.len(), 1, "updated in place");
    assert_eq!(
        shown[0].detail.as_deref(),
        Some("1 of 3 sessions · 1 of 2 tabs")
    );
    assert!(shown[0].sticky);
    let key = shown[0].key.clone().unwrap_or_default();
    let progress = h.root(|root, _| root.preset_launch_progress(&key));
    assert!(
        progress.is_some_and(|fraction| (fraction - 1.0 / 3.0).abs() < 1e-6),
        "the bar is a third full: {progress:?}"
    );
    let bar = h.bounds(&format!("toast-progress-{}", shown[0].id));
    assert!(
        bar.size.width > gpui::px(0.0),
        "the bar is drawn under the detail"
    );
}

#[gpui::test]
fn dismissed_progress_toast_stays_dismissed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    let id = toasts(&mut h)[0].id;
    h.click_on(&format!("toast-close-{id}"));
    assert!(toasts(&mut h).is_empty(), "the × closes the progress toast");

    h.send(job(&job_id, PresetLaunchJobStatus::Spawning, 1, &["t1"]));
    h.send(DaemonMessage::PresetLaunchProgress {
        job_id: job_id.clone(),
        preset_id: "triage".to_owned(),
        total: 3,
        launched: 2,
        current_tab_id: Some("t1".to_owned()),
        tab_ids: vec!["t1".to_owned()],
    });
    assert!(
        toasts(&mut h).is_empty(),
        "progress does not bring the dismissed toast back"
    );

    h.send(job(
        &job_id,
        PresetLaunchJobStatus::Completed,
        3,
        &["t1", "t2"],
    ));
    let shown = toasts(&mut h);
    assert_eq!(shown.len(), 1, "the end still shows");
    assert_eq!(shown[0].title, "Launched Triage");
}

#[gpui::test]
fn foreign_job_raises_no_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let own = launch_three(&mut h);
    h.send(job("theirs", PresetLaunchJobStatus::Spawning, 1, &[]));
    h.send(job("theirs", PresetLaunchJobStatus::Failed, 1, &[]));
    h.send(DaemonMessage::PresetLaunchFailed {
        job_id: "theirs".to_owned(),
        preset_id: "triage".to_owned(),
        error: "boom".to_owned(),
        partial_session_ids: Vec::new(),
        partial_tab_ids: Vec::new(),
    });
    let keys: Vec<Option<String>> = toasts(&mut h).into_iter().map(|toast| toast.key).collect();
    assert_eq!(
        keys,
        [Some(format!("preset:{own}"))],
        "only this window's own job shows; another client's is not ours"
    );
}

#[gpui::test]
fn failed_job_raises_one_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    h.send(job(&job_id, PresetLaunchJobStatus::Failed, 1, &["t1"]));
    h.send(DaemonMessage::PresetLaunchFailed {
        job_id,
        preset_id: "triage".to_owned(),
        error: "boom".to_owned(),
        partial_session_ids: Vec::new(),
        partial_tab_ids: Vec::new(),
    });
    let shown = toasts(&mut h);
    assert_eq!(shown.len(), 1, "one toast for the two failure messages");
    assert_eq!(shown[0].kind, ToastKind::Warning);
    assert_eq!(shown[0].title, "Preset launch failed: Triage");
    assert_eq!(shown[0].detail.as_deref(), Some("boom"));
    assert!(!shown[0].sticky);

    let second = launch_three(&mut h);
    h.send(DaemonMessage::PresetLaunchFailed {
        job_id: second.clone(),
        preset_id: "triage".to_owned(),
        error: "refused".to_owned(),
        partial_session_ids: Vec::new(),
        partial_tab_ids: Vec::new(),
    });
    h.send(job(&second, PresetLaunchJobStatus::Failed, 0, &[]));
    let key = Some(format!("preset:{second}"));
    let of_second: Vec<Toast> = toasts(&mut h)
        .into_iter()
        .filter(|toast| toast.key == key)
        .collect();
    assert_eq!(
        of_second.len(),
        1,
        "one toast when the failure message comes before the snapshot"
    );
    assert_eq!(of_second[0].detail.as_deref(), Some("refused"));
}

#[gpui::test]
fn completed_job_replaces_the_sticky_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    let before = toasts(&mut h);
    assert_eq!(before[0].title, "Launching Triage");
    let sticky = before[0].id;
    h.send(job(
        &job_id,
        PresetLaunchJobStatus::Completed,
        3,
        &["t1", "t2"],
    ));
    let shown = toasts(&mut h);
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].id, sticky, "the same toast, replaced in place");
    assert_eq!(shown[0].title, "Launched Triage");
    assert_eq!(shown[0].detail.as_deref(), Some("3 sessions in 2 tabs"));
    assert!(!shown[0].sticky, "it goes by itself now");
    let key = shown[0].key.clone().unwrap_or_default();
    assert_eq!(h.root(|root, _| root.preset_launch_progress(&key)), None);

    h.send(support::welcome(PROTOCOL));
    assert!(
        toasts(&mut h).is_empty(),
        "a new connection drops the job's toast"
    );
    h.send(job(&job_id, PresetLaunchJobStatus::Spawning, 1, &[]));
    assert!(
        toasts(&mut h).is_empty(),
        "a new connection forgets the job"
    );
}
