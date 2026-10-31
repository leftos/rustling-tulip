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
//! sticky toast and another client's job none; the toast's Cancel, Select
//! and Stop all buttons follow the job (Select tints its sessions' leaves
//! until a session click or a new connection, Stop all stops only live
//! ones), a closed or timed-out ended toast drops its job, a new `Welcome`
//! keeps ended jobs' toasts, and a tab-grouping job's first tab is shown
//! once; Cancel, Esc (also from a text field), a new `Welcome` and a lost
//! connection close the wizard. On a remote host the path rows take typed
//! host paths where this PC's pickers never open, the folder row starts at
//! the source repo's path there, and a launch's Cancel goes to that host.

#![expect(
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::path::PathBuf;

use gpui::{Modifiers, TestAppContext, px};
use protocol::{
    AgentId, ClientMessage, DaemonMessage, LaunchPresetSource, PresetEntry,
    PresetLaunchJobSnapshot, PresetLaunchJobStatus, PresetTarget,
};
use rustling_tulip_native::palette::{LAUNCHED_TINT, SELECTION};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{
    Activity, FileFilter, LaunchButton, NextButton, Toast, ToastKind, toast_action_selector,
};
use serde_json::json;
use std::time::Duration;
use support::{Fixture, Harness, PROTOCOL, TestDir, pane, repo, session, tab};

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
    let request_id = Some(h.preset_request());
    h.send(DaemonMessage::Presets {
        target: PresetTarget::Repo {
            repo_id: "r1".to_owned(),
        },
        entries: vec![entry],
        request_id,
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
fn preset_wizard_path_rows_stay_read_only_on_a_local_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open_wizard(&mut h, preset(all_sources(), json!([])));

    h.click_on("preset-source-file");
    type_text(&mut h, "bugs");
    assert_eq!(
        h.root(|root, _| root.preset_wizard_file_path()),
        None,
        "typing fills nothing"
    );
    assert_eq!(
        h.root(|root, cx| root.preset_wizard_path_text("preset-source-file", cx)),
        None,
        "the row has no text field"
    );
    h.set_picked_file(Some("E:/prompts/bugs.md"));
    h.click_on("preset-source-file-pick");
    assert_eq!(h.file_asks(), 1, "Pick… opens this PC's picker");
}

/// The tailnet host the remote specs connect beside This PC.
const REMOTE: &str = "box.tail1234.ts.net";

/// The one agent the remote host lists, which This PC does not.
const BOX_AGENT: &str = "box-agent";

/// What a pick row's Pick… says on a host that opens no picker on this PC.
const LOCAL_FILES: &str = "Not available on a remote host: this opens files on this PC";

/// [`REMOTE`] as the specs connect it.
fn remote() -> Target {
    Target::tailnet(REMOTE, None)
}

/// A window on This PC holding `local_repos`, with the tailnet host
/// connected beside it, listing only [`BOX_AGENT`] and holding
/// `remote_repos`; nothing sent is left unread on either host.
fn two_hosts<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    local_repos: &[&str],
    remote_repos: &[&str],
) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s1").build());
    fixture.repos = local_repos
        .iter()
        .map(|id| repo(id, &format!("C:/{id}")))
        .collect();
    let mut h = Harness::with(cx, dir, &fixture);
    let mut agent = agent_registry::descriptors()
        .into_iter()
        .next()
        .expect("the registry lists an agent");
    agent.id = AgentId(BOX_AGENT.to_owned());
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Agents {
        agents: vec![agent],
    });
    host.send(DaemonMessage::Sessions {
        sessions: Vec::new(),
    });
    host.send(DaemonMessage::Repos {
        repos: remote_repos
            .iter()
            .map(|id| repo(id, &format!("D:/{id}")))
            .collect(),
    });
    host.sent();
    h.sent();
    h
}

/// Whether the model shows what `selector` tags and it was painted; gpui
/// keeps an element's bounds after it leaves, so a spec asks before the
/// element was ever drawn.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.in_model(selector) && h.bounds(selector).origin.x >= px(0.0)
}

/// Whether resting the mouse on `selector` draws a tooltip reading `text`
/// against the mouse there. gpui puts a tooltip's corner a pixel from the
/// mouse, so one drawn for an earlier control elsewhere does not count.
fn tooltip_on(h: &mut Harness<'_>, selector: &str, text: &str) -> bool {
    let at = h.center(selector);
    h.hover(at, Modifiers::none());
    h.advance(Duration::from_millis(600));
    let tip = h.bounds(&format!("tooltip={text}"));
    let slack = px(2.0);
    tip.origin.x >= px(0.0)
        && at.x >= tip.left() - slack
        && at.x <= tip.right() + slack
        && at.y >= tip.top() - slack
        && at.y <= tip.bottom() + slack
}

