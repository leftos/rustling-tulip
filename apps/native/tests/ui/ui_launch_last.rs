//! Launch-last specs: the row's ▶ shows on hover or focus, a double-click
//! replays the stored config into the current tab, a worktree config waits
//! for a fresh branch name, and the wait's timeout reports the failure.

#![expect(
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::time::Duration;

use gpui::{
    Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point, TestAppContext, point, px,
};
use protocol::{
    ClientMessage, DaemonMessage, SessionMode, SessionSnapshot, SpawnConfig, SpawnTarget,
    SplitDirection, SuggestTarget, TabEntry, WorktreeReusePolicy,
};
use rustling_tulip_native::launch_last;
use rustling_tulip_native::{RootView, ToastKind};
use support::{Fixture, Harness, TestDir, pane, repo, session, split, tab};

const TRUSTED_WARNING: &str = "Review trusted launch";

/// Whether the element tagged `selector` has been painted on screen.
fn painted(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

/// The toast kinds and titles on screen.
fn toasts(h: &mut Harness<'_>) -> Vec<(ToastKind, String)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.kind, toast.title.clone()))
            .collect()
    })
}

/// Whether a trusted launch's warning has been shown.
fn warned_trusted(h: &mut Harness<'_>) -> bool {
    toasts(h)
        .iter()
        .any(|(kind, title)| *kind == ToastKind::Warning && title == TRUSTED_WARNING)
}

/// The failure notice on screen: its title and detail.
fn failure_notice(h: &mut Harness<'_>) -> Option<(String, String)> {
    h.root(|root, _| {
        root.action_failed()
            .map(|notice| (notice.title.clone(), notice.detail.clone()))
    })
}

/// One press and release pair carrying `count`, as the platform reports the
/// clicks of a multiple click.
fn click_with_count(h: &mut Harness<'_>, at: Point<Pixels>, count: usize) {
    h.cx.simulate_event(MouseDownEvent {
        position: at,
        modifiers: Modifiers::none(),
        button: MouseButton::Left,
        click_count: count,
        first_mouse: false,
    });
    h.cx.simulate_event(MouseUpEvent {
        position: at,
        modifiers: Modifiers::none(),
        button: MouseButton::Left,
        click_count: count,
    });
    h.cx.run_until_parked();
}

/// The two click events a platform sends for a double-click: the count-1
/// click first, then the count-2 one.
fn double_click_events(h: &mut Harness<'_>, at: Point<Pixels>) {
    click_with_count(h, at, 1);
    click_with_count(h, at, 2);
}

/// The messages that place a session.
fn placements(sent: Vec<ClientMessage>) -> Vec<ClientMessage> {
    sent.into_iter()
        .filter(|message| {
            matches!(
                message,
                ClientMessage::ReplacePaneSession { .. }
                    | ClientMessage::SplitPane { .. }
                    | ClientMessage::CreateTab { .. }
            )
        })
        .collect()
}

fn is_replace(message: &ClientMessage, tab: &str, pane: &str, session: &str) -> bool {
    matches!(message, ClientMessage::ReplacePaneSession { tab_id, pane_id, session_id: Some(s), host: None }
        if tab_id == tab && pane_id == pane && s == session)
}

fn spawn_count(sent: &[ClientMessage]) -> usize {
    sent.iter()
        .filter(|message| matches!(message, ClientMessage::SpawnSession(_)))
        .count()
}

/// A stored launch config over `target`, with a model and an env row so a
/// replay carries more than the defaults.
fn config(target: SpawnTarget) -> SpawnConfig {
    SpawnConfig {
        target,
        mode: SessionMode::Interactive,
        dangerously_skip_permissions: false,
        agent: Some(serde_json::from_value(serde_json::json!({ "agent": "claude", "options": { "permission_mode": "accept_edits", "model": "opus" } })).expect("agent selection")),
        extra_env: vec![("A".to_owned(), "1".to_owned())],
    }
}

fn single_target(use_worktree: bool) -> SpawnTarget {
    SpawnTarget::Single {
        repo_id: "r1".to_owned(),
        branch_name: "feat/x".to_owned(),
        base_branch: Some("main".to_owned()),
        use_worktree,
        checkout_strategy: None,
        worktree_reuse: WorktreeReusePolicy::default(),
        existing_worktree: None,
    }
}

/// The harness over one repo, `r1`, that last launched with `stored`, and
/// one session in the active tab.
fn harness<'a>(cx: &'a mut TestAppContext, dir: &TestDir, stored: SpawnConfig) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    let mut launched = repo("r1", "D:/src/r1");
    launched.last_spawn_config = Some(stored);
    fixture.repos = vec![launched];
    Harness::with(cx, dir, &fixture)
}

/// The harness over repos `r1` and `r2` with one session of `r1`.
fn two_repos<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
    Harness::with(cx, dir, &fixture)
}

