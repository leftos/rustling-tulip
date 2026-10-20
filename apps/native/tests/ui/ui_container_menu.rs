//! Container menu specs: a repo's and workspace's rows (spawn, launch last,
//! appearance, preset, Explorer, VS Code, copy path, remove), a tab
//! container's spawn entry, the spawn dialog locked to the container or the
//! tab, and the opener and clipboard each row drives.

use crate::support;

use std::path::PathBuf;

use gpui::{TestAppContext, px};
use protocol::{ClientMessage, DaemonMessage, PresetEntry, PresetTarget, WorkspaceEntry};
use rustling_tulip_native::ToastKind;
use serde_json::json;
use support::{Fixture, Harness, Opened, TestDir, pane, repo, session, tab};

/// A workspace over `members`, linked to `linked`'s `.code-workspace` when
/// given.
fn workspace_at(id: &str, members: &[&str], linked: Option<&str>) -> WorkspaceEntry {
    serde_json::from_value(json!({
        "id": id, "name": id, "member_repo_ids": members,
        "linked_vscode_workspace": linked,
    }))
    .expect("workspace fixture")
}

/// Repos `r1` and `r2`, one session of `r1` alone in pane `p1` of tab `t1`.
fn two_repos() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
    fixture
}

/// The two-repo fixture with `ws1` over `members`, linked or not.
fn with_workspace(members: &[&str], linked: Option<&str>) -> Fixture {
    let mut fixture = two_repos();
    fixture.workspaces = vec![workspace_at("ws1", members, linked)];
    fixture
}

/// The two-repo fixture with two grid tabs, `t1` active over `s1` in `r1`
/// and `t2` over `s2` in `r2`.
fn two_tabs() -> Fixture {
    let mut fixture = two_repos();
    fixture.sessions.push(session("s2").in_repo("r2").build());
    fixture.tabs = vec![
        tab("t1", &pane("p1", Some("s1"))),
        tab("t2", &pane("p2", Some("s2"))),
    ];
    fixture
}

fn open(h: &mut Harness<'_>, selector: &str) {
    h.right_click_on(selector);
    assert!(
        h.root(|root, _| root.container_menu_open()),
        "the menu opened"
    );
}

#[gpui::test]
fn container_menu_lists_spawn_preset_explorer_vscode_copy_and_remove(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open(&mut h, "container-repo:r1");

    assert_eq!(
        h.root(|root, _| root.container_menu_groups()),
        vec![
            vec!["container-menu-spawn".to_owned()],
            vec![
                "container-menu-appearance".to_owned(),
                "container-menu-launch-preset".to_owned(),
            ],
            vec![
                "container-menu-explorer".to_owned(),
                "container-menu-vscode".to_owned(),
                "container-menu-copy-path".to_owned(),
            ],
            vec!["container-menu-remove".to_owned()],
        ],
        "spawn, appearance and preset, the open rows, then Remove"
    );
    assert_eq!(
        h.root(|root, _| root.container_menu_labels()),
        [
            "Spawn session",
            "Appearance…",
            "Launch preset… (loading)",
            "Open in Explorer",
            "Open in VS Code",
            "Copy path",
            "Remove repo",
        ]
    );
}

#[gpui::test]
fn spawn_on_a_repo_opens_the_dialog_locked_to_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open(&mut h, "container-repo:r1");
    h.click_on("container-menu-spawn");

    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "the menu closed"
    );
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the dialog opened"
    );
    let selected = h.root(|root, _| root.spawn_dialog_selected());
    let targets: Vec<String> = selected
        .iter()
        .filter(|selector| selector.starts_with("spawn-target-"))
        .cloned()
        .collect();
    assert_eq!(
        targets,
        ["spawn-target-repo-r1".to_owned()],
        "held on r1 alone; selected {selected:?}"
    );
}