/// A preset of the remote host's `r2` with `sources` and `variables`, as
/// the wizard's specs build one.
#[expect(
    clippy::needless_pass_by_value,
    reason = "each spec builds its JSON inline at the call"
)]
fn remote_preset(sources: serde_json::Value, variables: serde_json::Value) -> PresetEntry {
    serde_json::from_value(json!({
        "id": "triage", "name": "Triage", "source_repo_id": "r2",
        "description": "Free-form bug prompts",
        "prompt_sources": sources, "variables": variables,
        "prompt_template": "{prompt}", "branch_template": "tmp/{index}",
        "tab_grouping": {"kind": "none"},
        "injector": {"startup_delay_ms": 0, "pre_input": [], "post_input": []}
    }))
    .expect("preset fixture")
}

/// The target of the remote host's `r2`.
fn remote_repo() -> PresetTarget {
    PresetTarget::Repo {
        repo_id: "r2".to_owned(),
    }
}

/// Opens [`REMOTE`]'s `r2` menu, answers it with `entry` as that host does
/// and presses its row.
fn open_remote_wizard(h: &mut Harness<'_>, entry: PresetEntry) {
    h.right_click_on(&format!("container-{REMOTE}::repo:r2"));
    let target = remote_repo();
    let request_id = h
        .host(&remote())
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::ListPresets {
                target: asked,
                request_id,
            } if asked == target => request_id,
            _ => None,
        });
    h.host(&remote()).send(DaemonMessage::Presets {
        target,
        entries: vec![entry],
        request_id,
    });
    h.click_on("container-menu-launch-preset-0");
    assert!(
        h.root(|root, _| root.preset_wizard_open()),
        "the wizard opened"
    );
}

/// The text of the wizard's typed host path field tagged `selector`.
fn path_text(h: &mut Harness<'_>, selector: &str) -> Option<String> {
    h.root(|root, cx| root.preset_wizard_path_text(selector, cx))
}

/// Answers the preview the remote host was asked for, if it was asked, with
/// one prompt, presses Launch and returns the launch the client sent.
fn launch_remote_from_preview(h: &mut Harness<'_>) -> ClientMessage {
    let asked = h
        .host(&remote())
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::PreviewPreset { id, .. } => Some(id),
            _ => None,
        });
    if let Some(id) = asked {
        h.host(&remote()).send(DaemonMessage::PresetPreview {
            id,
            prompts: vec!["fix".to_owned()],
        });
    }
    h.click_on("preset-launch");
    h.host(&remote())
        .sent()
        .into_iter()
        .find(|msg| matches!(msg, ClientMessage::LaunchPreset { .. }))
        .expect("a launch went out")
}

/// The preset messages This PC received: none, from a remote wizard.
fn local_preset_messages(h: &mut Harness<'_>) -> Vec<&'static str> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::PreviewPreset { .. } => Some("preview"),
            ClientMessage::ResolvePresetScripts { .. } => Some("scripts"),
            ClientMessage::LaunchPreset { .. } => Some("launch"),
            ClientMessage::CancelPresetLaunch { .. } => Some("cancel"),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn preset_wizard_browse_opens_no_picker_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, &["r1"], &["r2"]);
    h.set_picked_file(Some("E:/prompts/bugs.md"));
    h.set_picked_folder(Some("E:/prompts"));

    let sources = json!([{"kind": "file"}, {"kind": "folder", "relative_path": "docs/prompts"}]);
    open_remote_wizard(&mut h, remote_preset(sources, json!([])));
    assert!(tooltip_on(&mut h, "preset-source-file-pick", LOCAL_FILES));
    h.click_on("preset-source-file-pick");
    h.click_on("preset-source-kind-folder");
    assert!(tooltip_on(&mut h, "preset-source-folder-pick", LOCAL_FILES));
    h.click_on("preset-source-folder-pick");
    h.click_on("preset-cancel");

    let variables = json!([
        {"name": "log", "label": "Log file", "kind": {"kind": "file_path", "extensions": ["log"]},
         "prompt_at_launch": true}
    ]);
    open_remote_wizard(
        &mut h,
        remote_preset(json!([{"kind": "inline"}]), variables),
    );
    h.click_on("preset-source-inline-text");
    type_text(&mut h, "fix");
    h.click_on("preset-next");
    assert!(tooltip_on(&mut h, "preset-variable-log-pick", LOCAL_FILES));
    h.click_on("preset-variable-log-pick");

    assert_eq!(h.file_asks(), 0, "no file picker opens on this PC");
    assert_eq!(h.folder_asks(), 0, "no folder picker opens on this PC");
    assert!(
        local_preset_messages(&mut h).is_empty(),
        "This PC is asked for, and sent, no preset of the remote repo"
    );
}

#[gpui::test]
fn preset_wizard_path_rows_take_a_typed_host_path_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, &["r1"], &["r2"]);
    open_remote_wizard(&mut h, remote_preset(json!([{"kind": "file"}]), json!([])));

    h.click_on("preset-source-file");
    type_text(&mut h, "E:/prompts/bugs.md");
    assert_eq!(
        path_text(&mut h, "preset-source-file").as_deref(),
        Some("E:/prompts/bugs.md")
    );
    assert!(
        tooltip_on(&mut h, "preset-source-file-pick", LOCAL_FILES),
        "Pick… stays disabled beside the field"
    );
    h.click_on("preset-next");
    let ClientMessage::LaunchPreset { source, .. } = launch_remote_from_preview(&mut h) else {
        panic!("a launch went out");
    };
    assert_eq!(
        source,
        LaunchPresetSource::File {
            path: "E:/prompts/bugs.md".to_owned()
        }
    );
    assert_eq!(h.file_asks(), 0, "no file picker opens on this PC");
    assert!(
        local_preset_messages(&mut h).is_empty(),
        "the launch went to the remote host alone"
    );
}

