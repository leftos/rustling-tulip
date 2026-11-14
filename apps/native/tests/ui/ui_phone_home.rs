//! The phone's Home: every saved host's sessions under their repos, rows
//! that never move and are at least 44 pt tall, a header per host while
//! more than one is listed, and a tap that shows a session page without
//! sending anything; above them the Needs you tray, one card at a time,
//! answered from the card or opened, and swiped between.

use crate::support;

use chrono::{DateTime, TimeDelta, Utc};
use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{
    Bounds, Modifiers, Pixels, ScrollDelta, ScrollEvent, ScrollSource, TestAppContext, TouchPhase,
    point, px,
};
use protocol::{ClientMessage, DEVICE_NOT_APPROVED, DaemonMessage, PromptChoice, SessionSnapshot};
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, NetEvent};
use serde_json::json;
use support::{Harness, REMOTE, TestDir, repo, saved_remote, seed_ui, session};

/// The second tailnet host the two-host specs save.
const DEN: &str = "den.tail1234.ts.net";

/// The sessions each spec host lists, in Home's order.
const IDS: [&str; 3] = ["a", "b", "c"];

/// The least height of a row or a target.
fn touch_target() -> Pixels {
    px(44.0)
}

fn den() -> Target {
    Target::tailnet(DEN, Some(8787))
}

/// The key `target`'s header and rows are tagged with.
fn wire(target: &Target) -> String {
    HostKey::of(target)
        .wire()
        .expect("a tailnet host has a wire key")
        .to_owned()
}

/// The selector of `target`'s row for session `id`.
fn row(target: &Target, id: &str) -> String {
    format!("phone-home-row-{}::{id}", wire(target))
}

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// The id of the session the phone shows in place of Home.
fn shown(h: &mut Harness<'_>) -> Option<String> {
    h.cx.update(|window, cx| {
        window
            .root::<PhoneRoot>()
            .flatten()
            .and_then(|phone| phone.read(cx).shown_session().map(|(_, id)| id.to_owned()))
    })
}

/// Session `c` in r2: idle, or waiting on a question.
fn session_c(asking: bool) -> SessionSnapshot {
    let c = session("c").label("Cleanup").in_repo("r2");
    if !asking {
        return c.status("idle").build();
    }
    c.status("awaiting_input")
        .pending_input(json!({
            "kind": "question",
            "questions": [{
                "question": "keep newest or merge?",
                "header": "Migration",
                "options": [],
                "multi_select": false,
            }],
        }))
        .build()
}

fn listed() -> Vec<SessionSnapshot> {
    vec![
        session("a")
            .label("Alpha")
            .in_repo("r1")
            .status("working")
            .build(),
        session("b")
            .label("Bravo")
            .in_repo("r2")
            .status("working")
            .build(),
        session_c(false),
    ]
}

/// Opens `target` listing repos r1 and r2: `a` working in r1, `b` working
/// and `c` idle in r2.
fn open_host(h: &mut Harness<'_>, target: &Target) {
    let mut host = h.host(target);
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1"), repo("r2", "C:/r2")],
    });
    host.send(DaemonMessage::Sessions { sessions: listed() });
}

/// A phone with the saved [`REMOTE`] open as [`open_host`] lists it.
fn open_phone<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    open_host(&mut h, &saved_remote());
    h
}

/// A phone with [`REMOTE`] and [`DEN`] saved, each holding a token.
fn two_host_phone<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({
            "saved_hosts": [
                { "name": REMOTE, "port": 8787 },
                { "name": DEN, "port": 8787 },
            ]
        }),
    );
    let h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    h.tokens().set(DEN, "t2").expect("seed a token");
    h
}

/// `target`'s connection dropped after it was open.
fn drop_connection(h: &mut Harness<'_>, target: &Target) {
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_socket_open(8787);
    let _ = conn.on_closed("dropped".to_owned());
    h.host(target).event(NetEvent::State(conn));
    h.cx.run_until_parked();
}

fn row_bounds(h: &mut Harness<'_>, targets: &[Target]) -> Vec<Bounds<Pixels>> {
    targets
        .iter()
        .flat_map(|target| IDS.map(|id| row(target, id)))
        .map(|selector| h.bounds(&selector))
        .collect()
}

#[gpui::test]
fn home_draws_every_row_at_least_44_high(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);

    assert!(drawn(&mut h, "phone-home"));
    for id in IDS {
        let selector = row(&saved_remote(), id);
        assert!(drawn(&mut h, &selector), "{selector} is drawn");
        let height = h.bounds(&selector).size.height;
        assert!(height >= touch_target(), "{selector} is {height:?} high");
    }
}

