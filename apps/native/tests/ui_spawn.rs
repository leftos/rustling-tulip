//! Spawn specs: a spawn sends its request under a request id with a toast,
//! only its own reply is placed, by the Open-in choice, and focused; the
//! daemon's errors show as a toast, the action-failed modal or the checkout
//! prompt, which hold the keyboard and close on reconnect.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::time::Duration;

use gpui::{TestAppContext, px};
use protocol::{
    AgentOptions, CheckoutStrategy, ClientMessage, DaemonMessage, SessionMode, SessionSnapshot,
    SpawnRequest, SpawnTarget, SplitDirection, WorktreeReusePolicy,
};
use rustling_tulip_native::{OpenIn, TOAST_LIFETIME};
use support::{Fixture, Harness, TestDir, pane, session, split, tab};

const SPAWNING: (&str, Option<&str>) = (
    "Spawning session…",
    Some("Worktree creation may take a few seconds."),
);

/// A claude spawn on branch `feature` of `r1`, in a worktree or in place.
fn request(use_worktree: bool) -> SpawnRequest {
    SpawnRequest {
        label: None,
        target: SpawnTarget::Single {
            repo_id: "r1".to_owned(),
            branch_name: "feature".to_owned(),
            base_branch: None,
            use_worktree,
            checkout_strategy: None,
            worktree_reuse: WorktreeReusePolicy::default(),
            existing_worktree: None,
        },
        mode: SessionMode::Interactive,
        initial_prompt: None,
        dangerously_skip_permissions: false,
        agent_options: AgentOptions::Claude {
            permission_mode: None,
        },
        model: None,
        extra_env: Vec::new(),
        prompt_injector: None,
        request_id: None,
    }
}

/// The request of the one message sent, a spawn with an id.
fn spawned(sent: &[ClientMessage]) -> SpawnRequest {
    match sent {
        [ClientMessage::SpawnSession(request)] if request.request_id.is_some() => request.clone(),
        other => panic!("expected one SpawnSession with an id, sent {other:?}"),
    }
}

/// Spawns `request` into `open_in` through the view's entry point; returns
/// the request id it went out under.
fn spawn(h: &mut Harness<'_>, request: SpawnRequest, open_in: OpenIn) -> String {
    let root = h.root.clone();
    h.cx.update(|_, cx| root.update(cx, |root, cx| root.spawn(request, open_in, cx)));
    spawned(&h.sent()).request_id.expect("a request id")
}

fn updated(session: SessionSnapshot, request_id: Option<&str>) -> DaemonMessage {
    DaemonMessage::SessionUpdated {
        session,
        request_id: request_id.map(str::to_owned),
    }
}

/// The messages that place a session.
fn placements(sent: Vec<ClientMessage>) -> Vec<ClientMessage> {
    sent.into_iter()
        .filter(|m| {
            matches!(
                m,
                ClientMessage::ReplacePaneSession { .. }
                    | ClientMessage::SplitPane { .. }
                    | ClientMessage::CreateTab { .. }
            )
        })
        .collect()
}

fn is_replace(msg: &ClientMessage, tab: &str, pane: &str, session: &str) -> bool {
    matches!(msg, ClientMessage::ReplacePaneSession { tab_id, pane_id, session_id: Some(s) }
        if tab_id == tab && pane_id == pane && s == session)
}

fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|t| (t.title.clone(), t.detail.clone()))
            .collect()
    })
}

fn owned(toast: (&str, Option<&str>)) -> (String, Option<String>) {
    (toast.0.to_owned(), toast.1.map(str::to_owned))
}

fn toast_ids(h: &mut Harness<'_>) -> Vec<u64> {
    h.root(|root, _| root.toasts().iter().map(|t| t.id).collect())
}

fn painted(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

fn active_tab(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.active_tab_id().map(str::to_owned))
}

fn focused_pane(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.focused_pane())
}

fn notice_focused(h: &mut Harness<'_>) -> bool {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).notice_focused(window))
}

fn action_failed(h: &mut Harness<'_>) -> Option<(String, String, Option<String>)> {
    h.root(|root, _| {
        root.action_failed()
            .map(|n| (n.title.clone(), n.detail.clone(), n.hint.clone()))
    })
}

fn checkout_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    h.root(|root, _| root.checkout_focus())
}

fn failed(title: &str, request_id: Option<&str>) -> DaemonMessage {
    DaemonMessage::ActionFailed {
        title: title.to_owned(),
        detail: "worktree in use by\nsession s7".to_owned(),
        hint: Some("Stop it first.".to_owned()),
        request_id: request_id.map(str::to_owned),
    }
}