#[gpui::test]
fn preset_wizard_folder_row_is_prefilled_and_typed_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, &["r1"], &["r2"]);
    let sources = json!([{"kind": "folder", "relative_path": "docs/prompts"}]);
    open_remote_wizard(&mut h, remote_preset(sources, json!([])));

    assert_eq!(
        path_text(&mut h, "preset-source-folder").as_deref(),
        Some("D:/r2\\docs\\prompts"),
        "the field starts at the default under the source repo"
    );
    h.click_on("preset-source-folder");
    h.keys("ctrl-a backspace");
    type_text(&mut h, "E:/prompts");
    h.click_on("preset-next");
    let ClientMessage::LaunchPreset { source, .. } = launch_remote_from_preview(&mut h) else {
        panic!("a launch went out");
    };
    assert_eq!(
        source,
        LaunchPresetSource::Folder {
            path: "E:/prompts".to_owned()
        }
    );
    assert!(
        local_preset_messages(&mut h).is_empty(),
        "the launch went to the remote host alone"
    );
}

#[gpui::test]
fn preset_wizard_path_variables_take_typed_host_paths_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, &["r1"], &["r2"]);
    let variables = json!([
        {"name": "log", "label": "Log file", "kind": {"kind": "file_path", "extensions": ["log"]},
         "prompt_at_launch": true}
    ]);
    open_remote_wizard(
        &mut h,
        remote_preset(json!([{"kind": "inline"}]), variables),
    );
    h.click_on("preset-source-inline-text");
    type_text(&mut h, "fix");
    h.click_on("preset-next");

    h.click_on("preset-variable-log");
    type_text(&mut h, "E:/logs/run.log");
    assert!(
        !drawn(&mut h, "preset-variable-log-clear"),
        "a typed row is cleared by editing, not by a button"
    );
    h.click_on("preset-next");
    let ClientMessage::LaunchPreset {
        variable_values, ..
    } = launch_remote_from_preview(&mut h)
    else {
        panic!("a launch went out");
    };
    assert_eq!(
        variable_values,
        vec![("log".to_owned(), "E:/logs/run.log".to_owned())]
    );
    assert!(
        local_preset_messages(&mut h).is_empty(),
        "the launch went to the remote host alone"
    );
}

#[gpui::test]
fn preset_wizard_refuses_a_relative_typed_path_on_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, &["r1"], &["r2"]);
    open_remote_wizard(&mut h, remote_preset(json!([{"kind": "file"}]), json!([])));
    assert!(
        !drawn(&mut h, "preset-source-file-error"),
        "an empty row says nothing yet"
    );

    h.click_on("preset-source-file");
    type_text(&mut h, "prompts/bugs.md");
    assert!(drawn(&mut h, "preset-source-file-error"));
    assert_eq!(
        h.root(|root, _| root.preset_wizard_path_refusal("preset-source-file"))
            .as_deref(),
        Some("Use a full path on box, starting with a drive letter, \\\\server or /.")
    );
    assert_eq!(
        h.root(|root, _| root.preset_wizard_next())
            .map(|next| next.enabled),
        Some(false),
        "Next waits for a full path"
    );
    h.click_on("preset-next");
    assert_eq!(h.root(|root, _| root.preset_wizard_stage()), Some("source"));
    assert!(
        local_preset_messages(&mut h).is_empty(),
        "a refused path sends nothing to This PC"
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
    h.send_welcome(PROTOCOL);
    assert!(
        !h.root(|root, _| root.preset_wizard_open()),
        "a new connection closes the wizard"
    );
}

#[gpui::test]
fn escape_waits_while_the_scripts_run(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    preview_issues(&mut h, grouped(scripted(), Some(2)), "7");
    h.click_on("preset-launch");
    let id = scripts_request(&mut h);

    h.keys("escape");
    assert!(
        h.root(|root, _| root.preset_wizard_open()),
        "Esc waits while the scripts run"
    );
    h.click_on("preset-max-per-tab");
    h.keys("escape");
    assert!(
        h.root(|root, _| root.preset_wizard_open()),
        "Esc in a text field waits too"
    );

    h.send(DaemonMessage::PresetScriptsError {
        id,
        error: "exit 1".to_owned(),
        variable_name: None,
    });
    assert!(
        h.root(|root, _| root.preset_wizard_open()),
        "the failed run leaves it open to retry"
    );
    h.keys("escape");
    assert!(
        !h.root(|root, _| root.preset_wizard_open()),
        "Esc closes it once no scripts run"
    );
}