#[gpui::test]
fn rows_do_not_move_when_a_session_starts_needing_you(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    let idle = row_bounds(&mut h, &[saved_remote()]);

    // The first wait raises the tray above the list: the whole list moves
    // down by one amount, every row keeping its order and spacing.
    send_waits(&mut h, vec![waiting("b", 5, question(&["main", "dev"]))]);
    let raised = row_bounds(&mut h, &[saved_remote()]);
    let shift = raised[0].origin.y - idle[0].origin.y;
    assert!(shift > px(0.0), "the tray shows above the rows");
    for (before, after) in idle.iter().zip(&raised) {
        assert_eq!(after.origin.y - before.origin.y, shift, "one shift for all");
        assert_eq!((after.origin.x, after.size), (before.origin.x, before.size));
    }

    // With the tray up, another session starting to wait moves nothing.
    h.host(&saved_remote()).send(DaemonMessage::SessionUpdated {
        session: session_c(true),
        request_id: None,
    });
    h.cx.run_until_parked();

    assert_eq!(
        row_bounds(&mut h, &[saved_remote()]),
        raised,
        "every row keeps its place"
    );
}

#[gpui::test]
fn a_tap_shows_the_session_page_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    h.host(&saved_remote()).sent();

    h.click_on(&row(&saved_remote(), "b"));

    assert_eq!(shown(&mut h).as_deref(), Some("b"));
    assert!(drawn(&mut h, "phone-session"), "the session page shows");
    assert!(drawn(&mut h, "phone-session-label"));
    assert!(drawn(&mut h, "phone-session-back"));
    assert!(
        h.bounds("phone-session-back").size.height >= touch_target(),
        "Back is a 44 pt target"
    );
    assert!(!drawn(&mut h, "phone-home"), "the page replaces Home");
    let sent: Vec<ClientMessage> = h.host(&saved_remote()).sent();
    assert!(sent.is_empty(), "a tap sends nothing: {sent:?}");
}

#[gpui::test]
fn back_returns_to_home(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    h.click_on(&row(&saved_remote(), "a"));
    h.host(&saved_remote()).sent();

    h.click_on("phone-session-back");

    assert_eq!(shown(&mut h), None);
    assert!(drawn(&mut h, "phone-home"), "Home shows again");
    assert!(!drawn(&mut h, "phone-session"));
    assert!(
        h.host(&saved_remote()).sent().is_empty(),
        "Back sends nothing"
    );
}

#[gpui::test]
fn a_shown_session_the_host_removes_returns_to_home(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    h.click_on(&row(&saved_remote(), "c"));
    assert_eq!(shown(&mut h).as_deref(), Some("c"));

    let mut remaining = listed();
    remaining.retain(|s| s.id != "c");
    h.host(&saved_remote()).send(DaemonMessage::Sessions {
        sessions: remaining,
    });
    h.cx.run_until_parked();

    assert_eq!(shown(&mut h), None, "the gone session is no longer shown");
    assert!(drawn(&mut h, "phone-home"), "Home shows again");
    assert!(!drawn(&mut h, &row(&saved_remote(), "c")));
}

#[gpui::test]
fn a_host_refusing_the_token_shows_its_header_with_pair(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_host_phone(cx, &dir);
    open_host(&mut h, &saved_remote());
    let mut refused = Connection::for_target(den());
    refused.on_connecting();
    refused.on_socket_open(8787);
    refused.on_not_approved(DEVICE_NOT_APPROVED.to_owned(), true);
    h.host(&den()).event(NetEvent::State(refused));

    let den_wire = wire(&den());
    assert!(drawn(
        &mut h,
        &format!("phone-home-host-{}", wire(&saved_remote()))
    ));
    assert!(drawn(&mut h, &format!("phone-home-host-{den_wire}")));
    assert!(drawn(&mut h, &format!("phone-home-host-status-{den_wire}")));
    let pair = format!("phone-home-host-pair-{den_wire}");
    assert!(drawn(&mut h, &pair), "the refusing host offers Pair");
    assert!(
        h.bounds(&pair).size.height >= touch_target(),
        "Pair is a 44 pt target"
    );
    assert!(!h.root(|root, _| root.device_pairing_open()));

    h.click_on(&pair);

    assert!(
        h.root(|root, _| root.device_pairing_open()),
        "Pair opens the pairing flow"
    );
}