#[gpui::test]
fn spawn_on_a_tab_container_fixes_open_in_to_that_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_tabs());
    h.click_on("sidebar-view-tabs");
    open(&mut h, "container-tab:t2");
    h.click_on("container-menu-spawn");

    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the dialog opened"
    );
    let selected = h.root(|root, _| root.spawn_dialog_selected());
    assert!(
        selected.contains(&"spawn-placement-tab-t2".to_owned()),
        "the placement is fixed to t2; selected {selected:?}"
    );
    assert!(!selected.contains(&"spawn-placement-current-tab".to_owned()));
    assert!(!selected.contains(&"spawn-placement-new-tab".to_owned()));
    assert!(
        h.bounds("spawn-placement-tab-t2").origin.x >= px(0.0),
        "the tab's chip is drawn"
    );
    assert!(
        h.bounds("spawn-placement-new-tab").origin.x < px(0.0),
        "no other placement is offered"
    );
    let targets: Vec<String> = selected
        .iter()
        .filter(|selector| selector.starts_with("spawn-target-"))
        .cloned()
        .collect();
    assert_eq!(
        targets,
        ["spawn-target-repo-r2".to_owned()],
        "t2's session's repo, not the active tab's"
    );
}

#[gpui::test]
fn a_tab_row_without_a_target_lock_keeps_the_picker(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_tabs());
    h.click_on("sidebar-add-session");

    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the dialog opened"
    );
    for selector in ["spawn-placement-current-tab", "spawn-placement-new-tab"] {
        assert!(
            h.bounds(selector).origin.x >= px(0.0),
            "{selector} stays on screen"
        );
    }
    assert!(
        h.bounds("spawn-placement-tab-t2").origin.x >= px(0.0),
        "the other tab is offered"
    );
}

/// A preset named `name` with an inline source, as the daemon lists it.
fn preset(id: &str, name: &str) -> PresetEntry {
    serde_json::from_value(json!({
        "id": id, "name": name, "source_repo_id": "r1",
        "prompt_sources": [{"kind": "inline"}],
        "prompt_template": "{prompt}", "branch_template": "tmp/{index}",
        "tab_grouping": {"kind": "none"},
        "injector": {"startup_delay_ms": 0, "pre_input": [], "post_input": []}
    }))
    .expect("preset fixture")
}

fn repo_target(id: &str) -> PresetTarget {
    PresetTarget::Repo {
        repo_id: id.to_owned(),
    }
}

/// The `ListPresets` requests the client sent since the last read.
fn preset_requests(h: &mut Harness<'_>) -> Vec<PresetTarget> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ListPresets { target } => Some(target),
            _ => None,
        })
        .collect()
}

fn preset_labels(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.container_menu_labels())
        .into_iter()
        .filter(|label| label.starts_with("Launch preset"))
        .collect()
}

#[gpui::test]
fn the_preset_row_loads_on_every_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.sent();
    open(&mut h, "container-repo:r1");

    assert_eq!(
        preset_requests(&mut h),
        [repo_target("r1")],
        "opening asks for the repo's presets"
    );
    assert_eq!(preset_labels(&mut h), ["Launch preset… (loading)"]);
    assert_eq!(
        h.root(|root, _| root.container_menu_disabled_tip("container-menu-launch-preset")),
        None,
        "the loading row has no tooltip"
    );
    h.click_on("container-menu-launch-preset");
    assert!(
        h.root(|root, _| root.container_menu_open()),
        "the loading row cannot be pressed"
    );

    h.send(DaemonMessage::Presets {
        target: repo_target("r1"),
        entries: Vec::new(),
    });
    h.keys("escape");
    open(&mut h, "container-repo:r1");
    assert_eq!(
        preset_requests(&mut h),
        [repo_target("r1")],
        "a reopen asks again rather than reuse the last list"
    );
    assert_eq!(preset_labels(&mut h), ["Launch preset… (loading)"]);

    h.keys("escape");
    h.load(&with_workspace(&["r1", "r2"], None));
    open(&mut h, "container-ws:ws1");
    assert_eq!(
        preset_requests(&mut h),
        [PresetTarget::Workspace {
            workspace_id: "ws1".to_owned()
        }],
        "a workspace asks for its members' presets"
    );
}

#[gpui::test]
fn a_failed_list_shows_the_error_without_a_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open(&mut h, "container-repo:r1");
    h.send(DaemonMessage::Error {
        message: "presets.json: expected value at line 1".to_owned(),
        request_id: None,
    });

    assert_eq!(preset_labels(&mut h), ["Launch preset… (failed to load)"]);
    assert_eq!(
        h.root(|root, _| root.container_menu_disabled_tip("container-menu-launch-preset")),
        Some("presets.json: expected value at line 1".to_owned())
    );
    assert!(
        h.root(|root, _| root.toasts().is_empty()),
        "the failure shows in the row, not as a toast"
    );

    h.send(DaemonMessage::Error {
        message: "something else".to_owned(),
        request_id: None,
    });
    assert_eq!(
        h.root(|root, _| root.toasts().len()),
        1,
        "once the rows have loaded, an unrouted error toasts as before"
    );
    assert_eq!(preset_labels(&mut h), ["Launch preset… (failed to load)"]);
}