#[gpui::test]
fn a_new_welcome_closes_the_wizard_while_its_scripts_run(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    preview_issues(&mut h, scripted(), "7");
    h.click_on("preset-launch");
    scripts_request(&mut h);
    h.send_welcome(PROTOCOL);
    assert!(
        !h.root(|root, _| root.preset_wizard_open()),
        "a new connection closes it mid-run"
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
    job_with(job_id, status, launched, tabs, &[])
}

/// Job `job_id`'s snapshot naming the sessions it made.
fn job_with(
    job_id: &str,
    status: PresetLaunchJobStatus,
    launched: u32,
    tabs: &[&str],
    sessions: &[&str],
) -> DaemonMessage {
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
            created_session_ids: strings(sessions),
            created_tab_ids: strings(tabs),
            status,
            error,
            current_tab_id: None,
        },
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// The failure message of job `job_id`, naming the sessions it made.
fn failed_with(job_id: &str, sessions: &[&str]) -> DaemonMessage {
    DaemonMessage::PresetLaunchFailed {
        job_id: job_id.to_owned(),
        preset_id: "triage".to_owned(),
        error: "boom".to_owned(),
        partial_session_ids: strings(sessions),
        partial_tab_ids: Vec::new(),
    }
}

/// Job `job_id`'s progress: `launched` of three, in tabs `tabs`.
fn progress(job_id: &str, launched: u32, tabs: &[&str]) -> DaemonMessage {
    DaemonMessage::PresetLaunchProgress {
        job_id: job_id.to_owned(),
        preset_id: "triage".to_owned(),
        total: 3,
        launched,
        current_tab_id: tabs.last().map(|tab| (*tab).to_owned()),
        tab_ids: strings(tabs),
    }
}

/// r1's session `id`, at `status`, as a session update.
fn session_update(id: &str, status: &str) -> DaemonMessage {
    DaemonMessage::SessionUpdated {
        session: session(id).in_repo("r1").status(status).build(),
        request_id: None,
    }
}

/// Job `job_id`'s toast.
fn job_toast(h: &mut Harness<'_>, job_id: &str) -> Toast {
    let key = Some(format!("preset:{job_id}"));
    toasts(h)
        .into_iter()
        .find(|toast| toast.key == key)
        .expect("the job's toast shows")
}

/// The ids of the buttons on job `job_id`'s toast.
fn actions(h: &mut Harness<'_>, job_id: &str) -> Vec<&'static str> {
    job_toast(h, job_id)
        .actions
        .iter()
        .map(|action| action.id)
        .collect()
}

/// Presses button `action` on job `job_id`'s toast.
fn press(h: &mut Harness<'_>, job_id: &str, action: &str) {
    let id = job_toast(h, job_id).id;
    h.click_on(&toast_action_selector(id, action));
}

fn leaf_fill(h: &mut Harness<'_>, id: &str) -> Option<u32> {
    h.root(|root, cx| root.leaf_fill(id, cx))
}

fn active_tab(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.active_tab_id().map(str::to_owned))
}

/// Tab `id` with one empty pane arrives.
fn tab_arrives(h: &mut Harness<'_>, id: &str) {
    h.send(DaemonMessage::TabUpdated {
        tab: tab(id, &pane(&format!("p-{id}"), None)),
    });
}

/// The stops the client sent, by session id.
fn stops(h: &mut Harness<'_>) -> Vec<String> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::StopSession {
                session_id,
                cleanup,
            } => {
                assert!(cleanup.is_empty(), "the worktrees are kept");
                Some(session_id)
            }
            _ => None,
        })
        .collect()
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
fn script_error_marks_the_failing_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let entry = preset(
        json!([{"kind": "github_issues"}]),
        json!([
            {"name": "ticket", "label": "Fetch ticket",
             "kind": {"kind": "script", "cmd": "fetch", "args": ["--id", "7"]}},
            {"name": "branch", "label": "Fetch branch",
             "kind": {"kind": "script", "cmd": "git", "args": ["branch", "--show-current"]}}
        ]),
    );
    preview_issues(&mut h, entry, "7");
    h.click_on("preset-launch");
    let id = scripts_request(&mut h);
    assert!(
        h.bounds("preset-script-row-ticket-failed").origin.x < gpui::px(0.0),
        "no row is marked before the run fails"
    );

    h.send(DaemonMessage::PresetScriptsError {
        id,
        error: "exit 1".to_owned(),
        variable_name: Some("ticket".to_owned()),
    });
    assert!(
        h.bounds("preset-script-row-ticket-failed").origin.x >= gpui::px(0.0),
        "the failing script's row is marked"
    );
    assert!(
        h.bounds("preset-script-row-branch-failed").origin.x < gpui::px(0.0),
        "the other script's row is not"
    );
    assert!(
        h.bounds("preset-script-row-branch").origin.x >= gpui::px(0.0),
        "and it is still drawn"
    );
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
    assert!(shown[0].sticky, "a failure stays until closed");

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

    h.send_welcome(PROTOCOL);
    let kept = toasts(&mut h);
    assert_eq!(
        kept.len(),
        1,
        "a new connection keeps a finished job's toast"
    );
    assert_eq!(kept[0].title, "Launched Triage");
    h.send(job(&job_id, PresetLaunchJobStatus::Spawning, 1, &[]));
    assert_eq!(
        toasts(&mut h)[0].title,
        "Launched Triage",
        "a late event changes nothing"
    );
}