fn dirty() -> DaemonMessage {
    DaemonMessage::CheckoutConfirmRequired {
        repo_id: "r1".to_owned(),
        branch: "feature".to_owned(),
        dirty_count: 2,
    }
}

/// `s1` alone in pane `p1` of tab `t1`, its scrollback in and nothing sent.
fn single<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    h.sent();
    h
}

#[gpui::test]
fn spawn_sends_request_with_request_id_and_shows_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir);

    let root = h.root.clone();
    h.cx.update(|_, cx| {
        root.update(cx, |root, cx| root.spawn(request(true), OpenIn::NewTab, cx));
    });
    let sent = spawned(&h.sent());
    let first = sent.request_id.clone().expect("an id");
    assert!(!first.is_empty());
    assert_eq!(
        sent.target,
        request(true).target,
        "the request goes as given"
    );
    assert_eq!(toasts(&mut h), [owned(SPAWNING)]);
    let id = toast_ids(&mut h)[0];
    assert!(painted(&mut h, &format!("toast-{id}")));

    let second = spawn(&mut h, request(true), OpenIn::NewTab);
    assert_ne!(first, second, "each spawn has its own id");
    assert_eq!(toasts(&mut h).len(), 2);
}

#[gpui::test]
fn spawn_reply_is_placed_and_focused_in_current_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", None),
    );
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    assert_eq!(focused_pane(&mut h).as_deref(), Some("p1"));

    let id = spawn(&mut h, request(true), OpenIn::CurrentTab("t1".to_owned()));
    h.send(updated(session("s2").build(), Some(&id)));
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [m] if is_replace(m, "t1", "p2", "s2")),
        "sent {placed:?}"
    );
    assert_eq!(focused_pane(&mut h).as_deref(), Some("p2"));

    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s2")),
    );
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &grid),
    });
    h.answer_scrollback("s2", b"");
    h.keys("a");
    assert_eq!(h.sent_input("s2"), b"a", "the new pane has the keyboard");
}

#[gpui::test]
fn spawn_into_other_tab_activates_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", None)),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    assert_eq!(active_tab(&mut h).as_deref(), Some("t1"));

    let id = spawn(&mut h, request(true), OpenIn::Tab("t2".to_owned()));
    assert_eq!(
        active_tab(&mut h).as_deref(),
        Some("t1"),
        "nothing moves until the reply"
    );
    h.send(updated(session("s2").build(), Some(&id)));
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [m] if is_replace(m, "t2", "p2", "s2")),
        "sent {placed:?}"
    );
    assert_eq!(active_tab(&mut h).as_deref(), Some("t2"));
    assert_eq!(focused_pane(&mut h).as_deref(), Some("p2"));
}

#[gpui::test]
fn spawn_into_new_tab_creates_and_focuses_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir);

    let id = spawn(&mut h, request(true), OpenIn::NewTab);
    h.send(updated(session("s2").build(), Some(&id)));
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [ClientMessage::CreateTab { name: None, initial_session_id: Some(s) }] if s == "s2"),
        "sent {placed:?}"
    );

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t9", &pane("p9", Some("s2"))),
    });
    assert_eq!(active_tab(&mut h).as_deref(), Some("t9"));
    assert_eq!(focused_pane(&mut h).as_deref(), Some("p9"));
    h.answer_scrollback("s2", b"");
    h.keys("a");
    assert_eq!(
        h.sent_input("s2"),
        b"a",
        "the new tab's pane has the keyboard"
    );
}

#[gpui::test]
fn broadcast_session_without_request_id_is_not_placed_as_spawn(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir);

    let id = spawn(&mut h, request(true), OpenIn::NewTab);
    h.send(updated(session("s2").build(), None));
    h.send(updated(session("s3").build(), Some("another-client")));
    assert!(placements(h.sent()).is_empty(), "no reply of ours yet");

    h.send(updated(session("s2").build(), Some(&id)));
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [ClientMessage::CreateTab { initial_session_id: Some(s), .. }] if s == "s2"),
        "the spawn still waited for its own reply: sent {placed:?}"
    );
}

