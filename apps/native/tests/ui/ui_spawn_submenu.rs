//! Spawn session submenu specs: a repo's "Spawn session" row swaps the
//! container menu's rows for Last, the host's agents, Shell and the presets;
//! Last and the stored agent's row replay the stored config, another agent
//! opens the dialog held on the repo with that agent chosen, and Shell
//! spawns a plain shell in the repo's folder at once.

#![expect(
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::TestAppContext;
use protocol::{
    ClientMessage, DaemonMessage, PresetEntry, PresetTarget, SessionMode, SpawnConfig,
    SpawnRequest, SpawnTarget, SplitDirection, WorktreeReusePolicy,
};
use serde_json::json;
use support::{Fixture, Harness, TestDir, pane, repo, session, split, tab};

/// The registry's agent the fixtures' daemon lists.
const AGENT: &str = "claude";

/// A stored in-place launch on `r1`'s `feat/x`: the agent with no options
/// when `mode` runs one, else a plain shell.
fn config(mode: SessionMode) -> SpawnConfig {
    let agent = (mode != SessionMode::PlainShell).then(|| {
        serde_json::from_value(json!({ "agent": AGENT, "options": {} })).expect("agent selection")
    });
    SpawnConfig {
        target: SpawnTarget::Single {
            repo_id: "r1".to_owned(),
            branch_name: "feat/x".to_owned(),
            base_branch: None,
            use_worktree: false,
            checkout_strategy: None,
            worktree_reuse: WorktreeReusePolicy::default(),
            existing_worktree: None,
        },
        mode,
        dangerously_skip_permissions: false,
        agent,
        extra_env: Vec::new(),
    }
}

/// Repos `r1`, which last launched `stored`, and `r2`; tab `t1` on screen,
/// split over `s1` and an empty pane, and a second tab `t2`.
fn harness<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    stored: Option<SpawnConfig>,
) -> Harness<'a> {
    let mut fixture = Fixture {
        sessions: vec![session("s1").in_repo("r1").build()],
        tabs: vec![
            tab(
                "t1",
                &split(
                    SplitDirection::Horizontal,
                    pane("p1", Some("s1")),
                    pane("p2", None),
                ),
            ),
            tab("t2", &pane("p3", None)),
        ],
        ..Fixture::default()
    };
    let mut launched = repo("r1", "D:/src/r1");
    launched.last_spawn_config = stored;
    fixture.repos = vec![launched, repo("r2", "D:/src/r2")];
    let mut h = Harness::with(cx, dir, &fixture);
    h.sent();
    h
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

/// Opens `r1`'s menu, answers its preset list with `presets`, and opens the
/// Spawn session submenu; nothing sent is left unread.
fn open_submenu(h: &mut Harness<'_>, presets: Vec<PresetEntry>) {
    h.right_click_on("container-repo:r1");
    assert!(
        h.root(|root, _| root.container_menu_open()),
        "the menu opened"
    );
    let request_id = Some(h.preset_request());
    h.send(DaemonMessage::Presets {
        target: PresetTarget::Repo {
            repo_id: "r1".to_owned(),
        },
        entries: presets,
        request_id,
    });
    h.click_on("container-menu-spawn");
    assert!(
        h.root(|root, _| root.container_menu_open()),
        "the Spawn session row keeps the menu open"
    );
    h.sent();
}

fn groups(h: &mut Harness<'_>) -> Vec<Vec<String>> {
    h.root(|root, _| root.container_menu_groups())
}

fn agent_row() -> String {
    format!("container-spawn-agent-{AGENT}")
}

/// The one `SpawnSession` sent since the last read.
fn spawned(h: &mut Harness<'_>) -> SpawnRequest {
    let sent = h.sent();
    let [ClientMessage::SpawnSession(request)] = sent.as_slice() else {
        panic!("expected one SpawnSession, sent {sent:?}");
    };
    request.clone()
}

/// The daemon answers `request` with session `new` in `r1`; the tab the
/// client then places it in, `None` for a new tab.
fn placed_tab(h: &mut Harness<'_>, request: &SpawnRequest) -> Option<String> {
    h.send(DaemonMessage::SessionUpdated {
        session: session("new").in_repo("r1").build(),
        request_id: request.request_id.clone(),
    });
    let sent = h.sent();
    let placed: Vec<&ClientMessage> = sent
        .iter()
        .filter(|message| {
            matches!(
                message,
                ClientMessage::ReplacePaneSession { .. }
                    | ClientMessage::SplitPane { .. }
                    | ClientMessage::CreateTab { .. }
            )
        })
        .collect();
    match placed.as_slice() {
        [
            ClientMessage::ReplacePaneSession { tab_id, .. }
            | ClientMessage::SplitPane { tab_id, .. },
        ] => Some(tab_id.clone()),
        [ClientMessage::CreateTab { .. }] => None,
        other => panic!("expected one placement, sent {other:?}"),
    }
}