#[gpui::test]
fn launch_last_quick_button_shows_on_hover_or_focus(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_repos(cx, &dir);

    assert!(
        !painted(&mut h, "container-repo:r1-launch"),
        "the ▶ is hidden until the row is hovered or focused"
    );
    h.click_on("container-repo:r1");
    assert!(
        painted(&mut h, "container-repo:r1-launch"),
        "keyboard focus shows the ▶"
    );
    assert!(
        painted(&mut h, "container-repo:r1-focus"),
        "and the ring is on r1"
    );

    let r2 = h.center("container-repo:r2");
    h.hover(r2, Modifiers::default());
    assert!(
        painted(&mut h, "container-repo:r1-launch"),
        "r1 keeps its ▶ by focus while the pointer is over r2"
    );
    assert!(
        !painted(&mut h, "container-repo:r2-focus"),
        "r2 has no keyboard focus"
    );
    assert!(
        painted(&mut h, "container-repo:r2-launch"),
        "a hovered row with no focus shows its ▶ too"
    );
}

#[gpui::test]
fn the_quick_button_hides_once_the_pointer_leaves_the_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_repos(cx, &dir);

    let r1 = h.center("container-repo:r1");
    h.hover(r1, Modifiers::default());
    assert!(
        painted(&mut h, "container-repo:r1-launch"),
        "the hovered row shows its ▶"
    );
    h.root_update(|root, _, cx| root.set_window_hover_override(Some(false), cx));
    assert!(
        !painted(&mut h, "container-repo:r1-launch"),
        "the ▶ goes once the pointer has left the window"
    );
}

#[gpui::test]
fn a_fold_clears_the_row_hover(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_repos(cx, &dir);

    // The keyboard in the sidebar, so Ctrl+B folds it.
    let panel = h.bounds("sidebar-panel");
    h.click(
        point(panel.center().x, panel.bottom() - px(10.0)),
        Modifiers::none(),
    );
    let r1 = h.center("container-repo:r1");
    h.hover(r1, Modifiers::default());
    assert!(
        painted(&mut h, "container-repo:r1-launch"),
        "the hovered row shows its ▶"
    );
    h.keys("ctrl-b");
    assert!(h.root(|root, _| root.sidebar_collapsed()), "folded");
    h.click_on("activity-sessions");
    assert!(!h.root(|root, _| root.sidebar_collapsed()), "unfolded");
    assert!(
        !painted(&mut h, "container-repo:r1-launch"),
        "the ▶ does not come back with the unfold"
    );
}

#[gpui::test]
fn launch_last_double_click_replays_into_current_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let stored = config(single_target(false));
    let mut fixture = Fixture {
        sessions: vec![session("s1").in_repo("r1").build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", None),
            ),
        )],
        ..Fixture::default()
    };
    let mut launched = repo("r1", "D:/src/r1");
    launched.last_spawn_config = Some(stored.clone());
    fixture.repos = vec![launched];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();

    let row = h.center("container-repo:r1");
    click_with_count(&mut h, row, 1);
    assert!(!h.in_model("leaf-s1"), "the first click folds the row");
    assert!(h.sent().is_empty(), "and launches nothing");

    click_with_count(&mut h, row, 2);
    let sent = h.sent();
    let [ClientMessage::SpawnSession(request)] = sent.as_slice() else {
        panic!("expected one SpawnSession, sent {sent:?}");
    };
    let expected = stored.to_duplicate_request(None);
    assert_eq!(
        request.target, expected.target,
        "the target replays verbatim"
    );
    assert_eq!(request.mode, expected.mode);
    assert_eq!(request.agent, expected.agent);
    assert_eq!(request.extra_env, expected.extra_env);
    assert_eq!(request.label, None, "a replay names no label");
    assert_eq!(request.initial_prompt, None, "and carries no prompt");
    assert_eq!(request.prompt_injector, None, "and no injector");
    assert!(request.request_id.is_some(), "it goes out under an id");

    h.send(DaemonMessage::SessionUpdated {
        session: session("new").in_repo("r1").build(),
        request_id: request.request_id.clone(),
    });
    let placed = placements(h.sent());
    assert!(
        matches!(
            placed.as_slice(),
            [replace] if is_replace(replace, "t1", "p2", "new")
        ),
        "the replay fills the active tab's empty pane: {placed:?}"
    );
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t1"),
        "and the active tab took it"
    );
    assert_eq!(
        h.root(|root, _| root.focused_pane()).as_deref(),
        Some("p2"),
        "the new pane has the focus"
    );
}

#[gpui::test]
fn launch_last_worktree_config_waits_for_a_branch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, config(single_target(true)));
    h.sent();

    let row = h.center("container-repo:r1");
    h.double_click(row);

    let sent = h.sent();
    let [
        ClientMessage::SuggestBranchName {
            target: SuggestTarget::Repo { repo_id },
            request_id,
        },
    ] = sent.as_slice()
    else {
        panic!("expected one SuggestBranchName, sent {sent:?}");
    };
    assert_eq!(
        repo_id, "r1",
        "a worktree config asks for a branch name first"
    );
    let request_id = request_id.clone().expect("the wait names its request");
    assert!(
        !sent
            .iter()
            .any(|message| matches!(message, ClientMessage::SpawnSession(_))),
        "and spawns nothing until the daemon answers"
    );

    h.send(DaemonMessage::BranchNameSuggestion {
        target: SuggestTarget::Repo {
            repo_id: "r1".to_owned(),
        },
        name: "feat/fresh".to_owned(),
        request_id: Some(request_id),
    });

    let sent = h.sent();
    let [ClientMessage::SpawnSession(request)] = sent.as_slice() else {
        panic!("expected one SpawnSession after the suggestion, sent {sent:?}");
    };
    let SpawnTarget::Single {
        branch_name,
        worktree_reuse,
        existing_worktree,
        ..
    } = &request.target
    else {
        panic!("expected a single-repo target, got {:?}", request.target);
    };
    assert_eq!(branch_name, "feat/fresh", "the fresh name replaces the old");
    assert_eq!(*worktree_reuse, WorktreeReusePolicy::RefuseLeftover);
    assert_eq!(*existing_worktree, None, "a duplicate never reuses a pin");
}