#[gpui::test]
fn error_shows_daemon_error_toast_that_expires(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir);

    h.send(DaemonMessage::Error {
        message: "boom".to_owned(),
        request_id: None,
    });
    assert_eq!(toasts(&mut h), [owned(("Daemon error", Some("boom")))]);
    let id = toast_ids(&mut h)[0];
    assert!(painted(&mut h, &format!("toast-{id}")));
    h.advance(
        TOAST_LIFETIME
            .checked_sub(Duration::from_millis(100))
            .expect("a toast lives over 100 ms"),
    );
    assert_eq!(toasts(&mut h).len(), 1, "still shown just before its time");
    h.advance(Duration::from_millis(100));
    assert!(toasts(&mut h).is_empty(), "gone after 8 s");

    let spawn_id = spawn(&mut h, request(true), OpenIn::NewTab);
    h.send(DaemonMessage::Error {
        message: "no such repo".to_owned(),
        request_id: Some(spawn_id.clone()),
    });
    assert_eq!(
        toasts(&mut h),
        [
            owned(SPAWNING),
            owned(("Daemon error", Some("no such repo")))
        ]
    );
    h.send(updated(session("s2").build(), Some(&spawn_id)));
    assert!(
        placements(h.sent()).is_empty(),
        "the failed spawn is forgotten"
    );

    let error_id = toast_ids(&mut h)[1];
    h.click_on(&format!("toast-close-{error_id}"));
    assert_eq!(toasts(&mut h), [owned(SPAWNING)], "× dismisses its toast");

    for message in ["a", "b", "c"] {
        h.send(DaemonMessage::Error {
            message: message.to_owned(),
            request_id: None,
        });
    }
    let details: Vec<Option<String>> = toasts(&mut h).into_iter().map(|t| t.1).collect();
    assert_eq!(
        details,
        [Some("a"), Some("b"), Some("c")].map(|d| d.map(str::to_owned)),
        "at most three: the oldest goes"
    );
}

#[gpui::test]
fn action_failed_shows_modal_and_ok_closes_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir);

    let id = spawn(&mut h, request(true), OpenIn::NewTab);
    h.send(failed("Worktree in use", Some(&id)));
    assert_eq!(
        action_failed(&mut h),
        Some((
            "Worktree in use".to_owned(),
            "worktree in use by\nsession s7".to_owned(),
            Some("Stop it first.".to_owned())
        ))
    );
    assert!(painted(&mut h, "action-failed-modal"));
    assert!(notice_focused(&mut h), "the notice takes the keyboard");
    h.keys("a");
    assert!(h.sent_input("s1").is_empty(), "keys stay with the notice");
    h.send(updated(session("s2").build(), Some(&id)));
    assert!(
        placements(h.sent()).is_empty(),
        "the failed spawn is forgotten"
    );

    h.send(failed("Second", None));
    assert_eq!(
        action_failed(&mut h).map(|n| n.0).as_deref(),
        Some("Second"),
        "a new notice replaces the shown one"
    );
    h.keys("enter");
    assert_eq!(action_failed(&mut h), None, "Enter presses OK");
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the pane has the keyboard back");

    h.send(failed("Third", None));
    h.click_on("action-failed-dismiss");
    assert_eq!(action_failed(&mut h), None, "a click on OK closes it");

    h.send(failed("Fourth", None));
    h.keys("escape");
    assert_eq!(action_failed(&mut h), None, "Esc closes it");
}

#[gpui::test]
fn checkout_confirm_stash_resends_with_same_request_id(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir);

    let id = spawn(&mut h, request(false), OpenIn::NewTab);
    h.send(dirty());
    assert!(painted(&mut h, "checkout-confirm"));
    assert!(notice_focused(&mut h));
    assert_eq!(
        checkout_focus(&mut h),
        Some("checkout-cancel"),
        "safe button first"
    );
    let message = h.root(|root, _| {
        root.checkout_prompt()
            .map(rustling_tulip_native::CheckoutPrompt::message)
    });
    assert_eq!(
        message.as_deref(),
        Some(
            "This repo has 2 uncommitted changes and isn't on feature. Switching in place changes \
             what's checked out in your working directory."
        )
    );
    h.keys("tab");
    assert_eq!(checkout_focus(&mut h), Some("checkout-stash"));
    h.keys("enter");
    let retry = spawned(&h.sent());
    assert_eq!(
        retry.request_id.as_deref(),
        Some(id.as_str()),
        "same request id"
    );
    assert!(matches!(
        retry.target,
        SpawnTarget::Single {
            checkout_strategy: Some(CheckoutStrategy::Stash),
            use_worktree: false,
            ..
        }
    ));
    assert_eq!(checkout_focus(&mut h), None, "the prompt closed");
    assert_eq!(toasts(&mut h), [owned(SPAWNING), owned(SPAWNING)]);

    h.send(updated(session("s2").build(), Some(&id)));
    assert_eq!(placements(h.sent()).len(), 1, "the retry is placed");

    let carry_id = spawn(&mut h, request(false), OpenIn::NewTab);
    h.send(dirty());
    h.click_on("checkout-carry");
    let retry = spawned(&h.sent());
    assert_eq!(retry.request_id.as_deref(), Some(carry_id.as_str()));
    assert!(matches!(
        retry.target,
        SpawnTarget::Single {
            checkout_strategy: Some(CheckoutStrategy::Carry),
            ..
        }
    ));
}