#[gpui::test]
fn rows_do_not_move_when_a_host_disconnects(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_host_phone(cx, &dir);
    open_host(&mut h, &saved_remote());
    open_host(&mut h, &den());
    let hosts = [saved_remote(), den()];
    let before = row_bounds(&mut h, &hosts);
    let status = format!("phone-home-host-status-{}", wire(&den()));
    assert!(
        !drawn(&mut h, &status),
        "an open host's header says nothing"
    );

    drop_connection(&mut h, &den());

    assert_eq!(
        row_bounds(&mut h, &hosts),
        before,
        "both hosts' rows keep their place"
    );
    assert!(drawn(&mut h, &status), "the dropped host's header says so");
}

#[gpui::test]
fn the_session_page_stays_while_its_host_disconnects(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_host_phone(cx, &dir);
    open_host(&mut h, &saved_remote());
    open_host(&mut h, &den());
    h.click_on(&row(&den(), "b"));
    assert_eq!(shown(&mut h).as_deref(), Some("b"));

    drop_connection(&mut h, &den());

    assert_eq!(shown(&mut h).as_deref(), Some("b"), "the page stays");
    assert!(drawn(&mut h, "phone-session"));
    assert!(drawn(&mut h, "phone-session-label"), "with its label");

    h.click_on("phone-session-back");

    assert_eq!(shown(&mut h), None);
    assert!(drawn(&mut h, "phone-home"), "Back returns to Home");
}

/// `minutes` ago.
fn minutes_ago(minutes: i64) -> DateTime<Utc> {
    Utc::now() - TimeDelta::minutes(minutes)
}

/// A single single-choice question with `options`.
fn question(options: &[&str]) -> serde_json::Value {
    json!({
        "kind": "question",
        "questions": [{
            "question": "Which branch?",
            "header": "",
            "options": options,
            "multi_select": false,
        }],
    })
}

fn permission() -> serde_json::Value {
    json!({ "kind": "permission", "tool_name": "Bash", "summary": "gh pr merge 118 --squash" })
}

/// [`listed`] with `id` waiting since `minutes` ago on `pending`.
fn waiting(id: &str, minutes: i64, pending: serde_json::Value) -> SessionSnapshot {
    let label = match id {
        "a" => "Alpha",
        "b" => "Bravo",
        _ => "Cleanup",
    };
    let repo = if id == "a" { "r1" } else { "r2" };
    session(id)
        .label(label)
        .in_repo(repo)
        .status("awaiting_input")
        .status_since(minutes_ago(minutes))
        .pending_input(pending)
        .build()
}

/// The phone's host lists [`listed`] with `waits` in place of their ids.
fn send_waits(h: &mut Harness<'_>, mut waits: Vec<SessionSnapshot>) {
    let sessions = listed()
        .into_iter()
        .map(|s| match waits.iter().position(|w| w.id == s.id) {
            Some(at) => waits.swap_remove(at),
            None => s,
        })
        .collect();
    h.host(&saved_remote())
        .send(DaemonMessage::Sessions { sessions });
    h.cx.run_until_parked();
}

/// A phone whose host has `waits` waiting, the messages sent so far read.
fn waiting_phone<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    waits: Vec<SessionSnapshot>,
) -> Harness<'a> {
    let mut h = open_phone(cx, dir);
    send_waits(&mut h, waits);
    h.host(&saved_remote()).sent();
    h
}

/// The tray header's text, as Home draws it now.
fn header(h: &mut Harness<'_>) -> Option<String> {
    h.cx.run_until_parked();
    h.cx.update(|window, cx| {
        window
            .root::<PhoneRoot>()
            .flatten()
            .and_then(|phone| phone.read(cx).tray_header(cx))
    })
}

/// Why the shown card's last answer was refused, while it says so.
fn refusal(h: &mut Harness<'_>) -> Option<String> {
    h.cx.update(|window, cx| {
        window
            .root::<PhoneRoot>()
            .flatten()
            .and_then(|phone| phone.read(cx).tray_refusal(cx))
    })
}

/// The `AnswerPrompt`s and every other message the host was sent since
/// the last look.
fn answers(h: &mut Harness<'_>) -> (Vec<(String, PromptChoice)>, Vec<ClientMessage>) {
    let mut answers = Vec::new();
    let mut others = Vec::new();
    for msg in h.host(&saved_remote()).sent() {
        match msg {
            ClientMessage::AnswerPrompt {
                session_id, choice, ..
            } => answers.push((session_id, choice)),
            other => others.push(other),
        }
    }
    (answers, others)
}