#[gpui::test]
fn launch_last_suggestion_timeout_shows_an_error(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, config(single_target(true)));
    h.sent();

    let row = h.center("container-repo:r1");
    double_click_events(&mut h, row);
    h.sent();

    h.advance(Duration::from_secs(11));

    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|message| matches!(message, ClientMessage::SpawnSession(_))),
        "the wait expires without spawning, sent {sent:?}"
    );
    let toasts = h.root(|root, _| root.toasts().to_vec());
    assert!(
        toasts.iter().any(|toast| toast.kind == ToastKind::Error
            && toast.title == "Couldn't pick a branch name for launch-last"),
        "an error toast names the failure, got {toasts:?}"
    );
}

#[gpui::test]
fn a_lost_connection_drops_the_launch_last_wait(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, config(single_target(true)));
    h.sent();

    let row = h.center("container-repo:r1");
    double_click_events(&mut h, row);
    assert!(
        h.sent()
            .iter()
            .any(|message| matches!(message, ClientMessage::SuggestBranchName { .. })),
        "the launch waits for a branch name"
    );

    h.lose_connection();
    h.advance(Duration::from_secs(11));

    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|message| matches!(message, ClientMessage::SpawnSession(_))),
        "a dropped wait spawns nothing, sent {sent:?}"
    );
    let toasts = toasts(&mut h);
    assert!(
        !toasts.iter().any(|(kind, title)| *kind == ToastKind::Error
            && title == "Couldn't pick a branch name for launch-last"),
        "and its timeout reports nothing, got {toasts:?}"
    );
}

#[gpui::test]
fn a_suggestion_for_another_request_does_not_fill_the_spawn_form(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, config(single_target(true)));
    h.click_on("sidebar-add-session");
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the dialog opened"
    );
    assert_eq!(
        h.root(RootView::spawn_dialog_branch).as_deref(),
        Some(""),
        "it waits for a suggestion with an empty field"
    );
    h.sent();

    h.send(DaemonMessage::BranchNameSuggestion {
        target: SuggestTarget::Repo {
            repo_id: "r1".to_owned(),
        },
        name: "wt/another-request".to_owned(),
        request_id: Some("launch-last-0".to_owned()),
    });
    assert_eq!(
        h.root(RootView::spawn_dialog_branch).as_deref(),
        Some(""),
        "a launch-last reply fills nothing in the dialog"
    );

    h.send(DaemonMessage::BranchNameSuggestion {
        target: SuggestTarget::Repo {
            repo_id: "r1".to_owned(),
        },
        name: "wt/its-own".to_owned(),
        request_id: None,
    });
    assert_eq!(
        h.root(RootView::spawn_dialog_branch).as_deref(),
        Some("wt/its-own"),
        "the dialog's own reply still fills it"
    );
}

#[gpui::test]
fn launch_last_without_a_config_opens_the_locked_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_repos(cx, &dir);

    let row = h.center("container-repo:r1");
    h.double_click(row);

    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "a row with no stored config opens the spawn dialog"
    );
    let chosen = h.root(|root, _| root.spawn_dialog_selected());
    assert!(
        chosen.contains(&"spawn-target-repo-r1".to_owned()),
        "locked on the row's repo: {chosen:?}"
    );
    assert!(
        painted(&mut h, "spawn-target-repo-r1"),
        "the locked target is on screen"
    );
    assert!(
        !painted(&mut h, "spawn-target-repo-r2"),
        "and the other repo is not offered"
    );
    assert_eq!(
        spawn_count(&h.sent()),
        0,
        "nothing replays from a row with no config"
    );
}

#[gpui::test]
fn launch_last_with_a_trusted_config_prefills_the_dialog_and_warns(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut stored = config(single_target(false));
    stored.dangerously_skip_permissions = true;
    let mut h = harness(cx, &dir, stored);

    let row = h.center("container-repo:r1");
    h.double_click(row);

    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "a trusted config opens the full dialog"
    );
    let chosen = h.root(|root, _| root.spawn_dialog_selected());
    assert!(
        chosen.contains(&"spawn-target-repo-r1".to_owned()),
        "on the row's repo: {chosen:?}"
    );
    assert!(
        chosen.contains(&"spawn-skip-perms".to_owned()),
        "with trusted launch prefilled: {chosen:?}"
    );
    assert_eq!(
        h.root(RootView::spawn_dialog_branch).as_deref(),
        Some("feat/x"),
        "an in-place prefill keeps the saved branch"
    );
    assert!(
        warned_trusted(&mut h),
        "the dialog is preceded by the Review trusted launch warning"
    );
    assert_eq!(
        spawn_count(&h.sent()),
        0,
        "the trusted launch waits in the dialog"
    );
}