#[gpui::test]
fn running_toast_offers_cancel_select_stop_all(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    assert_eq!(
        actions(&mut h, &job_id),
        ["cancel", "select", "stop-all"],
        "Stop all cancels the running job before a session exists"
    );
    h.send(session_update("s2", "working"));
    h.send(job_with(
        &job_id,
        PresetLaunchJobStatus::Spawning,
        1,
        &["t1"],
        &["s2"],
    ));
    assert_eq!(actions(&mut h, &job_id), ["cancel", "select", "stop-all"]);
    let toast = job_toast(&mut h, &job_id);
    let labels: Vec<&str> = toast.actions.iter().map(|action| action.label).collect();
    assert_eq!(labels, ["Cancel", "Select", "Stop all"]);
    for action in ["cancel", "select", "stop-all"] {
        let button = h.bounds(&toast_action_selector(toast.id, action));
        assert!(
            button.size.width > gpui::px(0.0),
            "{action} is drawn on the toast"
        );
    }
}

#[gpui::test]
fn cancel_sends_cancel_preset_launch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    h.sent();
    press(&mut h, &job_id, "cancel");
    let cancels: Vec<String> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::CancelPresetLaunch { job_id } => Some(job_id),
            _ => None,
        })
        .collect();
    assert_eq!(cancels, [job_id]);
}

#[gpui::test]
fn cancelling_a_remote_launch_goes_to_that_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, &["r1"], &["r2"]);
    open_remote_wizard(&mut h, remote_preset(json!([{"kind": "file"}]), json!([])));
    h.click_on("preset-source-file");
    type_text(&mut h, "E:/prompts/bugs.md");
    h.click_on("preset-next");
    let ClientMessage::LaunchPreset { job_id, .. } = launch_remote_from_preview(&mut h) else {
        panic!("a launch went out");
    };
    h.sent();
    press(&mut h, &job_id, "cancel");
    let cancels: Vec<String> = h
        .host(&remote())
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::CancelPresetLaunch { job_id } => Some(job_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        cancels,
        [job_id],
        "the cancel goes to the host the job runs on"
    );
    assert!(
        local_preset_messages(&mut h).is_empty(),
        "This PC is sent no cancel for the remote job"
    );
}

#[gpui::test]
fn finished_toast_drops_cancel_keeps_select_and_stop_all(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    h.send(session_update("s2", "idle"));
    h.send(job_with(
        &job_id,
        PresetLaunchJobStatus::Completed,
        3,
        &["t1", "t2"],
        &["s2"],
    ));
    assert_eq!(actions(&mut h, &job_id), ["select", "stop-all"]);
    assert!(
        !job_toast(&mut h, &job_id).sticky,
        "a clean success keeps its timer"
    );
}

#[gpui::test]
fn failed_toast_stays_until_closed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    h.send(session_update("s2", "idle"));
    h.send(failed_with(&job_id, &["s2"]));
    h.advance(Duration::from_secs(9));
    assert_eq!(
        actions(&mut h, &job_id),
        ["select", "stop-all"],
        "the warning outlasts a toast's lifetime, buttons and all"
    );
    let id = job_toast(&mut h, &job_id).id;
    h.click_on(&format!("toast-close-{id}"));
    assert!(toasts(&mut h).is_empty(), "the × closes it");
}

#[gpui::test]
fn closing_a_finished_toast_drops_the_job(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let known = |h: &mut Harness<'_>, job_id: &str| {
        let key = format!("preset:{job_id}");
        h.root(|root, _| root.preset_launch_known(&key))
    };
    let closed = launch_three(&mut h);
    h.send(failed_with(&closed, &[]));
    assert!(known(&mut h, &closed));
    let id = job_toast(&mut h, &closed).id;
    h.click_on(&format!("toast-close-{id}"));
    assert!(!known(&mut h, &closed), "the × drops the ended job");
    h.send(job(&closed, PresetLaunchJobStatus::Failed, 1, &[]));
    assert!(
        toasts(&mut h).is_empty(),
        "a closed job's events show nothing"
    );

    let timed = launch_three(&mut h);
    h.send(job(
        &timed,
        PresetLaunchJobStatus::Completed,
        3,
        &["t1", "t2"],
    ));
    assert!(known(&mut h, &timed));
    h.advance(Duration::from_secs(9));
    assert!(toasts(&mut h).is_empty(), "the success timed out");
    assert!(
        !known(&mut h, &timed),
        "a timed-out job is forgotten with its toast"
    );
}

