//! Preset wizard specs: a preset row of the container menu opens it on the
//! Source stage; the allowed sources switch, a folder starts at its default
//! and a pick replaces it, a file pick offers the prompts filter, an issue
//! spec is summed up; required variables hold Next with a `Fill in:`
//! tooltip; a file variable is picked and cleared; a preset without
//! prompted variables skips that stage; Next on the last stage is disabled;
//! Cancel, Esc (also from a text field), a new `Welcome` and a lost
//! connection close it.

use crate::support;

use std::path::PathBuf;

use gpui::TestAppContext;
use protocol::{DaemonMessage, PresetEntry, PresetTarget};
use rustling_tulip_native::{FileFilter, NextButton, PREVIEW_LATER_TIP};
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
    assert_eq!(
        next(&mut h),
        Some(NextButton {
            enabled: false,
            tip: Some(PREVIEW_LATER_TIP.to_owned())
        }),
        "no prompted variable skips their stage, so this Next is the last"
    );
    h.click_on("preset-next");
    assert_eq!(
        h.root(|root, _| root.preset_wizard_stage()),
        Some("source"),
        "the disabled last Next goes nowhere"
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
            enabled: false,
            tip: Some(PREVIEW_LATER_TIP.to_owned())
        }),
        "filled in, Next is the last one"
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
            enabled: false,
            tip: Some(PREVIEW_LATER_TIP.to_owned())
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
