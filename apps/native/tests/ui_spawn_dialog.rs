//! Spawn dialog specs: Ctrl+Shift+N and "+ Session" open it, plain Ctrl+N
//! stays the terminal's; on open it asks for the branches, a branch name and
//! a fetch; it submits once through the spawn pipeline into the Open-in
//! choice, asks before sharing a live worktree, closes on Esc and on a lost
//! connection but not on a backdrop click, and Tab cycles its controls.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{Modifiers, TestAppContext, point, px};
use protocol::{
    AgentOptions, ClientMessage, DaemonMessage, PermissionMode, RootWorktreeStatus, SessionMode,
    SpawnRequest, SpawnTarget, SuggestTarget, WorktreeInfo,
};
use rustling_tulip_native::RootView;
use support::{Fixture, Harness, TestDir, repo, session};

/// Repos `r0` and `r1`; session `s1` of `r1` alone in pane `p1` of tab `t1`.
fn fixture() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r0", "C:/r0"), repo("r1", "C:/r1")];
    fixture
}

fn is_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.spawn_dialog_open())
}

fn focus(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.spawn_dialog_focus())
}

fn selected(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.spawn_dialog_selected())
}

fn branch(h: &mut Harness<'_>) -> Option<String> {
    h.root(RootView::spawn_dialog_branch)
}

/// Opens the dialog with "+ Session"; returns what it sent.
fn open(h: &mut Harness<'_>) -> Vec<ClientMessage> {
    h.click_on("sidebar-add-session");
    assert!(is_open(h), "the dialog opened");
    h.sent()
}

fn suggest(h: &mut Harness<'_>, repo_id: &str, name: &str) {
    h.send(DaemonMessage::BranchNameSuggestion {
        target: SuggestTarget::Repo {
            repo_id: repo_id.to_owned(),
        },
        name: name.to_owned(),
    });
}

fn spawns(sent: &[ClientMessage]) -> Vec<SpawnRequest> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SpawnSession(request) => Some(request.clone()),
            _ => None,
        })
        .collect()
}

fn the_spawn(sent: &[ClientMessage]) -> SpawnRequest {
    match spawns(sent).as_slice() {
        [request] => request.clone(),
        other => panic!("expected one SpawnSession, sent {other:?}"),
    }
}

fn suggestions(sent: &[ClientMessage], repo_id: &str) -> usize {
    sent.iter()
        .filter(|msg| {
            matches!(msg, ClientMessage::SuggestBranchName {
                target: SuggestTarget::Repo { repo_id: id },
            } if id == repo_id)
        })
        .count()
}

fn lists_branches(sent: &[ClientMessage], repo_id: &str) -> bool {
    sent.iter()
        .any(|msg| matches!(msg, ClientMessage::ListBranches { repo_id: id } if id == repo_id))
}

/// Replies to spawn `request` with session `new` of `r1`.
fn reply(h: &mut Harness<'_>, request: &SpawnRequest) {
    h.send(DaemonMessage::SessionUpdated {
        session: session("new").in_repo("r1").build(),
        request_id: request.request_id.clone(),
    });
}

#[gpui::test]
fn ctrl_shift_n_opens_dialog_from_a_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    let cell = h.cell_center("p1", 0, 0);
    h.click(cell, Modifiers::none());
    h.sent();

    h.keys("ctrl-shift-n");
    assert!(is_open(&mut h));
    assert!(h.sent_input("s1").is_empty(), "the terminal never saw it");
    assert!(
        selected(&mut h).contains(&"spawn-target-repo-r1".to_owned()),
        "the focused pane's repo, not the first one: {:?}",
        selected(&mut h)
    );
    assert_eq!(focus(&mut h).as_deref(), Some("spawn-branch"));
}

#[gpui::test]
fn ctrl_n_in_a_terminal_reaches_the_pty(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    let cell = h.cell_center("p1", 0, 0);
    h.click(cell, Modifiers::none());
    h.sent();

    h.keys("ctrl-n");
    assert_eq!(h.sent_input("s1"), [0x0e]);
    assert!(!is_open(&mut h));

    let panel = h.bounds("sidebar-panel");
    h.click(
        point(panel.center().x, panel.bottom() - px(10.0)),
        Modifiers::none(),
    );
    h.keys("ctrl-n");
    assert!(is_open(&mut h), "outside the terminals Ctrl+N opens it");
    assert!(h.sent_input("s1").is_empty());
}