#[gpui::test]
fn no_presets_shows_the_none_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open(&mut h, "container-repo:r1");
    h.send(DaemonMessage::Presets {
        target: repo_target("r1"),
        entries: Vec::new(),
    });

    assert_eq!(preset_labels(&mut h), ["Launch preset… (none defined)"]);
    assert_eq!(
        h.root(|root, _| root.container_menu_disabled_tip("container-menu-launch-preset")),
        Some("No .rustling-tulip/presets.json in this repo".to_owned())
    );
}

#[gpui::test]
fn listed_presets_get_a_row_each(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open(&mut h, "container-repo:r1");
    h.send(DaemonMessage::Presets {
        target: repo_target("r1"),
        entries: vec![preset("a", "Bug triage"), preset("b", "Docs sweep")],
    });

    assert_eq!(
        preset_labels(&mut h),
        ["Launch preset · Bug triage", "Launch preset · Docs sweep"]
    );
    assert_eq!(
        h.root(|root, _| root.container_menu_groups())[1],
        [
            "container-menu-appearance".to_owned(),
            "container-menu-launch-preset-0".to_owned(),
            "container-menu-launch-preset-1".to_owned(),
        ]
    );
    assert_eq!(
        h.root(|root, _| root.container_menu_disabled_tip("container-menu-launch-preset-1")),
        None,
        "a preset row is enabled"
    );
}

#[gpui::test]
fn a_preset_row_closes_the_menu_and_opens_the_wizard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open(&mut h, "container-repo:r1");
    h.send(DaemonMessage::Presets {
        target: repo_target("r1"),
        entries: vec![preset("a", "Bug triage"), preset("b", "Docs sweep")],
    });
    h.click_on("container-menu-launch-preset-1");

    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "the menu closed"
    );
    assert_eq!(
        h.root(|root, _| root.preset_wizard_title()).as_deref(),
        Some("Launch preset · Docs sweep")
    );
}

#[gpui::test]
fn a_reply_for_another_container_is_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open(&mut h, "container-repo:r1");
    h.send(DaemonMessage::Presets {
        target: repo_target("r2"),
        entries: vec![preset("a", "Elsewhere")],
    });
    assert_eq!(preset_labels(&mut h), ["Launch preset… (loading)"]);

    h.keys("escape");
    h.send(DaemonMessage::Presets {
        target: repo_target("r1"),
        entries: vec![preset("a", "Late")],
    });
    open(&mut h, "container-repo:r2");
    h.send(DaemonMessage::Presets {
        target: repo_target("r1"),
        entries: vec![preset("a", "Moved")],
    });
    assert_eq!(
        preset_labels(&mut h),
        ["Launch preset… (loading)"],
        "a reply for a closed or moved menu fills nothing"
    );
}

#[gpui::test]
fn a_failed_row_takes_its_own_list_when_it_arrives(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open(&mut h, "container-repo:r1");
    h.keys("escape");
    open(&mut h, "container-repo:r2");
    h.send(DaemonMessage::Error {
        message: "r1's presets.json is malformed".to_owned(),
        request_id: None,
    });
    assert_eq!(preset_labels(&mut h), ["Launch preset… (failed to load)"]);

    h.send(DaemonMessage::Presets {
        target: repo_target("r2"),
        entries: vec![preset("a", "Bug triage")],
    });
    assert_eq!(
        preset_labels(&mut h),
        ["Launch preset · Bug triage"],
        "the menu's own list replaces a failure it was charged with"
    );
}

#[gpui::test]
fn a_tab_container_has_no_preset_tip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_tabs());
    h.click_on("sidebar-view-tabs");
    open(&mut h, "container-tab:t2");
    assert_eq!(
        h.root(|root, _| root.container_menu_disabled_tip("container-menu-launch-preset")),
        None,
        "a tab container draws no preset row, so it has no tooltip"
    );
}

#[gpui::test]
fn explorer_opens_the_repo_folder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open(&mut h, "container-repo:r1");
    h.click_on("container-menu-explorer");

    assert_eq!(h.opened(), [Opened::DefaultApp(PathBuf::from("D:/src/r1"))]);
    assert!(!h.root(|root, _| root.container_menu_open()));
}