/// An in-place spawn of `branch` in `repo`.
fn in_place(repo: &str, branch: &str) -> SpawnRequest {
    let mut request = request(false);
    if let SpawnTarget::Single {
        repo_id,
        branch_name,
        ..
    } = &mut request.target
    {
        repo.clone_into(repo_id);
        branch.clone_into(branch_name);
    }
    request
}

fn dirty_at(repo: &str, branch: &str) -> DaemonMessage {
    DaemonMessage::CheckoutConfirmRequired {
        repo_id: repo.to_owned(),
        branch: branch.to_owned(),
        dirty_count: 1,
    }
}

#[gpui::test]
fn checkout_confirm_with_two_spawns_resends_the_named_one(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir);

    let a = spawn(&mut h, in_place("R1", "x"), OpenIn::NewTab);
    let b = spawn(&mut h, in_place("R2", "y"), OpenIn::NewTab);
    h.send(dirty_at("R1", "x"));
    h.click_on("checkout-stash");
    let retry = spawned(&h.sent());
    assert_eq!(retry.request_id.as_deref(), Some(a.as_str()), "A is resent");
    assert!(matches!(
        &retry.target,
        SpawnTarget::Single { repo_id, branch_name, checkout_strategy: Some(CheckoutStrategy::Stash), .. }
            if repo_id == "R1" && branch_name == "x"
    ));

    h.send(dirty_at("R2", "y"));
    h.keys("escape");
    assert!(h.sent().is_empty(), "Cancel sends nothing");
    h.send(updated(session("s2").build(), Some(&a)));
    assert_eq!(placements(h.sent()).len(), 1, "A still lands");
    h.send(updated(session("s3").build(), Some(&b)));
    assert!(placements(h.sent()).is_empty(), "Cancel forgot B only");

    h.send(dirty_at("R9", "z"));
    assert!(
        checkout_focus(&mut h).is_some(),
        "an unmatched prompt still shows"
    );
    h.click_on("checkout-carry");
    assert_eq!(checkout_focus(&mut h), None);
    assert!(h.sent().is_empty(), "an unmatched prompt resends nothing");
}

#[gpui::test]
fn checkout_confirm_cancel_forgets_the_spawn(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir);

    let id = spawn(&mut h, request(false), OpenIn::NewTab);
    h.send(dirty());
    h.keys("escape");
    assert_eq!(checkout_focus(&mut h), None, "Esc cancels");
    assert!(h.sent().is_empty(), "Cancel sends nothing");
    h.send(updated(session("s2").build(), Some(&id)));
    assert!(placements(h.sent()).is_empty(), "the spawn is forgotten");

    let id = spawn(&mut h, request(false), OpenIn::NewTab);
    h.send(dirty());
    h.click_on("checkout-cancel");
    assert_eq!(checkout_focus(&mut h), None);
    assert!(h.sent().is_empty());
    h.send(updated(session("s3").build(), Some(&id)));
    assert!(placements(h.sent()).is_empty());
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the pane has the keyboard back");
}

#[gpui::test]
fn modals_close_on_reconnect(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir);

    let id = spawn(&mut h, request(false), OpenIn::NewTab);
    h.send(dirty());
    h.send(failed("Worktree in use", None));
    assert!(action_failed(&mut h).is_some());
    assert!(checkout_focus(&mut h).is_some());
    h.send(DaemonMessage::Welcome {
        protocol_version: 1,
        supported_versions: vec![1],
    });
    assert_eq!(action_failed(&mut h), None);
    assert_eq!(checkout_focus(&mut h), None);
    h.send(updated(session("s2").build(), Some(&id)));
    assert!(
        placements(h.sent()).is_empty(),
        "a new connection forgets the spawns"
    );

    h.send(dirty());
    h.send(failed("Again", None));
    h.lose_connection();
    assert_eq!(action_failed(&mut h), None, "a lost connection closes them");
    assert_eq!(checkout_focus(&mut h), None);
}

#[gpui::test]
fn terminal_does_not_take_focus_while_modal_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir);
    let two = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s1")),
    );

    h.send(failed("Worktree in use", None));
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &two),
    });
    assert!(
        notice_focused(&mut h),
        "a new pane's focus request leaves the keyboard with the notice"
    );
    h.keys("enter");
    assert!(!notice_focused(&mut h));

    h.send(dirty());
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p1", Some("s1"))),
    });
    assert!(notice_focused(&mut h), "the checkout prompt keeps it too");
    h.keys("escape");
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a");
}