#[gpui::test]
fn launch_last_of_a_headless_config_launches_nothing_and_warns(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut stored = config(single_target(false));
    stored.mode = SessionMode::Headless;
    let mut h = harness(cx, &dir, stored);

    let row = h.center("container-repo:r1");
    h.double_click(row);

    assert_eq!(
        spawn_count(&h.sent()),
        0,
        "a headless config is never replayed"
    );
    assert!(
        !h.root(|root, _| root.spawn_dialog_open()),
        "nor opened in the dialog"
    );
    assert!(
        toasts(&mut h).contains(&(
            ToastKind::Warning,
            "Headless runs aren't supported in this client".to_owned()
        )),
        "a warning says why: {:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn pressing_the_quick_button_replays_without_folding_the_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, config(single_target(false)));
    h.sent();

    let row = h.center("container-repo:r1");
    h.hover(row, Modifiers::default());
    assert!(painted(&mut h, "container-repo:r1-launch"));
    h.click_on("container-repo:r1-launch");

    let sent = h.sent();
    let [ClientMessage::SpawnSession(request)] = sent.as_slice() else {
        panic!("expected one SpawnSession, sent {sent:?}");
    };
    assert!(
        request.request_id.is_some(),
        "it replays like a double-click"
    );
    assert_eq!(
        h.root(|root, _| root.sidebar_row_focus().map(str::to_owned)),
        None,
        "the ▶ moves no row focus"
    );
    assert!(h.in_model("leaf-s1"), "and never folds the row");
}

#[gpui::test]
fn a_double_press_on_the_quick_button_launches_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, config(single_target(false)));
    h.sent();

    let row = h.center("container-repo:r1");
    h.hover(row, Modifiers::default());
    h.click_on("container-repo:r1-launch");
    h.click_on("container-repo:r1-launch");

    assert_eq!(
        spawn_count(&h.sent()),
        1,
        "the second press is the same gesture as the first"
    );
}

#[gpui::test]
fn a_triple_click_on_the_row_launches_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, config(single_target(false)));
    h.sent();

    let row = h.center("container-repo:r1");
    click_with_count(&mut h, row, 1);
    click_with_count(&mut h, row, 2);
    click_with_count(&mut h, row, 3);

    assert_eq!(
        spawn_count(&h.sent()),
        1,
        "the third click lands inside the same gesture"
    );
}

#[gpui::test]
fn a_consumed_suggestion_does_not_seed_the_next_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, config(single_target(true)));
    h.sent();

    let row = h.center("container-repo:r1");
    h.double_click(row);
    let sent = h.sent();
    let [ClientMessage::SuggestBranchName { request_id, .. }] = sent.as_slice() else {
        panic!("expected one SuggestBranchName, sent {sent:?}");
    };
    let request_id = request_id.clone().expect("the wait names its request");
    h.send(DaemonMessage::BranchNameSuggestion {
        target: SuggestTarget::Repo {
            repo_id: "r1".to_owned(),
        },
        name: "feat/fresh".to_owned(),
        request_id: Some(request_id),
    });
    assert_eq!(
        spawn_count(&h.sent()),
        1,
        "the suggestion is consumed by the launch"
    );

    h.click_on("sidebar-add-session");
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the dialog opens for r1"
    );
    assert_eq!(
        h.root(RootView::spawn_dialog_branch).as_deref(),
        Some(""),
        "the taken branch is not seeded"
    );
    assert!(
        h.sent().iter().any(|message| matches!(
            message,
            ClientMessage::SuggestBranchName {
                target: SuggestTarget::Repo { repo_id },
                request_id: None,
            } if repo_id == "r1"
        )),
        "so the dialog asks for a fresh name"
    );
}

#[gpui::test]
fn a_trusted_launch_with_a_dialog_open_shows_no_warning(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut stored = config(single_target(false));
    stored.dangerously_skip_permissions = true;
    let mut h = harness(cx, &dir, stored);

    h.click_on("sidebar-add-session");
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the first dialog opened"
    );

    h.root_update(|root, window, cx| {
        root.launch_last_row(
            launch_last::Container::Repo("r1"),
            launch_last::Placement::CurrentTab,
            window,
            cx,
        );
    });

    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the open dialog stays"
    );
    assert!(
        !warned_trusted(&mut h),
        "no warning for a dialog that never opened"
    );
    assert!(
        !h.root(|root, _| root.spawn_dialog_selected())
            .contains(&"spawn-skip-perms".to_owned()),
        "and the open dialog was not replaced by the trusted one"
    );
    assert_eq!(
        failure_notice(&mut h),
        Some((
            "Couldn't review the trusted launch".to_owned(),
            "Couldn't open the spawn dialog: another dialog is open".to_owned()
        )),
        "the refused launch says why"
    );
}

#[gpui::test]
fn a_trusted_launch_refused_by_an_open_dialog_says_why(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut stored = config(single_target(false));
    stored.dangerously_skip_permissions = true;
    let mut h = harness(cx, &dir, stored);

    h.click_on("sidebar-add-session");
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the first dialog opened"
    );

    h.root_update(|root, window, cx| {
        root.launch_last_row(
            launch_last::Container::Repo("r1"),
            launch_last::Placement::CurrentTab,
            window,
            cx,
        );
    });

    assert!(
        !h.root(|root, _| root.spawn_dialog_selected())
            .contains(&"spawn-skip-perms".to_owned()),
        "the open dialog kept its own configuration"
    );
    assert!(
        !warned_trusted(&mut h),
        "no warning for a dialog that never opened"
    );
    assert_eq!(spawn_count(&h.sent()), 0, "nothing spawned");
    assert_eq!(
        failure_notice(&mut h),
        Some((
            "Couldn't review the trusted launch".to_owned(),
            "Couldn't open the spawn dialog: another dialog is open".to_owned()
        )),
        "the refused launch says why"
    );
}