#[gpui::test]
fn the_spawn_row_opens_a_submenu_with_last_agents_shell_and_presets(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, Some(config(SessionMode::Interactive)));
    h.right_click_on("container-repo:r1");
    let top = groups(&mut h).concat();
    assert!(
        top.contains(&"container-menu-spawn".to_owned())
            && top.contains(&"container-launch-last".to_owned()),
        "Launch last again stays beside Spawn session: {top:?}"
    );
    h.keys("escape");

    open_submenu(
        &mut h,
        vec![preset("a", "Bug triage"), preset("b", "Docs sweep")],
    );
    assert_eq!(
        groups(&mut h),
        vec![
            vec!["container-spawn-back".to_owned()],
            vec!["container-spawn-last".to_owned()],
            vec![agent_row()],
            vec!["container-spawn-shell".to_owned()],
            vec![
                "container-menu-launch-preset-0".to_owned(),
                "container-menu-launch-preset-1".to_owned(),
            ],
        ],
        "Back, Last, the agents, Shell, then the presets, a separator between each"
    );
    assert_eq!(
        h.root(|root, _| root.container_menu_labels()),
        [
            "‹ Back",
            "Last (Claude · Interactive · feat/x)",
            "Claude",
            "Shell",
            "Launch preset · Bug triage",
            "Launch preset · Docs sweep",
        ],
        "Last carries the launch summary and the agent row its descriptor's label"
    );

    h.click_on("container-menu-launch-preset-1");
    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "the menu closed"
    );
    assert_eq!(
        h.root(|root, _| root.preset_wizard_title()).as_deref(),
        Some("Launch preset · Docs sweep"),
        "a preset row opens its wizard"
    );
}

/// Opens the submenu on a stored interactive launch, presses `row`, and
/// checks the stored config went out and landed in the tab on screen.
fn assert_row_replays_the_stored_config(cx: &mut TestAppContext, row: &str) {
    let dir = TestDir::new();
    let stored = config(SessionMode::Interactive);
    let mut h = harness(cx, &dir, Some(stored.clone()));
    open_submenu(&mut h, Vec::new());
    h.click_on(row);

    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "{row}: the menu closed"
    );
    assert!(
        !h.root(|root, _| root.spawn_dialog_open()),
        "{row}: no dialog"
    );
    let request = spawned(&mut h);
    assert_eq!(request.target, stored.target, "{row}: the stored target");
    assert_eq!(request.mode, stored.mode, "{row}: the stored mode");
    assert_eq!(request.agent, stored.agent, "{row}: the stored agent");
    assert_eq!(
        request.dangerously_skip_permissions, stored.dangerously_skip_permissions,
        "{row}: the stored trust"
    );
    assert_eq!(
        placed_tab(&mut h, &request).as_deref(),
        Some("t1"),
        "{row}: the replay lands in the tab on screen"
    );
}

#[gpui::test]
fn last_replays_the_stored_config(cx: &mut TestAppContext) {
    assert_row_replays_the_stored_config(cx, "container-spawn-last");
    assert_row_replays_the_stored_config(cx, &agent_row());
}

#[gpui::test]
fn shell_spawns_in_the_container_folder_at_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, None);
    open_submenu(&mut h, Vec::new());
    h.click_on("container-spawn-shell");

    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "the menu closed"
    );
    assert!(!h.root(|root, _| root.spawn_dialog_open()), "no dialog");
    let request = spawned(&mut h);
    assert_eq!(request.mode, SessionMode::PlainShell);
    assert_eq!(request.agent, None);
    assert!(
        matches!(&request.target, SpawnTarget::Standalone { cwd: Some(cwd), .. } if cwd == "D:/src/r1"),
        "a shell in r1's folder: {:?}",
        request.target
    );
    assert_eq!(
        placed_tab(&mut h, &request).as_deref(),
        Some("t1"),
        "the shell lands in the tab on screen"
    );
}

#[gpui::test]
fn another_agent_opens_the_dialog_locked_to_the_container(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, Some(config(SessionMode::PlainShell)));
    open_submenu(&mut h, Vec::new());
    h.click_on(&agent_row());

    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "the menu closed"
    );
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the dialog opened"
    );
    let selected = h.root(|root, _| root.spawn_dialog_selected());
    let targets: Vec<&String> = selected
        .iter()
        .filter(|selector| selector.starts_with("spawn-target-"))
        .collect();
    assert_eq!(
        targets,
        ["spawn-target-repo-r1"],
        "held on r1 alone; selected {selected:?}"
    );
    assert!(
        selected.contains(&format!("spawn-agent-{AGENT}")),
        "the row's agent is chosen over the stored plain shell; selected {selected:?}"
    );
    assert!(
        h.sent()
            .iter()
            .all(|message| !matches!(message, ClientMessage::SpawnSession(_))),
        "nothing spawns before the dialog submits"
    );
}

#[gpui::test]
fn no_last_row_without_a_stored_spawn(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, None);
    open_submenu(&mut h, Vec::new());

    assert_eq!(
        groups(&mut h),
        vec![
            vec!["container-spawn-back".to_owned()],
            vec![agent_row()],
            vec!["container-spawn-shell".to_owned()],
            vec!["container-menu-launch-preset".to_owned()],
        ],
        "no Last row; the presets row says none are defined"
    );

    h.click_on("container-spawn-back");
    assert!(
        groups(&mut h)
            .concat()
            .contains(&"container-menu-spawn".to_owned()),
        "Back returns to the menu's rows"
    );
}