#[gpui::test]
fn vscode_opens_a_repo_folder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    open(&mut h, "container-repo:r1");
    h.click_on("container-menu-vscode");

    assert_eq!(
        h.opened(),
        [Opened::VsCodeFolder(vec![PathBuf::from("D:/src/r1")])]
    );
}

#[gpui::test]
fn vscode_opens_a_linked_workspace_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = with_workspace(&["r1", "r2"], Some("D:/ws/team.code-workspace"));
    let mut h = Harness::with(cx, &dir, &fixture);
    open(&mut h, "container-ws:ws1");
    h.click_on("container-menu-vscode");

    assert_eq!(
        h.opened(),
        [Opened::VsCodeWorkspace(PathBuf::from(
            "D:/ws/team.code-workspace"
        ))]
    );
}

#[gpui::test]
fn vscode_opens_workspace_members_multi_root(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_workspace(&["r1", "r2"], None));
    open(&mut h, "container-ws:ws1");
    h.click_on("container-menu-vscode");

    assert_eq!(
        h.opened(),
        [Opened::VsCodeFolder(vec![
            PathBuf::from("D:/src/r1"),
            PathBuf::from("D:/src/r2"),
        ])]
    );
}

#[gpui::test]
fn copy_path_copies_the_repo_folder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.set_clipboard("");
    open(&mut h, "container-repo:r1");
    h.click_on("container-menu-copy-path");

    assert_eq!(h.clipboard().as_deref(), Some("D:/src/r1"));
    assert!(!h.root(|root, _| root.container_menu_open()));
}

#[gpui::test]
fn vscode_missing_falls_back_to_the_file_manager_and_default_app(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let warnings = |h: &mut Harness<'_>| {
        h.root(|root, _| {
            root.toasts()
                .iter()
                .filter(|toast| {
                    toast.kind == ToastKind::Warning && toast.title == "VS Code not found"
                })
                .count()
        })
    };

    {
        let mut h = Harness::with(cx, &dir, &two_repos());
        h.fail_vscode_opens();
        open(&mut h, "container-repo:r1");
        h.click_on("container-menu-vscode");
        assert_eq!(
            h.opened(),
            [Opened::DefaultApp(PathBuf::from("D:/src/r1"))],
            "a folder opens in the file manager"
        );
        assert_eq!(warnings(&mut h), 1, "the fallback says VS Code is missing");
    }

    {
        let fixture = with_workspace(&["r1", "r2"], Some("D:/ws/team.code-workspace"));
        let mut h = Harness::with(cx, &dir, &fixture);
        h.fail_vscode_opens();
        open(&mut h, "container-ws:ws1");
        h.click_on("container-menu-vscode");
        assert_eq!(
            h.opened(),
            [Opened::DefaultApp(PathBuf::from(
                "D:/ws/team.code-workspace"
            ))],
            "a workspace file opens in its default app"
        );
        assert_eq!(warnings(&mut h), 1, "the fallback says VS Code is missing");
    }
}

#[gpui::test]
fn workspace_explorer_and_copy_are_disabled(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = with_workspace(&["r1", "r2"], None);
    fixture.workspaces = vec![
        workspace_at("ws1", &["r1", "r2"], None),
        workspace_at("ws2", &["missing"], None),
    ];
    let mut h = Harness::with(cx, &dir, &fixture);

    open(&mut h, "container-ws:ws1");
    for selector in ["container-menu-explorer", "container-menu-copy-path"] {
        assert_eq!(
            h.root(|root, _| root.container_menu_disabled_tip(selector))
                .as_deref(),
            Some("This container has no folder"),
            "{selector} is disabled"
        );
    }
    assert_eq!(
        h.root(|root, _| root.container_menu_disabled_tip("container-menu-vscode"))
            .as_deref(),
        None,
        "a workspace with member folders opens in VS Code"
    );
    h.click_on("container-menu-explorer");
    assert_eq!(h.opened(), [], "the disabled Explorer row opens nothing");

    h.keys("escape");
    open(&mut h, "container-ws:ws2");
    assert_eq!(
        h.root(|root, _| root.container_menu_disabled_tip("container-menu-vscode"))
            .as_deref(),
        Some("This container has no folder"),
        "a workspace with no resolvable folder cannot open in VS Code"
    );
}