/// A quick flick over the tray's card: `dx` in two movements 10 ms
/// apart, then the lift.
fn flick(h: &mut Harness<'_>, dx: Pixels) {
    let at = h.center("phone-tray-card");
    let pan = |delta: Pixels, touch_phase: TouchPhase| ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(delta, px(0.0))),
        modifiers: Modifiers::none(),
        touch_phase,
        source: ScrollSource::Touch,
    };
    let gap = std::time::Duration::from_millis(10);
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Started));
    h.advance(gap);
    h.cx.simulate_event(pan(dx * 0.5, TouchPhase::Moved));
    h.advance(gap);
    h.cx.simulate_event(pan(dx * 0.5, TouchPhase::Moved));
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Ended));
    h.cx.run_until_parked();
}

#[gpui::test]
fn a_sideways_swipe_does_not_scroll_the_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    let pending = question(&["main", "dev"]);
    let mut sessions = vec![
        session("a")
            .label("Alpha")
            .in_repo("r1")
            .status("working")
            .build(),
        waiting("b", 5, pending.clone()),
        waiting("c", 2, pending),
    ];
    let fillers: Vec<String> = (0..30).map(|n| format!("f{n:02}")).collect();
    sessions.extend(
        fillers
            .iter()
            .map(|id| session(id).in_repo("r1").status("idle").build()),
    );
    h.host(&saved_remote())
        .send(DaemonMessage::Sessions { sessions });
    h.cx.run_until_parked();
    let last = row(&saved_remote(), &fillers[29]);
    assert!(
        h.bounds(&last).bottom() > h.bounds("phone-home").bottom(),
        "the list is taller than its room"
    );
    let rows: Vec<String> = ["a", "b", "c", "f00", "f01"]
        .iter()
        .map(|id| row(&saved_remote(), id))
        .collect();
    let before: Vec<Bounds<Pixels>> = rows.iter().map(|r| h.bounds(r)).collect();

    flick(&mut h, px(-120.0));

    let after: Vec<Bounds<Pixels>> = rows.iter().map(|r| h.bounds(r)).collect();
    assert_eq!(after, before, "the list did not scroll");
    assert_eq!(header(&mut h).as_deref(), Some("Needs you · 2 of 2"));
}

#[gpui::test]
fn a_closed_host_gives_no_card(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_host_phone(cx, &dir);
    open_host(&mut h, &saved_remote());
    open_host(&mut h, &den());
    h.host(&den()).send(DaemonMessage::Sessions {
        sessions: vec![waiting("c", 9, question(&["a", "b", "c", "d"]))],
    });
    drop_connection(&mut h, &den());
    send_waits(
        &mut h,
        vec![waiting("b", 3, question(&["a", "b", "c", "d"]))],
    );

    assert_eq!(header(&mut h).as_deref(), Some("Needs you · 1 of 1"));
    h.click_on("phone-tray-open");
    let host = h.cx.update(|window, cx| {
        window
            .root::<PhoneRoot>()
            .flatten()
            .and_then(|phone| phone.read(cx).shown_session().map(|(host, _)| host.clone()))
    });
    assert_eq!(
        host,
        Some(HostKey::of(&saved_remote())),
        "the open host's card"
    );
}

/// A pan over the tray's card, `steps` movements of `dx`, then the lift.
fn swipe(h: &mut Harness<'_>, dx: Pixels, steps: usize) {
    let at = h.center("phone-tray-card");
    let pan = |delta: Pixels, touch_phase: TouchPhase| ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(delta, px(0.0))),
        modifiers: Modifiers::none(),
        touch_phase,
        source: ScrollSource::Touch,
    };
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Started));
    for _ in 0..steps {
        h.cx.simulate_event(pan(dx, TouchPhase::Moved));
    }
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Ended));
    h.cx.run_until_parked();
}

#[gpui::test]
fn the_tray_shows_one_card_while_sessions_wait(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let options = ["main", "dev", "next"];
    let mut h = waiting_phone(
        cx,
        &dir,
        vec![
            waiting("b", 5, question(&options)),
            waiting("c", 2, question(&options)),
        ],
    );

    assert!(drawn(&mut h, "phone-tray"), "the tray draws");
    assert_eq!(header(&mut h).as_deref(), Some("Needs you · 1 of 2"));
    for selector in [
        "phone-tray-card",
        "phone-tray-choice-1",
        "phone-tray-choice-2",
        "phone-tray-choice-3",
    ] {
        let height = h.bounds(selector).size.height;
        assert!(height >= touch_target(), "{selector} is {height:?} high");
    }
    let tray = h.bounds("phone-tray");
    let first_row = h.bounds(&row(&saved_remote(), "a"));
    assert!(
        tray.bottom() <= first_row.top(),
        "the tray is above the list"
    );

    send_waits(&mut h, Vec::new());
    assert!(!drawn(&mut h, "phone-tray"), "no wait, no tray");
}

