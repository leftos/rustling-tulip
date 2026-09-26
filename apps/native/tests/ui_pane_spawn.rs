//! Pane-targeted spawn specs: an empty pane's "New session…" and "Shell
//! here", the stopped-pane overlay's "New session…" that replaces and then
//! discards the stopped session, and the main area's spawn choices when no
//! tab is open.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{TestAppContext, px};
use protocol::{
    ClientMessage, DaemonMessage, SessionMode, SpawnRequest, SpawnTarget, SplitDirection,
    SuggestTarget,
};
use support::{Fixture, Harness, TestDir, pane, repo, session, split, tab};

/// Whether the element tagged `selector` has been painted on screen.
fn painted(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

fn is_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.spawn_dialog_open())
}

fn selected(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.spawn_dialog_selected())
}

fn focused_pane(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.focused_pane())
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

/// The messages that place a session or retire one.
fn placements(sent: Vec<ClientMessage>) -> Vec<ClientMessage> {
    sent.into_iter()
        .filter(|m| {
            matches!(
                m,
                ClientMessage::ReplacePaneSession { .. }
                    | ClientMessage::SplitPane { .. }
                    | ClientMessage::CreateTab { .. }
                    | ClientMessage::DiscardSession { .. }
            )
        })
        .collect()
}

fn is_replace(msg: &ClientMessage, tab: &str, pane: &str, session: &str) -> bool {
    matches!(msg, ClientMessage::ReplacePaneSession { tab_id, pane_id, session_id: Some(id) }
        if tab_id == tab && pane_id == pane && id == session)
}

/// Answers spawn `request` with session `new` of `r1`.
fn reply(h: &mut Harness<'_>, request: &SpawnRequest) {
    h.send(DaemonMessage::SessionUpdated {
        session: session("new").in_repo("r1").build(),
        request_id: request.request_id.clone(),
    });
}

/// Fills the open dialog's branch and submits it; returns the spawn sent.
fn submit(h: &mut Harness<'_>, repo_id: &str) -> SpawnRequest {
    h.send(DaemonMessage::BranchNameSuggestion {
        target: SuggestTarget::Repo {
            repo_id: repo_id.to_owned(),
        },
        name: "wt/brave-fox".to_owned(),
    });
    h.click_on("spawn-submit");
    assert!(!is_open(h), "the submit closed the dialog");
    the_spawn(&h.sent())
}

/// Repos `r0` and `r1`; tab `t1` split into `p1`, showing session `s1` of
/// `r1`, and the empty `p2`.
fn split_fixture() -> Fixture {
    Fixture {
        repos: vec![repo("r0", "C:/r0"), repo("r1", "C:/r1")],
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
    }
}

fn open_with<'a>(cx: &'a mut TestAppContext, dir: &TestDir, fixture: &Fixture) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, fixture);
    h.sent();
    h
}

#[gpui::test]
fn empty_pane_new_session_preselects_the_neighbour_and_fills_the_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_with(cx, &dir, &split_fixture());
    assert_eq!(focused_pane(&mut h).as_deref(), Some("p1"));

    h.click_on("empty-pane-new-session-p2");
    assert!(is_open(&mut h));
    assert_eq!(
        focused_pane(&mut h).as_deref(),
        Some("p2"),
        "the pane first"
    );
    let chosen = selected(&mut h);
    assert!(
        chosen.contains(&"spawn-target-repo-r1".to_owned()),
        "the split sibling's repo, not the first one: {chosen:?}"
    );
    assert!(chosen.contains(&"spawn-placement-current-tab".to_owned()));
    h.sent();

    let request = submit(&mut h, "r1");
    reply(&mut h, &request);
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [msg] if is_replace(msg, "t1", "p2", "new")),
        "the pane takes it, nothing is discarded: {placed:?}"
    );
    assert_eq!(focused_pane(&mut h).as_deref(), Some("p2"));
}