#[gpui::test]
fn plus_session_disabled_without_repos(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.click_on("sidebar-add-session");
    assert!(!is_open(&mut h), "disabled with no repo");
    h.keys("ctrl-shift-n");
    assert!(!is_open(&mut h), "nor by the shortcut");

    h.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    open(&mut h);
}

#[gpui::test]
fn dialog_requests_branches_suggestion_and_fetch_on_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    let sent = open(&mut h);
    assert!(lists_branches(&sent, "r1"), "sent {sent:?}");
    assert_eq!(suggestions(&sent, "r1"), 1);
    assert!(
        sent.iter()
            .any(|m| matches!(m, ClientMessage::FetchRepo { repo_id } if repo_id == "r1"))
    );

    h.send(DaemonMessage::RepoFetched {
        repo_id: "r1".to_owned(),
        error: None,
    });
    assert!(lists_branches(&h.sent(), "r1"), "the fetch re-lists");
    h.send(DaemonMessage::Branches {
        repo_id: "r1".to_owned(),
        branches: vec!["main".to_owned()],
        current: Some("main".to_owned()),
        remote_branches: vec!["origin/main".to_owned()],
    });
    let base = h.root(RootView::spawn_dialog_base);
    assert_eq!(base.as_deref(), Some("origin/main"));
}

#[gpui::test]
fn random_rerequests_a_suggestion(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    suggest(&mut h, "r1", "wt/brave-fox");
    assert_eq!(branch(&mut h).as_deref(), Some("wt/brave-fox"));

    h.click_on("spawn-branch-random");
    assert_eq!(suggestions(&h.sent(), "r1"), 1, "Random asks again");
    assert_eq!(
        branch(&mut h).as_deref(),
        Some("wt/brave-fox"),
        "kept until the reply"
    );
    suggest(&mut h, "r1", "wt/calm-owl");
    assert_eq!(branch(&mut h).as_deref(), Some("wt/calm-owl"));
}

#[gpui::test]
fn enter_in_branch_field_submits_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    suggest(&mut h, "r1", "wt/brave-fox");
    h.keys("x");
    assert_eq!(branch(&mut h).as_deref(), Some("wt/brave-foxx"));

    h.keys("enter");
    let mut sent = h.sent();
    assert!(!is_open(&mut h), "a submit closes the dialog");
    h.keys("enter");
    sent.extend(h.sent());
    let request = the_spawn(&sent);
    assert!(matches!(
        request.target,
        SpawnTarget::Single { ref branch_name, .. } if branch_name == "wt/brave-foxx"
    ));
}

#[gpui::test]
fn submit_sends_spawn_with_request_id_and_places_reply(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    suggest(&mut h, "r1", "wt/brave-fox");
    h.click_on("spawn-submit");
    let request = the_spawn(&h.sent());
    assert!(request.request_id.is_some());
    assert_eq!(
        request.target,
        SpawnTarget::Single {
            repo_id: "r1".to_owned(),
            branch_name: "wt/brave-fox".to_owned(),
            base_branch: Some("main".to_owned()),
            use_worktree: true,
            checkout_strategy: None,
            worktree_reuse: protocol::WorktreeReusePolicy::default(),
            existing_worktree: None,
        }
    );
    assert!(!request.dangerously_skip_permissions);
    assert!(!is_open(&mut h));
    let toasts = h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|t| t.title.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(toasts, ["Spawning session…"]);

    reply(&mut h, &request);
    let placed = h.sent();
    assert!(
        placed.iter().any(
            |m| matches!(m, ClientMessage::SplitPane { tab_id, pane_id, .. }
            if tab_id == "t1" && pane_id == "p1")
        ),
        "the current tab, by smart placement: {placed:?}"
    );
}

#[gpui::test]
fn submit_into_new_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    suggest(&mut h, "r1", "wt/brave-fox");
    h.click_on("spawn-placement-new-tab");
    assert!(selected(&mut h).contains(&"spawn-placement-new-tab".to_owned()));
    h.click_on("spawn-submit");
    let request = the_spawn(&h.sent());
    reply(&mut h, &request);
    let placed = h.sent();
    assert!(
        placed.iter().any(
            |m| matches!(m, ClientMessage::CreateTab { initial_session_id: Some(id), .. }
            if id == "new")
        ),
        "a tab of its own: {placed:?}"
    );
}