#[gpui::test]
fn tapping_an_option_answers_it_from_the_card(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = waiting_phone(
        cx,
        &dir,
        vec![waiting("b", 3, question(&["main", "dev", "next"]))],
    );

    h.click_on("phone-tray-choice-2");

    let (answers, others) = answers(&mut h);
    assert_eq!(
        answers,
        [("b".to_owned(), PromptChoice::Option { index: 1 })],
        "the second option"
    );
    assert!(others.is_empty(), "no tab or layout message: {others:?}");
}

#[gpui::test]
fn allow_answers_a_permission(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = waiting_phone(cx, &dir, vec![waiting("b", 3, permission())]);
    assert!(drawn(&mut h, "phone-tray-command"), "the command shows");
    assert!(!drawn(&mut h, "phone-tray-choice-3"), "Allow and Deny only");

    h.click_on("phone-tray-choice-1");

    let (answers, others) = answers(&mut h);
    assert_eq!(answers, [("b".to_owned(), PromptChoice::Allow)]);
    assert!(others.is_empty(), "no tab or layout message: {others:?}");
}

#[gpui::test]
fn open_shows_the_session_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = waiting_phone(
        cx,
        &dir,
        vec![waiting("b", 3, question(&["a", "b", "c", "d"]))],
    );
    assert!(!drawn(&mut h, "phone-tray-choice-1"), "four options: Open");
    assert!(h.bounds("phone-tray-open").size.height >= touch_target());

    h.click_on("phone-tray-open");

    assert_eq!(shown(&mut h).as_deref(), Some("b"));
    assert!(drawn(&mut h, "phone-session"), "the session page shows");
    assert!(!drawn(&mut h, "phone-tray"), "the tray is Home's");
    let sent: Vec<ClientMessage> = h.host(&saved_remote()).sent();
    assert!(sent.is_empty(), "Open sends nothing: {sent:?}");
}

#[gpui::test]
fn a_refused_answer_shows_on_its_card(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = waiting_phone(cx, &dir, vec![waiting("b", 3, permission())]);
    h.click_on("phone-tray-choice-1");
    let request_id = h
        .host(&saved_remote())
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::AnswerPrompt { request_id, .. } => request_id,
            _ => None,
        })
        .expect("the answer carries a request id");
    assert!(!drawn(&mut h, "phone-tray-error"));

    h.host(&saved_remote()).send(DaemonMessage::Error {
        message: "the prompt changed".to_owned(),
        request_id: Some(request_id),
        code: None,
    });
    h.cx.run_until_parked();

    assert!(drawn(&mut h, "phone-tray-error"), "the refusal shows");
    assert_eq!(refusal(&mut h).as_deref(), Some("the prompt changed"));
}

#[gpui::test]
fn a_swipe_past_a_third_moves_to_the_next_card(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let pending = question(&["main", "dev"]);
    let mut h = waiting_phone(
        cx,
        &dir,
        vec![waiting("b", 5, pending.clone()), waiting("c", 2, pending)],
    );
    let width = h.bounds("phone-tray-card").size.width;

    swipe(&mut h, -(width * 0.1), 1);
    assert_eq!(
        header(&mut h).as_deref(),
        Some("Needs you · 1 of 2"),
        "a short swipe snaps back"
    );

    swipe(&mut h, -(width * 0.25), 2);
    assert_eq!(header(&mut h).as_deref(), Some("Needs you · 2 of 2"));
}

#[gpui::test]
fn rows_do_not_move_when_a_card_leaves(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let pending = question(&["main", "dev"]);
    let mut h = waiting_phone(
        cx,
        &dir,
        vec![waiting("b", 5, pending.clone()), waiting("c", 2, pending)],
    );
    let before = row_bounds(&mut h, &[saved_remote()]);

    send_waits(&mut h, vec![waiting("c", 2, question(&["main", "dev"]))]);

    assert_eq!(header(&mut h).as_deref(), Some("Needs you · 1 of 1"));
    assert_eq!(
        row_bounds(&mut h, &[saved_remote()]),
        before,
        "the rows keep their place"
    );
}