#[gpui::test]
fn empty_pane_new_session_with_new_tab_choice_ignores_the_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_with(cx, &dir, &split_fixture());

    h.click_on("empty-pane-new-session-p2");
    h.click_on("spawn-placement-new-tab");
    let request = submit(&mut h, "r1");
    reply(&mut h, &request);
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [ClientMessage::CreateTab { initial_session_id: Some(id), .. }] if id == "new"),
        "an explicit New tab wins over the pane: {placed:?}"
    );
}

#[gpui::test]
fn empty_pane_shell_here_fills_the_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_with(cx, &dir, &split_fixture());

    h.click_on("empty-pane-shell-p2");
    assert_eq!(
        focused_pane(&mut h).as_deref(),
        Some("p2"),
        "the pane first"
    );
    assert!(!is_open(&mut h), "no dialog");
    let request = the_spawn(&h.sent());
    assert_eq!(request.target, SpawnTarget::Standalone { cwd: None });
    assert_eq!(request.mode, SessionMode::PlainShell);

    reply(&mut h, &request);
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [msg] if is_replace(msg, "t1", "p2", "new")),
        "sent {placed:?}"
    );
    assert_eq!(focused_pane(&mut h).as_deref(), Some("p2"));
}

#[gpui::test]
fn empty_pane_new_session_disabled_without_repos(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = split_fixture();
    fixture.repos.clear();
    let mut h = open_with(cx, &dir, &fixture);
    assert!(painted(&mut h, "empty-pane-new-session-p2"));

    h.click_on("empty-pane-new-session-p2");
    assert!(!is_open(&mut h), "no repo, no dialog");
    assert!(h.sent().is_empty());

    h.click_on("empty-pane-shell-p2");
    assert!(
        !spawns(&h.sent()).is_empty(),
        "a shell needs no repo: Shell here still spawns"
    );
}

/// `s1` of `r1` exited in pane `p1` of tab `t1`, beside `s2` of `r0` in
/// `p2`; repos `r0` and `r1`.
fn stopped_fixture() -> Fixture {
    Fixture {
        repos: vec![repo("r0", "C:/r0"), repo("r1", "C:/r1")],
        sessions: vec![
            session("s1").in_repo("r1").exited(1).build(),
            session("s2").in_repo("r0").build(),
        ],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        )],
        ..Fixture::default()
    }
}

#[gpui::test]
fn overlay_new_session_replaces_the_stopped_session_and_discards_it_last(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_with(cx, &dir, &stopped_fixture());

    h.click_on("exited-new-session-p1");
    assert!(is_open(&mut h));
    let chosen = selected(&mut h);
    assert!(
        chosen.contains(&"spawn-target-repo-r1".to_owned()),
        "the stopped session's repo: {chosen:?}"
    );
    assert!(
        h.sent()
            .iter()
            .all(|m| !matches!(m, ClientMessage::DiscardSession { .. }))
    );

    let request = submit(&mut h, "r1");
    reply(&mut h, &request);
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [
            replace,
            ClientMessage::DiscardSession { session_id, cleanup },
        ] if is_replace(replace, "t1", "p1", "new") && session_id == "s1" && cleanup.is_empty()),
        "the pane is rebound, then the stopped session goes, worktree kept: {placed:?}"
    );
    assert_eq!(focused_pane(&mut h).as_deref(), Some("p1"));
}

#[gpui::test]
fn overlay_new_session_whose_pane_closed_falls_back_to_smart_placement_without_discard(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = open_with(cx, &dir, &stopped_fixture());

    h.click_on("exited-new-session-p1");
    let request = submit(&mut h, "r1");
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p2", Some("s2"))),
    });
    h.sent();

    reply(&mut h, &request);
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [ClientMessage::SplitPane { tab_id, pane_id, new_session_id: Some(id), .. }]
            if tab_id == "t1" && pane_id == "p2" && id == "new"),
        "smart placement in the active tab and no discard: {placed:?}"
    );
}