#[gpui::test]
fn stop_all_stops_only_live_created_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    h.send(session_update("s2", "working"));
    h.send(session_update("s3", "stopped"));
    h.send(job_with(
        &job_id,
        PresetLaunchJobStatus::Spawning,
        3,
        &["t1"],
        &["s2", "s3", "gone"],
    ));
    h.send(DaemonMessage::SessionRemoved {
        session_id: "gone".to_owned(),
    });
    h.sent();
    press(&mut h, &job_id, "stop-all");
    assert_eq!(
        stops(&mut h),
        ["s2"],
        "a stopped or removed session is not stopped again, s1 is not the job's"
    );
    h.send(session_update("s2", "stopped"));
    assert_eq!(
        actions(&mut h, &job_id),
        ["select", "stop-all"],
        "the running job keeps Stop all, and its cancel took Cancel"
    );
    h.send(job_with(
        &job_id,
        PresetLaunchJobStatus::Cancelled,
        3,
        &["t1"],
        &["s2", "s3", "gone"],
    ));
    assert_eq!(
        actions(&mut h, &job_id),
        ["select"],
        "Stop all goes once the job ended and nothing it would stop runs"
    );
}

#[gpui::test]
fn stop_all_stops_a_session_whose_snapshot_has_not_arrived(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    h.send(job_with(
        &job_id,
        PresetLaunchJobStatus::Spawning,
        1,
        &["t1"],
        &["s9"],
    ));
    h.sent();
    press(&mut h, &job_id, "stop-all");
    assert_eq!(
        cancels_and_stops(&mut h),
        [format!("cancel {job_id}"), "stop s9".to_owned()],
        "the job reported s9 before its snapshot arrived"
    );
}

#[gpui::test]
fn an_ended_job_offers_no_stop_all_after_a_reconnect_without_its_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = two_repos();
    let mut h = Harness::with(cx, &dir, &fixture);
    let job_id = launch_three(&mut h);
    h.send(job_with(
        &job_id,
        PresetLaunchJobStatus::Completed,
        3,
        &["t1", "t2"],
        &["s1"],
    ));
    assert_eq!(actions(&mut h, &job_id), ["select", "stop-all"], "s1 runs");
    h.send_welcome(PROTOCOL);
    let sessions = fixture
        .sessions
        .iter()
        .filter(|session| session.id != "s1")
        .cloned()
        .collect();
    h.send(DaemonMessage::Sessions { sessions });
    assert_eq!(
        actions(&mut h, &job_id),
        ["select"],
        "the new connection's list lacks s1, discarded while the client was away"
    );
    assert!(
        stops(&mut h).is_empty(),
        "nothing goes to the daemon for the gone s1"
    );
}

#[gpui::test]
fn a_running_job_with_nothing_listed_offers_stop_all(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    h.send(job(&job_id, PresetLaunchJobStatus::Spawning, 0, &[]));
    assert_eq!(actions(&mut h, &job_id), ["cancel", "select", "stop-all"]);
    h.sent();
    press(&mut h, &job_id, "stop-all");
    assert_eq!(
        cancels_and_stops(&mut h),
        [format!("cancel {job_id}")],
        "Stop all cancels the job with no session to stop"
    );
}

#[gpui::test]
fn stop_all_includes_a_failed_jobs_partial_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    h.send(session_update("s2", "working"));
    h.send(session_update("s3", "idle"));
    h.send(job_with(
        &job_id,
        PresetLaunchJobStatus::Spawning,
        1,
        &["t1"],
        &["s3"],
    ));
    h.send(failed_with(&job_id, &["s2"]));
    h.sent();
    press(&mut h, &job_id, "stop-all");
    assert_eq!(stops(&mut h), ["s3", "s2"]);
}

/// Launches a job that makes s1 (the focused session), s2, s3 and s5, all
/// but s5 already listed, and ends it; returns its id.
fn finished_job_of_four(h: &mut Harness<'_>) -> String {
    let job_id = launch_three(h);
    h.send(session_update("s2", "idle"));
    h.send(session_update("s3", "idle"));
    h.send(job_with(
        &job_id,
        PresetLaunchJobStatus::Completed,
        3,
        &["t1", "t2"],
        &["s1", "s2", "s3", "s5"],
    ));
    job_id
}

#[gpui::test]
fn select_launched_tints_job_sessions_and_shows_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = finished_job_of_four(&mut h);
    h.click_on("activity-needs-you");
    assert_eq!(h.root(|root, _| root.activity()), Activity::NeedsYou);
    assert_eq!(leaf_fill(&mut h, "s2"), None, "no tint before Select");
    let tab_before = active_tab(&mut h);

    press(&mut h, &job_id, "select");
    assert_eq!(h.root(|root, _| root.activity()), Activity::Sessions);
    assert_eq!(leaf_fill(&mut h, "s2"), Some(LAUNCHED_TINT));
    assert_eq!(leaf_fill(&mut h, "s3"), Some(LAUNCHED_TINT));
    assert_eq!(
        leaf_fill(&mut h, "s1"),
        Some(SELECTION),
        "the focused leaf keeps its fill"
    );
    assert_eq!(active_tab(&mut h), tab_before, "no focus change");

    h.send(session_update("s5", "idle"));
    assert_eq!(
        leaf_fill(&mut h, "s5"),
        Some(LAUNCHED_TINT),
        "a session listed later is tinted when it appears"
    );
    press(&mut h, &job_id, "select");
    assert!(
        !h.root(|root, _| root.sidebar_collapsed()),
        "a second Select leaves the panel open"
    );
    assert_eq!(h.root(|root, _| root.activity()), Activity::Sessions);
}