#[gpui::test]
fn escape_closes_and_backdrop_click_does_not(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    let backdrop = h.bounds("spawn-dialog");
    h.click(
        point(backdrop.origin.x + px(4.0), backdrop.origin.y + px(4.0)),
        Modifiers::none(),
    );
    assert!(is_open(&mut h), "a backdrop click keeps the form");

    h.click_on("spawn-agent-codex");
    assert_eq!(focus(&mut h).as_deref(), Some("spawn-agent-codex"));
    h.keys("escape");
    assert!(!is_open(&mut h), "Esc on a button closes");

    open(&mut h);
    assert_eq!(focus(&mut h).as_deref(), Some("spawn-branch"));
    h.keys("escape");
    assert!(!is_open(&mut h), "Esc in the branch field closes");
    assert!(spawns(&h.sent()).is_empty());
}

#[gpui::test]
fn share_worktree_confirm_for_active_existing_worktree(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    h.click_on("spawn-worktree-mode-existing");
    let sent = h.sent();
    assert!(
        sent.iter()
            .any(|m| matches!(m, ClientMessage::ListWorktrees { repo_id } if repo_id == "r1"))
    );
    h.send(DaemonMessage::Worktrees {
        repo_id: "r1".to_owned(),
        worktrees: vec![WorktreeInfo {
            branch: "feat".to_owned(),
            path: "C:/wt/x".to_owned(),
            status: RootWorktreeStatus::Active,
            group_path: None,
            size_bytes: None,
            last_modified_unix: None,
        }],
    });
    h.click_on("spawn-existing-C:/wt/x");
    h.click_on("spawn-submit");
    assert!(spawns(&h.sent()).is_empty(), "not before the confirm");
    assert!(h.root(|root, _| root.spawn_share_confirm_open()));
    assert_eq!(
        focus(&mut h).as_deref(),
        Some("spawn-share-worktree-cancel"),
        "the safe button first"
    );

    h.click_on("spawn-share-worktree-ok");
    let request = the_spawn(&h.sent());
    assert!(matches!(
        request.target,
        SpawnTarget::Single { ref existing_worktree, ref branch_name, .. }
            if existing_worktree.as_deref() == Some("C:/wt/x") && branch_name == "feat"
    ));
    assert!(!is_open(&mut h));
}

#[gpui::test]
fn dialog_closes_on_reconnect(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    h.lose_connection();
    assert!(!is_open(&mut h));
}

#[gpui::test]
fn tab_cycles_controls(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    let mut seen = vec![focus(&mut h).expect("a focus")];
    for _ in 0..7 {
        h.keys("tab");
        seen.push(focus(&mut h).expect("a focus"));
    }
    assert_eq!(
        seen,
        [
            "spawn-branch",
            "spawn-branch-random",
            "spawn-base-branch",
            "spawn-runmode-interactive",
            "spawn-runmode-headless",
            "spawn-skip-perms",
            "spawn-advanced",
            "spawn-cancel"
        ],
        "Spawn is skipped while it cannot go, and collapsed Advanced keeps its controls out"
    );
    assert_eq!(branch(&mut h).as_deref(), Some(""), "Tab typed nothing");
    h.keys("tab");
    assert_eq!(
        focus(&mut h).as_deref(),
        Some("spawn-close"),
        "wraps to the first"
    );
    h.keys("shift-tab");
    assert_eq!(focus(&mut h).as_deref(), Some("spawn-cancel"), "and back");

    h.keys("tab tab tab tab tab");
    assert_eq!(focus(&mut h).as_deref(), Some("spawn-agent-codex"));
    h.keys("space");
    assert!(
        selected(&mut h).contains(&"spawn-agent-codex".to_owned()),
        "Space chooses the focused option"
    );
    assert!(is_open(&mut h));
}

fn prompt(h: &mut Harness<'_>) -> Option<String> {
    h.root(RootView::spawn_dialog_prompt)
}

/// Opens the dialog with a branch name, picks Headless and moves the
/// keyboard to the prompt.
fn open_headless(h: &mut Harness<'_>) {
    open(h);
    suggest(h, "r1", "wt/brave-fox");
    h.click_on("spawn-runmode-headless");
    h.keys("tab");
    assert_eq!(focus(h).as_deref(), Some("spawn-headless-prompt"));
}