#[gpui::test]
fn overlay_new_session_rebinds_every_pane_showing_the_stopped_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = stopped_fixture();
    fixture.tabs.push(tab("t2", &pane("p3", Some("s1"))));
    let mut h = open_with(cx, &dir, &fixture);

    h.click_on("exited-new-session-p1");
    let request = submit(&mut h, "r1");
    reply(&mut h, &request);
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [
            first,
            second,
            ClientMessage::DiscardSession { session_id, cleanup },
        ] if is_replace(first, "t1", "p1", "new")
            && is_replace(second, "t2", "p3", "new")
            && session_id == "s1"
            && cleanup.is_empty()),
        "every pane of the stopped session is rebound, then it goes: {placed:?}"
    );
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t1")
    );
    assert_eq!(
        focused_pane(&mut h).as_deref(),
        Some("p1"),
        "the aimed pane"
    );
}

#[gpui::test]
fn double_click_shell_here_spawns_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_with(cx, &dir, &split_fixture());

    h.click_on("empty-pane-shell-p2");
    h.click_on("empty-pane-shell-p2");
    h.click_on("empty-pane-new-session-p2");
    assert!(
        !is_open(&mut h),
        "the pane's New session… waits for the shell too"
    );
    let request = the_spawn(&h.sent());

    reply(&mut h, &request);
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [msg] if is_replace(msg, "t1", "p2", "new")),
        "sent {placed:?}"
    );
}

#[gpui::test]
fn restart_during_a_pane_spawn_keeps_the_restarted_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_with(cx, &dir, &stopped_fixture());

    h.click_on("exited-new-session-p1");
    let request = submit(&mut h, "r1");
    h.click_on("exited-restart-p1");
    let restart = h
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::DuplicateSession {
                session_id,
                request_id,
            } if session_id == "s1" => request_id,
            _ => None,
        })
        .expect("a restart of s1 with a request id");
    h.send(DaemonMessage::SessionUpdated {
        session: session("s1b").in_repo("r1").build(),
        request_id: Some(restart),
    });
    let restarted = placements(h.sent());
    assert!(
        matches!(restarted.as_slice(), [replace, ClientMessage::DiscardSession { session_id, .. }]
            if is_replace(replace, "t1", "p1", "s1b") && session_id == "s1"),
        "the restart took the pane: {restarted:?}"
    );
    h.send(DaemonMessage::TabUpdated {
        tab: tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1b")),
                pane("p2", Some("s2")),
            ),
        ),
    });
    h.sent();

    reply(&mut h, &request);
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [ClientMessage::SplitPane { tab_id, new_session_id: Some(id), .. }]
            if tab_id == "t1" && id == "new"),
        "the new session goes beside the restarted one, which stays, and nothing is discarded: {placed:?}"
    );
}

/// Repos as given and no tab.
fn no_tab_fixture(repos: bool) -> Fixture {
    Fixture {
        repos: if repos {
            vec![repo("r1", "C:/r1")]
        } else {
            Vec::new()
        },
        ..Fixture::default()
    }
}

#[gpui::test]
fn no_tab_empty_state_offers_spawn_and_shell(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_with(cx, &dir, &no_tab_fixture(true));
    assert!(painted(&mut h, "empty-spawn-session"));
    assert!(painted(&mut h, "empty-open-shell"));
    assert!(!h.in_model("empty-repo-hint"), "a repo is registered");

    h.click_on("empty-spawn-session");
    assert!(is_open(&mut h), "opens as the toolbar does");
    h.keys("escape");
    assert!(!is_open(&mut h));
    h.sent();

    h.click_on("empty-open-shell");
    let request = the_spawn(&h.sent());
    assert_eq!(request.target, SpawnTarget::Standalone { cwd: None });
    reply(&mut h, &request);
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [ClientMessage::CreateTab { initial_session_id: Some(id), .. }] if id == "new"),
        "the quick shell in a new tab: {placed:?}"
    );
}

#[gpui::test]
fn no_tab_empty_state_without_repos_shows_the_hint(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_with(cx, &dir, &no_tab_fixture(false));
    assert!(h.in_model("empty-repo-hint"));
    assert!(painted(&mut h, "empty-repo-hint"));

    h.click_on("empty-spawn-session");
    assert!(!is_open(&mut h), "disabled with no repo");
    assert!(h.sent().is_empty());

    h.click_on("empty-open-shell");
    assert!(!spawns(&h.sent()).is_empty(), "a shell needs no repo");
}