#[gpui::test]
fn session_click_clears_the_highlight(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = finished_job_of_four(&mut h);
    press(&mut h, &job_id, "select");
    assert_eq!(leaf_fill(&mut h, "s3"), Some(LAUNCHED_TINT));
    h.click_on("leaf-s2");
    assert_eq!(leaf_fill(&mut h, "s3"), None, "any session click clears it");
}

#[gpui::test]
fn welcome_keeps_finished_job_toasts_and_drops_running_ones(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let finished = launch_three(&mut h);
    h.send(session_update("s2", "idle"));
    h.send(failed_with(&finished, &["s2"]));
    let running = launch_three(&mut h);
    assert_eq!(toasts(&mut h).len(), 2);

    h.send_welcome(PROTOCOL);
    let mut fixture = two_repos();
    fixture
        .sessions
        .push(session("s2").in_repo("r1").status("idle").build());
    h.load(&fixture);
    let keys: Vec<Option<String>> = toasts(&mut h).into_iter().map(|toast| toast.key).collect();
    assert_eq!(keys, [Some(format!("preset:{finished}"))]);
    assert_eq!(actions(&mut h, &finished), ["select", "stop-all"]);
    h.sent();
    press(&mut h, &finished, "stop-all");
    assert_eq!(stops(&mut h), ["s2"], "its buttons still act");
    h.send(job(&running, PresetLaunchJobStatus::Spawning, 1, &[]));
    assert_eq!(
        toasts(&mut h).len(),
        1,
        "the running job is forgotten with its toast"
    );
}

#[gpui::test]
fn welcome_clears_the_highlight(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = finished_job_of_four(&mut h);
    press(&mut h, &job_id, "select");
    assert_eq!(leaf_fill(&mut h, "s2"), Some(LAUNCHED_TINT));
    h.send_welcome(PROTOCOL);
    h.load(&two_repos());
    h.send(session_update("s2", "idle"));
    assert_eq!(leaf_fill(&mut h, "s2"), None);
}

#[gpui::test]
fn first_tab_activated_once_with_tab_grouping(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    h.send(job(&job_id, PresetLaunchJobStatus::Spawning, 1, &["t9"]));
    assert_eq!(
        active_tab(&mut h).as_deref(),
        Some("t1"),
        "t9 is not in yet"
    );
    tab_arrives(&mut h, "t9");
    assert_eq!(
        active_tab(&mut h).as_deref(),
        Some("t9"),
        "the first tab shows on arrival"
    );

    h.click_on("tab-t1");
    h.send(progress(&job_id, 3, &["t9", "t10"]));
    tab_arrives(&mut h, "t10");
    tab_arrives(&mut h, "t9");
    assert_eq!(
        active_tab(&mut h).as_deref(),
        Some("t1"),
        "once per job: neither a later tab nor an update takes over"
    );

    let second = launch_three(&mut h);
    tab_arrives(&mut h, "t20");
    assert_eq!(active_tab(&mut h).as_deref(), Some("t1"));
    h.send(progress(&second, 1, &["t20"]));
    assert_eq!(
        active_tab(&mut h).as_deref(),
        Some("t20"),
        "a first tab already in shows once its id is known"
    );
}

#[gpui::test]
fn no_first_tab_activation_without_tab_grouping(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    preview_issues(&mut h, preset(all_sources(), json!([])), "1,2,3");
    h.click_on("preset-launch");
    let job_id = launched_job(&mut h);
    h.send(job(&job_id, PresetLaunchJobStatus::Spawning, 1, &["t9"]));
    tab_arrives(&mut h, "t9");
    assert_eq!(active_tab(&mut h).as_deref(), Some("t1"));
}

#[gpui::test]
fn progress_bar_fill_is_a_third_after_one_of_three(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    h.send(progress(&job_id, 1, &["t1"]));
    let id = job_toast(&mut h, &job_id).id;
    let track = h.bounds(&format!("toast-progress-{id}"));
    let fill = h.bounds(&format!("toast-progress-fill-{id}"));
    assert!(track.size.width > gpui::px(0.0), "the track is drawn");
    let third = track.size.width / 3.0;
    assert!(
        (fill.size.width - third).abs() <= gpui::px(1.0),
        "the fill is a third of the track: {:?} of {:?}",
        fill.size.width,
        track.size.width
    );
}