/// The harness over repo `r1` with tabs `t1` (showing `s1` beside an empty
/// pane) and `t2` (an empty pane), and `stored` as its last launch.
fn two_tabs<'a>(
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
    fixture.repos = vec![launched];
    Harness::with(cx, dir, &fixture)
}

/// The request id of the one `SpawnSession` a replay sent.
fn spawned_request(h: &mut Harness<'_>) -> String {
    let sent = h.sent();
    let [ClientMessage::SpawnSession(request)] = sent.as_slice() else {
        panic!("expected one SpawnSession, sent {sent:?}");
    };
    request
        .request_id
        .clone()
        .expect("a replay names its request")
}

/// The tab a placement message puts the session in; `None` for a new tab.
fn placed_tab(message: &ClientMessage) -> Option<&str> {
    match message {
        ClientMessage::ReplacePaneSession { tab_id, .. }
        | ClientMessage::SplitPane { tab_id, .. } => Some(tab_id),
        _ => None,
    }
}

#[gpui::test]
fn the_container_menu_offers_launch_last_only_with_a_stored_config(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    {
        let mut h = two_repos(cx, &dir);
        h.right_click_on("container-repo:r1");
        assert!(
            h.root(|root, _| root.container_menu_open()),
            "the menu opened"
        );
        assert!(
            !h.root(|root, _| root.container_menu_groups())
                .concat()
                .contains(&"container-launch-last".to_owned()),
            "a container that never launched offers no Launch last again row"
        );
        h.keys("escape");
        assert!(!h.root(|root, _| root.container_menu_open()));
    }
    {
        let mut h = two_tabs(cx, &dir, Some(config(single_target(false))));
        h.right_click_on("container-repo:r1");
        assert!(
            h.root(|root, _| root.container_menu_groups())
                .concat()
                .contains(&"container-launch-last".to_owned()),
            "a stored config adds the row"
        );
    }
}

#[gpui::test]
fn the_launch_last_submenu_lists_the_placements_in_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir, Some(config(single_target(false))));
    h.right_click_on("container-repo:r1");
    h.click_on("container-launch-last");

    assert_eq!(
        h.root(|root, _| root.container_menu_groups()),
        vec![
            vec![
                "container-launch-last-back".to_owned(),
                "container-launch-last-current".to_owned(),
                "container-launch-last-new".to_owned(),
            ],
            vec![
                "container-launch-last-tab-t1".to_owned(),
                "container-launch-last-tab-t2".to_owned(),
            ],
            vec!["container-launch-last-edit".to_owned()],
        ],
        "Back, the two placements, the grid tabs, then Edit first"
    );
    assert_eq!(
        h.root(|root, _| root.container_menu_labels()),
        [
            "‹ Back",
            "Current tab",
            "New tab",
            "t1",
            "t2",
            "Edit first…"
        ],
        "the tab rows carry the tabs' names"
    );
}

#[gpui::test]
fn container_launch_current_tab_replays_into_the_active_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir, Some(config(single_target(false))));
    h.sent();
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t1"),
        "t1 is the tab on screen"
    );

    h.right_click_on("container-repo:r1");
    h.sent(); // opening the menu asked for its presets
    h.click_on("container-launch-last");
    h.click_on("container-launch-last-current");
    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "the menu closed on the choice"
    );

    let request_id = spawned_request(&mut h);
    h.send(DaemonMessage::SessionUpdated {
        session: session("new").in_repo("r1").build(),
        request_id: Some(request_id),
    });
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [message] if placed_tab(message) == Some("t1")),
        "the replay lands in the tab on screen: {placed:?}"
    );
}

#[gpui::test]
fn container_launch_new_tab_replays_into_a_new_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir, Some(config(single_target(false))));
    h.sent();

    h.right_click_on("container-repo:r1");
    h.sent(); // opening the menu asked for its presets
    h.click_on("container-launch-last");
    h.click_on("container-launch-last-new");

    let request_id = spawned_request(&mut h);
    h.send(DaemonMessage::SessionUpdated {
        session: session("new").in_repo("r1").build(),
        request_id: Some(request_id),
    });
    let placed = placements(h.sent());
    assert!(
        matches!(
            placed.as_slice(),
            [ClientMessage::CreateTab {
                initial_session_id: Some(placed_session),
                ..
            }] if placed_session == "new"
        ),
        "a new tab takes the replay: {placed:?}"
    );
}

#[gpui::test]
fn container_launch_named_tab_replays_into_that_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir, Some(config(single_target(false))));
    h.sent();

    h.right_click_on("container-repo:r1");
    h.sent(); // opening the menu asked for its presets
    h.click_on("container-launch-last");
    h.click_on("container-launch-last-tab-t2");

    let request_id = spawned_request(&mut h);
    h.send(DaemonMessage::SessionUpdated {
        session: session("new").in_repo("r1").build(),
        request_id: Some(request_id),
    });
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [message] if placed_tab(message) == Some("t2")),
        "the named tab takes the replay: {placed:?}"
    );
}