#[gpui::test]
fn headless_spawn_sends_the_prompt(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open_headless(&mut h);
    h.keys("space f i x space");
    assert_eq!(prompt(&mut h).as_deref(), Some(" fix "));
    h.keys("ctrl-enter");
    let request = the_spawn(&h.sent());
    assert_eq!(request.mode, SessionMode::Headless);
    assert_eq!(request.initial_prompt.as_deref(), Some("fix"), "trimmed");
    assert!(!is_open(&mut h));
}

#[gpui::test]
fn headless_is_disabled_for_cursor(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    h.click_on("spawn-runmode-headless");
    assert!(selected(&mut h).contains(&"spawn-runmode-headless".to_owned()));
    h.click_on("spawn-agent-cursor");
    let chosen = selected(&mut h);
    assert!(
        chosen.contains(&"spawn-runmode-interactive".to_owned()),
        "cursor snaps back to interactive: {chosen:?}"
    );
    h.click_on("spawn-runmode-headless");
    assert!(
        !selected(&mut h).contains(&"spawn-runmode-headless".to_owned()),
        "a click on the disabled choice does nothing"
    );
}

#[gpui::test]
fn advanced_opens_and_sends_model_approval_and_env(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    suggest(&mut h, "r1", "wt/brave-fox");
    h.click_on("spawn-advanced");
    h.click_on("spawn-model-sonnet");
    assert_eq!(
        h.root(RootView::spawn_dialog_model).as_deref(),
        Some("sonnet")
    );
    h.click_on("spawn-approval-accept-edits");
    h.click_on("spawn-env-add");
    assert_eq!(focus(&mut h).as_deref(), Some("spawn-env-key-0"));
    h.keys("F O O tab b a r");
    h.click_on("spawn-submit");
    let request = the_spawn(&h.sent());
    assert_eq!(request.model.as_deref(), Some("sonnet"));
    assert_eq!(
        request.agent_options,
        AgentOptions::Claude {
            permission_mode: Some(PermissionMode::AcceptEdits)
        }
    );
    assert_eq!(request.extra_env, [("FOO".to_owned(), "bar".to_owned())]);
}

#[gpui::test]
fn invalid_env_key_blocks_spawn(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    suggest(&mut h, "r1", "wt/brave-fox");
    h.click_on("spawn-advanced");
    h.click_on("spawn-env-add");
    h.keys("1");
    h.click_on("spawn-submit");
    assert!(spawns(&h.sent()).is_empty(), "an invalid key blocks Spawn");
    assert!(is_open(&mut h));

    h.click_on("spawn-env-key-0");
    h.keys("backspace a");
    h.click_on("spawn-submit");
    let request = the_spawn(&h.sent());
    assert_eq!(request.extra_env, [("a".to_owned(), String::new())]);
}

#[gpui::test]
fn enter_in_the_prompt_inserts_a_newline_not_a_submit(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open_headless(&mut h);
    h.keys("a enter b");
    assert!(spawns(&h.sent()).is_empty());
    assert!(is_open(&mut h));
    assert_eq!(prompt(&mut h).as_deref(), Some("a\nb"));
    h.keys("ctrl-enter");
    let request = the_spawn(&h.sent());
    assert_eq!(request.initial_prompt.as_deref(), Some("a\nb"));
}

#[gpui::test]
fn closing_action_failed_over_the_dialog_refocuses_the_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    open(&mut h);
    suggest(&mut h, "r1", "wt/brave-fox");
    h.send(DaemonMessage::ActionFailed {
        title: "Resume failed".to_owned(),
        detail: "gone".to_owned(),
        hint: None,
        request_id: None,
    });
    assert!(h.root(|root, _| root.action_failed().is_some()));
    h.keys("escape");
    assert!(h.root(|root, _| root.action_failed().is_none()));
    assert!(is_open(&mut h), "Esc closed the notice, not the dialog");

    h.keys("x");
    assert_eq!(
        branch(&mut h).as_deref(),
        Some("wt/brave-foxx"),
        "the branch field has the keyboard again"
    );
    assert!(h.sent_input("s1").is_empty(), "the terminal did not");
}