/// The cancels and stops the client sent, in order, as `cancel <job>` and
/// `stop <session>`.
fn cancels_and_stops(h: &mut Harness<'_>) -> Vec<String> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::CancelPresetLaunch { job_id } => Some(format!("cancel {job_id}")),
            ClientMessage::StopSession { session_id, .. } => Some(format!("stop {session_id}")),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn cancel_is_dropped_once_pressed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    h.sent();
    press(&mut h, &job_id, "cancel");
    assert_eq!(cancels_and_stops(&mut h), [format!("cancel {job_id}")]);
    assert_eq!(
        actions(&mut h, &job_id),
        ["select", "stop-all"],
        "a second press cannot send another cancel"
    );
    h.send(job(&job_id, PresetLaunchJobStatus::Spawning, 1, &["t1"]));
    assert_eq!(
        actions(&mut h, &job_id),
        ["select", "stop-all"],
        "nor does a snapshot of the still running job bring it back"
    );
}

#[gpui::test]
fn stop_all_on_a_running_launch_cancels_it_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    h.send(session_update("s2", "working"));
    h.send(session_update("s3", "idle"));
    h.send(job_with(
        &job_id,
        PresetLaunchJobStatus::Spawning,
        2,
        &["t1"],
        &["s2", "s3"],
    ));
    h.sent();
    press(&mut h, &job_id, "stop-all");
    assert_eq!(
        cancels_and_stops(&mut h),
        [
            format!("cancel {job_id}"),
            "stop s2".to_owned(),
            "stop s3".to_owned()
        ]
    );
    assert_eq!(actions(&mut h, &job_id), ["select", "stop-all"]);
}

#[gpui::test]
fn select_follows_a_running_launch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = launch_three(&mut h);
    h.send(session_update("s2", "working"));
    h.send(job_with(
        &job_id,
        PresetLaunchJobStatus::Spawning,
        1,
        &["t1"],
        &["s2"],
    ));
    press(&mut h, &job_id, "select");
    assert_eq!(leaf_fill(&mut h, "s2"), Some(LAUNCHED_TINT));
    h.send(job_with(
        &job_id,
        PresetLaunchJobStatus::Spawning,
        2,
        &["t1"],
        &["s2", "s3"],
    ));
    h.send(session_update("s3", "working"));
    assert_eq!(
        leaf_fill(&mut h, "s3"),
        Some(LAUNCHED_TINT),
        "a session the job makes after the click is tinted as it arrives"
    );
}

#[gpui::test]
fn needs_you_row_click_clears_the_highlight(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = finished_job_of_four(&mut h);
    h.send(session_update("s4", "awaiting_input"));
    press(&mut h, &job_id, "select");
    assert_eq!(leaf_fill(&mut h, "s2"), Some(LAUNCHED_TINT));
    h.click_on("activity-needs-you");
    h.click_on("needs-you-row-s4");
    assert_eq!(leaf_fill(&mut h, "s2"), None, "a Needs You click clears it");
}

#[gpui::test]
fn a_programmatic_selection_keeps_the_highlight(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open_wanting(cx, &dir, Some("s2"));
    let fixture = two_repos();
    h.send(DaemonMessage::Repos {
        repos: fixture.repos.clone(),
    });
    h.send(DaemonMessage::Workspaces {
        workspaces: Vec::new(),
    });
    let mut sessions = fixture.sessions.clone();
    sessions.push(session("s2").in_repo("r1").build());
    h.send(DaemonMessage::Sessions { sessions });
    let job_id = launch_three(&mut h);
    h.send(session_update("s3", "idle"));
    h.send(job_with(
        &job_id,
        PresetLaunchJobStatus::Completed,
        3,
        &["t1", "t2"],
        &["s2", "s3"],
    ));
    press(&mut h, &job_id, "select");
    assert_eq!(leaf_fill(&mut h, "s3"), Some(LAUNCHED_TINT));
    h.sent();

    // The layout's arrival lets the command line's session be shown.
    h.send(DaemonMessage::Tabs {
        tabs: fixture.tabs.clone(),
    });
    let placed = h
        .sent()
        .iter()
        .any(|msg| format!("{msg:?}").contains("\"s2\""));
    assert!(placed, "the wanted session s2 was shown");
    assert_eq!(
        leaf_fill(&mut h, "s3"),
        Some(LAUNCHED_TINT),
        "a selection no click made keeps the highlight"
    );
}

#[gpui::test]
fn select_from_a_folded_sidebar_unfolds_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    let job_id = finished_job_of_four(&mut h);
    h.click_on("activity-sessions");
    assert!(h.root(|root, _| root.sidebar_collapsed()), "folded");
    press(&mut h, &job_id, "select");
    assert!(!h.root(|root, _| root.sidebar_collapsed()));
    assert_eq!(h.root(|root, _| root.activity()), Activity::Sessions);
    assert_eq!(leaf_fill(&mut h, "s2"), Some(LAUNCHED_TINT));
}

#[gpui::test]
fn output_frames_skip_the_launch_toast_upkeep(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    launch_three(&mut h);
    let syncs = |h: &mut Harness<'_>| h.root(|root, _| root.preset_launch_syncs());
    let before = syncs(&mut h);
    h.pty_raw("s1", b"output");
    assert_eq!(syncs(&mut h), before, "a terminal frame refits nothing");
    h.send(session_update("s2", "idle"));
    assert!(
        syncs(&mut h) > before,
        "a session update refits the buttons"
    );
}