#[gpui::test]
fn container_launch_edit_first_opens_the_prefilled_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir, Some(config(single_target(false))));
    h.sent();

    h.right_click_on("container-repo:r1");
    h.click_on("container-launch-last");
    h.click_on("container-launch-last-edit");

    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "the menu gave way to the dialog"
    );
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "Edit first opens the spawn dialog"
    );
    let chosen = h.root(|root, _| root.spawn_dialog_selected());
    assert!(
        chosen.contains(&"spawn-target-repo-r1".to_owned()),
        "held on the row's repo: {chosen:?}"
    );
    assert_eq!(
        h.root(RootView::spawn_dialog_branch).as_deref(),
        Some("feat/x"),
        "and prefilled with the stored branch"
    );
    assert_eq!(spawn_count(&h.sent()), 0, "nothing replays from the dialog");
}

#[gpui::test]
fn a_trusted_launch_from_the_submenu_preselects_the_chosen_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut stored = config(single_target(false));
    stored.dangerously_skip_permissions = true;
    let mut h = two_tabs(cx, &dir, Some(stored));
    h.sent();

    h.right_click_on("container-repo:r1");
    h.click_on("container-launch-last");
    h.click_on("container-launch-last-new");

    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "a trusted config opens the full dialog"
    );
    let chosen = h.root(|root, _| root.spawn_dialog_selected());
    assert!(
        chosen.contains(&"spawn-placement-new-tab".to_owned()),
        "the submenu's New tab stays chosen: {chosen:?}"
    );
    assert!(
        chosen.contains(&"spawn-skip-perms".to_owned()),
        "with trusted launch prefilled: {chosen:?}"
    );
    assert!(warned_trusted(&mut h), "the warning still shows");
    assert_eq!(
        spawn_count(&h.sent()),
        0,
        "the trusted launch waits in the dialog"
    );
}

#[gpui::test]
fn a_tab_switch_during_the_branch_wait_leaves_the_placement_where_the_click_named(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir, Some(config(single_target(true))));
    h.sent();

    h.right_click_on("container-repo:r1");
    h.sent(); // opening the menu asked for its presets
    h.click_on("container-launch-last");
    h.click_on("container-launch-last-current");
    let sent = h.sent();
    let [ClientMessage::SuggestBranchName { request_id, .. }] = sent.as_slice() else {
        panic!("expected one SuggestBranchName, sent {sent:?}");
    };
    let request_id = request_id.clone().expect("the wait names its request");

    h.click_on("tab-t2");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t2"),
        "the tab switch took"
    );
    h.sent();

    h.send(DaemonMessage::BranchNameSuggestion {
        target: SuggestTarget::Repo {
            repo_id: "r1".to_owned(),
        },
        name: "feat/fresh".to_owned(),
        request_id: Some(request_id),
    });
    let request_id = spawned_request(&mut h);
    h.send(DaemonMessage::SessionUpdated {
        session: session("new").in_repo("r1").build(),
        request_id: Some(request_id),
    });
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [message] if placed_tab(message) == Some("t1")),
        "the click's tab, not the one on screen at the reply: {placed:?}"
    );
}

/// The Detached container's sessions, under one tab that shows none of them.
fn detached(leaves: Vec<SessionSnapshot>) -> Fixture {
    Fixture {
        sessions: leaves,
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    }
}

/// The sessions each `StopSession` in `sent` names, with its cleanup checked
/// empty.
fn stops(h: &mut Harness<'_>) -> Vec<String> {
    h.sent()
        .into_iter()
        .filter_map(|message| match message {
            ClientMessage::StopSession {
                session_id,
                cleanup,
            } => {
                assert!(cleanup.is_empty(), "a stop keeps the worktree");
                Some(session_id)
            }
            _ => None,
        })
        .collect()
}

/// The Detached banner's Stop all label, while the chip is shown.
fn stop_all_label(h: &mut Harness<'_>) -> Option<&'static str> {
    h.root(|root, _| root.detached_stop_all_label())
}

#[gpui::test]
fn the_detached_stop_all_chip_shows_only_while_a_leaf_runs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    {
        let mut h = Harness::with(cx, &dir, &detached(vec![session("s1").exited(0).build()]));
        assert!(
            !painted(&mut h, "detached-stop-all"),
            "every leaf stopped, so the chip is not drawn"
        );
        assert_eq!(stop_all_label(&mut h), None, "and no label is offered");
    }

    let mut h = Harness::with(
        cx,
        &dir,
        &detached(vec![
            session("s1").build(),
            session("s2").status("working").build(),
        ]),
    );
    assert!(
        painted(&mut h, "detached-stop-all"),
        "a running leaf draws the chip"
    );
    assert_eq!(stop_all_label(&mut h), Some("Stop all"), "unarmed at first");

    h.click_on("detached-stop-all");
    assert_eq!(
        stop_all_label(&mut h),
        Some("Confirm: stop all"),
        "the first click arms it"
    );
    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").exited(0).build(),
        request_id: None,
    });
    h.send(DaemonMessage::SessionUpdated {
        session: session("s2").exited(0).build(),
        request_id: None,
    });
    assert_eq!(
        stop_all_label(&mut h),
        None,
        "the last running leaf stopping takes the chip and its arm"
    );
    h.send(DaemonMessage::SessionUpdated {
        session: session("s2").build(),
        request_id: None,
    });
    assert_eq!(
        stop_all_label(&mut h),
        Some("Stop all"),
        "a live leaf brings it back unarmed"
    );
}

#[gpui::test]
fn the_detached_stop_all_first_click_only_arms(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &detached(vec![
            session("s1").build(),
            session("s2").status("working").build(),
        ]),
    );
    assert!(stops(&mut h).is_empty(), "startup stops nothing");

    h.click_on("detached-stop-all");

    assert!(
        stops(&mut h).is_empty(),
        "the first click arms without sending a stop"
    );
    assert_eq!(
        stop_all_label(&mut h),
        Some("Confirm: stop all"),
        "and the chip asks to confirm"
    );
}

#[gpui::test]
fn the_detached_stop_all_second_click_stops_every_running_leaf(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &detached(vec![
            session("s1").build(),
            session("s2").status("working").build(),
            session("s3").exited(0).build(),
        ]),
    );
    h.sent();
    h.click_on("detached-stop-all");
    assert!(stops(&mut h).is_empty(), "the first click only arms");

    h.click_on("detached-stop-all");

    assert_eq!(
        stops(&mut h),
        vec!["s1".to_owned(), "s2".to_owned()],
        "one stop per running leaf, and none for the stopped one"
    );
    assert_eq!(
        stop_all_label(&mut h),
        Some("Stop all"),
        "the chip disarms after the second click"
    );
}

#[gpui::test]
fn escape_disarms_the_detached_stop_all(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &detached(vec![session("s1").build()]));
    h.sent();
    h.click_on("detached-stop-all");
    assert_eq!(stop_all_label(&mut h), Some("Confirm: stop all"));

    h.keys("escape");

    assert_eq!(
        stop_all_label(&mut h),
        Some("Stop all"),
        "Esc disarms the chip"
    );
    assert!(stops(&mut h).is_empty(), "and sends nothing");
}

#[gpui::test]
fn the_detached_stop_all_arm_does_not_survive_a_collapsed_sidebar(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &detached(vec![session("s1").build()]));
    h.sent();
    h.click_on("detached-stop-all");
    assert_eq!(stop_all_label(&mut h), Some("Confirm: stop all"));

    h.keys("ctrl-b");
    assert_eq!(
        stop_all_label(&mut h),
        None,
        "a folded sidebar draws no chip"
    );
    h.keys("ctrl-b");

    assert_eq!(
        stop_all_label(&mut h),
        Some("Stop all"),
        "the chip returns unarmed"
    );
    h.click_on("detached-stop-all");
    assert!(
        stops(&mut h).is_empty(),
        "one click only arms the returned chip"
    );
}

#[gpui::test]
fn the_detached_stop_all_arm_does_not_survive_an_activity_change(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &detached(vec![session("s1").build()]));
    h.sent();
    h.click_on("detached-stop-all");
    assert_eq!(stop_all_label(&mut h), Some("Confirm: stop all"));

    h.click_on("activity-needs-you");
    assert_eq!(stop_all_label(&mut h), None, "another panel draws no chip");
    h.click_on("activity-sessions");

    assert_eq!(
        stop_all_label(&mut h),
        Some("Stop all"),
        "the chip returns unarmed"
    );
    h.click_on("detached-stop-all");
    assert!(
        stops(&mut h).is_empty(),
        "one click only arms the returned chip"
    );
}

#[gpui::test]
fn a_hidden_sidebar_reports_no_stop_all_label(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &detached(vec![session("s1").build()]));
    h.click_on("detached-stop-all");
    assert_eq!(stop_all_label(&mut h), Some("Confirm: stop all"));

    h.keys("ctrl-b");

    assert_eq!(
        stop_all_label(&mut h),
        None,
        "the label follows the chip out of a folded sidebar"
    );
}

#[gpui::test]
fn a_failed_suggestion_drops_the_wait_and_reports_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, config(single_target(true)));
    h.sent();

    let row = h.center("container-repo:r1");
    double_click_events(&mut h, row);
    let sent = h.sent();
    let [ClientMessage::SuggestBranchName { request_id, .. }] = sent.as_slice() else {
        panic!("expected one SuggestBranchName, sent {sent:?}");
    };
    let request_id = request_id.clone().expect("the wait names its request");

    h.send(DaemonMessage::Error {
        message: "suggest failed".to_owned(),
        request_id: Some(request_id),
        code: None,
    });

    let errors: Vec<_> = toasts(&mut h)
        .into_iter()
        .filter(|(kind, _)| *kind == ToastKind::Error)
        .collect();
    assert_eq!(
        errors.len(),
        1,
        "the failure shows one error toast, got {errors:?}"
    );
    assert_eq!(
        spawn_count(&h.sent()),
        0,
        "and the failed suggestion spawns nothing"
    );

    h.advance(Duration::from_secs(11));
    let toasts = toasts(&mut h);
    assert!(
        !toasts.iter().any(|(kind, title)| *kind == ToastKind::Error
            && title == "Couldn't pick a branch name for launch-last"),
        "the dropped wait adds no timeout toast, got {toasts:?}"
    );
    assert_eq!(
        spawn_count(&h.sent()),
        0,
        "and still spawns nothing past the deadline"
    );
}

/// The harness over r1 with no live sessions and a stored launch config, two
/// empty tabs on screen.
fn empty_repo_launch<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut fixture = Fixture {
        tabs: vec![tab("t1", &pane("p1", None)), tab("t2", &pane("p2", None))],
        ..Fixture::default()
    };
    let mut launched = repo("r1", "D:/src/r1");
    launched.last_spawn_config = Some(config(single_target(false)));
    fixture.repos = vec![launched];
    Harness::with(cx, dir, &fixture)
}

/// A non-grid tab, so the app has a tab but no tab a placement row can name.
fn diff_tab(id: &str) -> TabEntry {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "name": id,
        "content": {
            "kind": "diff",
            "repo_id": "r1",
            "path": "src/lib.rs",
            "against": null,
            "worktree_path": null,
        },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("diff tab fixture")
}

#[gpui::test]
fn an_armed_remove_row_does_not_survive_the_launch_last_submenu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = empty_repo_launch(cx, &dir);
    h.sent();

    h.right_click_on("container-repo:r1");
    h.sent(); // opening the menu asked for its presets
    h.click_on("container-menu-remove");
    assert_eq!(
        h.root(|root, _| root.container_menu_labels()),
        [
            "Spawn session",
            "Launch last again ▸",
            "Appearance…",
            "Launch preset… (loading)",
            "Open in Explorer",
            "Open in VS Code",
            "Copy path",
            "Confirm: remove repo…"
        ],
        "the first click arms the Remove row"
    );
    assert!(h.sent().is_empty(), "arming sends nothing");

    h.click_on("container-launch-last");
    h.click_on("container-launch-last-back");

    assert_eq!(
        h.root(|root, _| root.container_menu_labels()),
        [
            "Spawn session",
            "Launch last again ▸",
            "Appearance…",
            "Launch preset… (loading)",
            "Open in Explorer",
            "Open in VS Code",
            "Copy path",
            "Remove repo"
        ],
        "Back returns the row unarmed"
    );
    h.click_on("container-menu-remove");
    assert!(
        h.sent().is_empty(),
        "its first click only arms; nothing destructive goes out"
    );
}

#[gpui::test]
fn back_returns_the_launch_last_submenu_to_the_top_level(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir, Some(config(single_target(false))));
    h.right_click_on("container-repo:r1");
    h.click_on("container-launch-last");

    h.click_on("container-launch-last-back");

    assert_eq!(
        h.root(|root, _| root.container_menu_groups()),
        vec![
            vec!["container-menu-spawn".to_owned()],
            vec!["container-launch-last".to_owned()],
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
        "Spawn, Launch last again, appearance and preset, the open rows, then Remove"
    );
    assert_eq!(
        h.root(|root, _| root.container_menu_labels()),
        [
            "Spawn session",
            "Launch last again ▸",
            "Appearance…",
            "Launch preset… (loading)",
            "Open in Explorer",
            "Open in VS Code",
            "Copy path",
            "Remove repo"
        ],
    );
}

#[gpui::test]
fn escape_closes_the_whole_menu_from_the_launch_last_submenu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir, Some(config(single_target(false))));
    h.right_click_on("container-repo:r1");
    h.click_on("container-launch-last");
    assert!(
        h.root(|root, _| root.container_menu_groups())
            .concat()
            .iter()
            .any(|selector| selector == "container-launch-last-back"),
        "the submenu is open"
    );

    h.keys("escape");

    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "Esc closes the menu rather than returning to its rows"
    );
}

#[gpui::test]
fn a_click_outside_closes_the_whole_menu_from_the_launch_last_submenu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_tabs(cx, &dir, Some(config(single_target(false))));
    h.sent();
    h.right_click_on("container-repo:r1");
    h.sent(); // opening the menu asked for its presets
    h.click_on("container-launch-last");

    let outside = h.center("new-tab");
    h.click(outside, Modifiers::none());

    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "a click outside closes the menu rather than returning to its rows"
    );
    assert!(
        h.sent().is_empty(),
        "the click that closes the menu reaches nothing under it"
    );
}

#[gpui::test]
fn the_launch_last_submenu_names_no_tab_when_no_grid_tab_is_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture {
        tabs: vec![diff_tab("d1")],
        ..Fixture::default()
    };
    let mut launched = repo("r1", "D:/src/r1");
    launched.last_spawn_config = Some(config(single_target(false)));
    fixture.repos = vec![launched];
    let mut h = Harness::with(cx, &dir, &fixture);

    h.right_click_on("container-repo:r1");
    h.click_on("container-launch-last");

    let groups = h.root(|root, _| root.container_menu_groups());
    assert!(
        !groups
            .concat()
            .iter()
            .any(|selector| selector.starts_with("container-launch-last-tab-")),
        "a diff tab has no placement row: {groups:?}"
    );
    assert_eq!(
        groups,
        vec![
            vec![
                "container-launch-last-back".to_owned(),
                "container-launch-last-current".to_owned(),
                "container-launch-last-new".to_owned(),
            ],
            vec!["container-launch-last-edit".to_owned()],
        ],
        "only Back, the two placements and Edit first"
    );
}
