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
use protocol::{
    ClientMessage, DEVICE_NOT_APPROVED, DaemonMessage, FEATURE_RESUME_SESSION, PromptChoice,
    SessionSnapshot,
};
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::target::{HostKey, SshHost, Target};
use rustling_tulip_native::{Connection, NetCommand, NetEvent};
use serde_json::json;
use std::time::Duration;
use support::{
    Harness, REMOTE, TestDir, daemon_features, repo, saved_remote, saved_ui, seed_ui, session,
};

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
fn a_tap_shows_the_session_page_and_loads_its_chat(cx: &mut TestAppContext) {
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
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::LoadChat { session_id, .. }] if session_id == "b"
        ),
        "a tap sends only its chat's load: {sent:?}"
    );
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
    let sent: Vec<ClientMessage> = h.host(&saved_remote()).sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::UnloadChat { session_id }] if session_id == "a"
        ),
        "Back sends only its chat's unload: {sent:?}"
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

/// A single single-choice question with `options`, each described by the
/// entry at its place in `descriptions`.
fn question_described(options: &[&str], descriptions: &[&str]) -> serde_json::Value {
    json!({
        "kind": "question",
        "questions": [{
            "question": "Which branch?",
            "header": "Migration",
            "options": options,
            "option_descriptions": descriptions,
            "multi_select": false,
        }],
    })
}

/// Two questions, the second one taking more than one choice.
fn two_questions() -> serde_json::Value {
    json!({
        "kind": "question",
        "questions": [
            {
                "question": "Keep the newest copy?",
                "header": "Migration",
                "options": ["Keep newest", "Merge both"],
                "multi_select": false,
            },
            {
                "question": "Which checks?",
                "header": "Checks",
                "options": ["fmt", "test"],
                "multi_select": true,
            },
        ],
    })
}

/// A single question of three options whose descriptions each run to three
/// lines, taller than the card's room.
fn long_question() -> serde_json::Value {
    let description = "keep the newest copy and drop the older one from the migration's history, leaving \
                       the branch pointer where it is";
    question_described(
        &["Keep newest", "Merge both", "Stop"],
        &[description, description, description],
    )
}

/// Three questions of three options each: a card the tray cannot answer,
/// taller than its body's room.
fn three_questions() -> serde_json::Value {
    json!({
        "kind": "question",
        "questions": [
            {
                "question": "Which release does the phone app ride?",
                "header": "Release",
                "options": ["vNext", "Hold"],
                "multi_select": false,
            },
            {
                "question": "Which checks gate it?",
                "header": "Checks",
                "options": ["fmt and clippy", "the full suite"],
                "multi_select": true,
            },
            {
                "question": "Who signs off?",
                "header": "Sign-off",
                "options": ["the owner", "a reviewer"],
                "multi_select": false,
            },
        ],
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

/// A finger's vertical pan up over `selector`: `moves` movements of `dy`
/// each, then the lift.
fn pan(h: &mut Harness<'_>, selector: &str, dy: Pixels, moves: usize) {
    let start = h.center(selector);
    h.touch(TouchPhase::Started, start);
    for step in 1..=moves {
        h.touch(TouchPhase::Moved, start - point(px(0.0), dy * step));
    }
    h.touch(TouchPhase::Ended, start - point(px(0.0), dy * moves));
}

/// How far the shown card's first option sits below its body's top.
fn body_line_offset(h: &mut Harness<'_>) -> Pixels {
    h.bounds("phone-tray-option-1-1").top() - h.bounds("phone-tray-body").top()
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
fn open_shows_the_session_and_loads_its_chat(cx: &mut TestAppContext) {
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
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::LoadChat { session_id, .. }] if session_id == "b"
        ),
        "Open sends only its chat's load: {sent:?}"
    );
}

#[gpui::test]
fn a_tray_card_draws_each_answers_description_under_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = waiting_phone(
        cx,
        &dir,
        vec![waiting(
            "b",
            3,
            question_described(&["main", "dev"], &["the trunk", "the feature line"]),
        )],
    );

    for selector in ["phone-tray-choice-1", "phone-tray-choice-2"] {
        assert!(drawn(&mut h, selector), "{selector} draws");
    }
    for (choice, description) in [
        ("phone-tray-choice-1", "phone-tray-choice-desc-1"),
        ("phone-tray-choice-2", "phone-tray-choice-desc-2"),
    ] {
        assert!(drawn(&mut h, description), "{description} draws");
        let choice = h.bounds(choice);
        let description = h.bounds(description);
        assert!(
            description.top() > choice.top() && description.bottom() <= choice.bottom(),
            "{description} runs below its label, inside its choice"
        );
    }
}

#[gpui::test]
fn every_tray_card_offers_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = waiting_phone(
        cx,
        &dir,
        vec![waiting("b", 3, question(&["main", "dev", "next"]))],
    );
    assert!(drawn(&mut h, "phone-tray-choice-1"), "the answers draw");
    assert!(drawn(&mut h, "phone-tray-open"), "beside their Open");
    assert!(
        h.bounds("phone-tray-open").size.height >= touch_target(),
        "Open is a 44 pt target"
    );

    send_waits(&mut h, vec![waiting("b", 3, permission())]);
    assert!(drawn(&mut h, "phone-tray-choice-1"), "Allow draws");
    assert!(drawn(&mut h, "phone-tray-open"), "a permission offers Open");

    h.click_on("phone-tray-open");

    assert_eq!(shown(&mut h).as_deref(), Some("b"));
    assert!(drawn(&mut h, "phone-session"), "the session page shows");
}

#[gpui::test]
fn a_card_with_several_questions_draws_every_question_read_only_with_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = waiting_phone(cx, &dir, vec![waiting("b", 3, two_questions())]);

    for selector in [
        "phone-tray-question-1",
        "phone-tray-question-2",
        "phone-tray-option-2-1",
        "phone-tray-option-2-2",
    ] {
        assert!(drawn(&mut h, selector), "{selector} draws");
    }
    assert!(
        !drawn(&mut h, "phone-tray-choice-1"),
        "no answers a card cannot send"
    );
    assert!(drawn(&mut h, "phone-tray-open"), "with Open");
}

#[gpui::test]
fn a_long_tray_card_keeps_its_height_and_its_open_button(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = waiting_phone(
        cx,
        &dir,
        vec![waiting("b", 5, question(&["main", "dev", "next"]))],
    );
    let short = h.bounds("phone-tray-card").size.height;

    send_waits(&mut h, vec![waiting("b", 5, long_question())]);

    let card = h.bounds("phone-tray-card");
    assert_eq!(card.size.height, short, "the card keeps its height");
    let open = h.bounds("phone-tray-open");
    assert!(
        open.top() >= card.top() && open.bottom() <= card.bottom(),
        "Open stays inside the card"
    );
}

/// A phone whose host lists the long waiting card `b` and 30 idle sessions,
/// so Home's list overflows; the last of those rows' selector comes back
/// too.
fn long_card_over_a_long_list<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
) -> (Harness<'a>, String) {
    let mut h = open_phone(cx, dir);
    let fillers: Vec<String> = (0..30).map(|n| format!("f{n:02}")).collect();
    let mut sessions = vec![waiting("b", 3, three_questions())];
    sessions.extend(
        fillers
            .iter()
            .map(|id| session(id).in_repo("r1").status("idle").build()),
    );
    h.host(&saved_remote())
        .send(DaemonMessage::Sessions { sessions });
    h.cx.run_until_parked();
    let last = row(&saved_remote(), &fillers[29]);
    (h, last)
}

#[gpui::test]
fn a_sideways_swipe_over_a_long_card_leaves_its_body_where_it_was(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = waiting_phone(cx, &dir, vec![waiting("b", 3, three_questions())]);
    let line = h.bounds("phone-tray-option-1-1").top();
    let width = h.bounds("phone-tray-card").size.width;
    assert!(
        h.bounds("phone-tray-option-3-2").bottom() > h.bounds("phone-tray-body").bottom(),
        "the card's body overflows"
    );

    swipe(&mut h, -(width * 0.1), 1);

    assert_eq!(
        h.bounds("phone-tray-option-1-1").top(),
        line,
        "a sideways move left the body where it was"
    );
    assert_eq!(header(&mut h).as_deref(), Some("Needs you · 1 of 1"));
}

#[gpui::test]
fn the_next_card_opens_at_the_top_of_its_body(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let tall = three_questions();
    let mut h = waiting_phone(
        cx,
        &dir,
        vec![waiting("b", 5, tall.clone()), waiting("c", 2, tall)],
    );
    let top = body_line_offset(&mut h);

    pan(&mut h, "phone-tray-body", px(40.0), 2);
    assert!(
        body_line_offset(&mut h) < top,
        "the first card's body scrolled"
    );

    let width = h.bounds("phone-tray-card").size.width;
    swipe(&mut h, -(width * 0.25), 2);

    assert_eq!(header(&mut h).as_deref(), Some("Needs you · 2 of 2"));
    assert_eq!(
        body_line_offset(&mut h),
        top,
        "the second card opens at the top of its own body"
    );
}

#[gpui::test]
fn a_vertical_pan_over_a_long_card_scrolls_only_its_body(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, last) = long_card_over_a_long_list(cx, &dir);
    let line = body_line_offset(&mut h);
    let list = h.bounds(&last).top();
    assert!(drawn(&mut h, "phone-tray-body"), "the card's body draws");
    assert!(
        h.bounds("phone-tray-option-3-2").bottom() > h.bounds("phone-tray-body").bottom(),
        "the card's body overflows"
    );
    assert!(
        h.bounds(&last).bottom() > h.bounds("phone-home").bottom(),
        "the list is taller than its room"
    );

    pan(&mut h, "phone-tray-body", px(40.0), 3);

    assert!(
        body_line_offset(&mut h) < line,
        "the body scrolled under the finger"
    );
    assert_eq!(h.bounds(&last).top(), list, "the list held still");
}

#[gpui::test]
fn a_pan_that_turns_sideways_still_swipes_to_the_next_card(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let tall = three_questions();
    let mut h = waiting_phone(
        cx,
        &dir,
        vec![waiting("b", 5, tall.clone()), waiting("c", 2, tall)],
    );
    let at = h.center("phone-tray-body");
    let width = h.bounds("phone-tray-card").size.width;
    // The event stream a pan leaves: gpui's recogniser locks a real touch to
    // its first axis, so the turn to sideways travel is fed as its events.
    let event = |dx: f32, dy: f32, touch_phase: TouchPhase| ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(dx), px(dy))),
        modifiers: Modifiers::none(),
        touch_phase,
        source: ScrollSource::Touch,
    };

    h.cx.simulate_event(event(0.0, -40.0, TouchPhase::Started));
    h.cx.simulate_event(event(0.0, -40.0, TouchPhase::Moved));
    h.cx.simulate_event(event(-f32::from(width) * 0.5, 0.0, TouchPhase::Moved));
    h.cx.simulate_event(event(0.0, 0.0, TouchPhase::Ended));
    h.cx.run_until_parked();

    assert_eq!(
        header(&mut h).as_deref(),
        Some("Needs you · 2 of 2"),
        "the swipe still moves the tray on"
    );
}

#[gpui::test]
fn a_pan_past_the_bodys_end_scrolls_the_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, last) = long_card_over_a_long_list(cx, &dir);
    let list = h.bounds(&last).top();

    pan(&mut h, "phone-tray-body", px(40.0), 12);

    assert!(
        h.bounds(&last).top() < list,
        "the list took the move the body could not"
    );
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

/// The id of the session whose sheet is open.
fn sheet(h: &mut Harness<'_>) -> Option<String> {
    h.cx.update(|window, cx| {
        window
            .root::<PhoneRoot>()
            .flatten()
            .and_then(|phone| phone.read(cx).sheet_session().map(str::to_owned))
    })
}

/// Why the open sheet's last resume was refused.
fn sheet_refusal(h: &mut Harness<'_>) -> Option<String> {
    h.cx.update(|window, cx| {
        window
            .root::<PhoneRoot>()
            .flatten()
            .and_then(|phone| phone.read(cx).sheet_refusal().map(str::to_owned))
    })
}

/// A phone whose host lists `resume_session`: `a` working in r1, `b`
/// working and `c` parked in r2; the messages sent so far read.
fn resume_phone<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut features = daemon_features();
    features.push(FEATURE_RESUME_SESSION.to_owned());
    let mut host = h.host(&saved_remote());
    host.connect_keeping_sent_with(features);
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1"), repo("r2", "C:/r2")],
    });
    let mut sessions = listed();
    sessions[2] = session("c")
        .label("Cleanup")
        .in_repo("r2")
        .inactive()
        .build();
    host.send(DaemonMessage::Sessions { sessions });
    host.sent();
    h
}

/// The parked `c`'s sheet opened and Resume tapped: the request id the
/// `ResumeSession` carried.
fn resume_c(h: &mut Harness<'_>) -> String {
    h.long_press_on(&row(&saved_remote(), "c"));
    assert!(
        drawn(h, "phone-sheet-resume"),
        "a parked session offers Resume"
    );
    assert!(!drawn(h, "phone-sheet-stop"), "and no Stop");
    h.click_on("phone-sheet-resume");
    let sent = h.host(&saved_remote()).sent();
    assert_eq!(sent.len(), 1, "one message, nothing else: {sent:?}");
    sent.into_iter()
        .find_map(|msg| match msg {
            ClientMessage::ResumeSession {
                session_id,
                request_id,
            } if session_id == "c" => request_id,
            _ => None,
        })
        .expect("c's ResumeSession with a request id")
}

#[gpui::test]
fn a_long_press_opens_the_sheet_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    h.host(&saved_remote()).sent();
    let at = h.center(&row(&saved_remote(), "a"));

    h.touch(TouchPhase::Started, at);
    h.advance(Duration::from_millis(501));
    assert_eq!(sheet(&mut h).as_deref(), Some("a"), "the hold opens it");
    h.touch(TouchPhase::Ended, at);

    assert_eq!(shown(&mut h), None, "the lift opens no session page");
    for selector in [
        "phone-sheet",
        "phone-sheet-scrim",
        "phone-sheet-title",
        "phone-sheet-stop",
        "phone-sheet-cancel",
    ] {
        assert!(drawn(&mut h, selector), "{selector} is drawn");
    }
    for selector in [
        "phone-sheet-title",
        "phone-sheet-stop",
        "phone-sheet-cancel",
    ] {
        let height = h.bounds(selector).size.height;
        assert!(height >= touch_target(), "{selector} is {height:?} high");
    }
    assert!(
        !drawn(&mut h, "phone-sheet-resume"),
        "a running session: Stop"
    );
    let sent: Vec<ClientMessage> = h.host(&saved_remote()).sent();
    assert!(sent.is_empty(), "a long press sends nothing: {sent:?}");
}

#[gpui::test]
fn a_short_tap_still_opens_the_session_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);

    h.tap_on(&row(&saved_remote(), "b"));

    assert_eq!(shown(&mut h).as_deref(), Some("b"));
    assert_eq!(sheet(&mut h), None, "no sheet");
}

#[gpui::test]
fn a_vertical_pan_still_scrolls_the_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    let mut sessions = listed();
    let fillers: Vec<String> = (0..30).map(|n| format!("f{n:02}")).collect();
    sessions.extend(
        fillers
            .iter()
            .map(|id| session(id).in_repo("r1").status("idle").build()),
    );
    h.host(&saved_remote())
        .send(DaemonMessage::Sessions { sessions });
    h.cx.run_until_parked();
    let target = row(&saved_remote(), "a");
    let before = h.bounds(&target);
    let start = h.center(&target);

    h.touch(TouchPhase::Started, start);
    for step in (1..=6u8).map(f32::from) {
        h.touch(TouchPhase::Moved, start - point(px(0.0), px(20.0 * step)));
    }
    h.touch(TouchPhase::Ended, start - point(px(0.0), px(120.0)));
    h.advance(Duration::from_millis(600));

    assert!(
        h.bounds(&target).top() < before.top(),
        "the list scrolled up under the finger"
    );
    assert_eq!(sheet(&mut h), None, "a pan opens no sheet");
    assert_eq!(shown(&mut h), None, "and no session page");
}

#[gpui::test]
fn two_taps_on_stop_send_one_bare_stop(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    h.host(&saved_remote()).sent();
    h.long_press_on(&row(&saved_remote(), "a"));

    h.click_on("phone-sheet-stop");
    assert!(
        h.host(&saved_remote()).sent().is_empty(),
        "the first tap only arms"
    );
    assert_eq!(sheet(&mut h).as_deref(), Some("a"), "the sheet stays");

    h.click_on("phone-sheet-stop");

    let sent = h.host(&saved_remote()).sent();
    assert!(
        !sent.iter().any(desktop_only),
        "no tab or layout message: {sent:?}"
    );
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::StopSession { session_id, cleanup }]
                if session_id == "a" && cleanup.is_empty()
        ),
        "exactly one bare StopSession of a: {sent:?}"
    );
    assert_eq!(sheet(&mut h), None, "the second tap closes the sheet");
}

#[gpui::test]
fn resume_discards_a_parked_original_once_the_new_session_is_up(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = resume_phone(cx, &dir);
    let request_id = resume_c(&mut h);

    h.host(&saved_remote()).send(DaemonMessage::SessionUpdated {
        session: session("c2")
            .label("Cleanup")
            .in_repo("r2")
            .status("working")
            .build(),
        request_id: Some(request_id),
    });
    h.cx.run_until_parked();

    let sent = h.host(&saved_remote()).sent();
    assert!(
        !sent.iter().any(desktop_only),
        "no tab or layout message: {sent:?}"
    );
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::DiscardSession { session_id, cleanup }]
                if session_id == "c" && cleanup.is_empty()
        ),
        "one DiscardSession of the original: {sent:?}"
    );
    assert_eq!(sheet(&mut h), None, "the sheet closes");
}

#[gpui::test]
fn tapping_the_scrim_dismisses_the_sheet(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    h.host(&saved_remote()).sent();
    h.long_press_on(&row(&saved_remote(), "a"));
    assert_eq!(sheet(&mut h).as_deref(), Some("a"));

    let scrim = h.bounds("phone-sheet-scrim");
    h.click(
        point(scrim.center().x, scrim.top() + px(20.0)),
        Modifiers::none(),
    );

    assert_eq!(sheet(&mut h), None, "the scrim dismisses it");
    assert!(!drawn(&mut h, "phone-sheet"));
    assert_eq!(shown(&mut h), None, "the tap reaches no row");
    assert!(h.host(&saved_remote()).sent().is_empty());
}

#[gpui::test]
fn a_refused_resume_shows_in_the_sheet(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = resume_phone(cx, &dir);
    let request_id = resume_c(&mut h);
    assert!(!drawn(&mut h, "phone-sheet-error"));

    h.host(&saved_remote()).send(DaemonMessage::Error {
        message: "the worktree is gone".to_owned(),
        request_id: Some(request_id),
        code: None,
    });
    h.cx.run_until_parked();

    assert!(drawn(&mut h, "phone-sheet-error"), "the refusal shows");
    assert_eq!(
        sheet_refusal(&mut h).as_deref(),
        Some("the worktree is gone")
    );
    assert!(
        h.host(&saved_remote()).sent().is_empty(),
        "no discard follows a refusal"
    );
}

/// What the open sheet's Resume row reads.
fn resume_label(h: &mut Harness<'_>) -> Option<String> {
    h.cx.update(|window, cx| {
        window
            .root::<PhoneRoot>()
            .flatten()
            .and_then(|phone| phone.read(cx).sheet_resume_label().map(str::to_owned))
    })
}

#[gpui::test]
fn a_reopened_sheet_sends_no_second_resume(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = resume_phone(cx, &dir);
    let request_id = resume_c(&mut h);
    h.click_on("phone-sheet-cancel");
    assert_eq!(sheet(&mut h), None);

    h.long_press_on(&row(&saved_remote(), "c"));
    assert_eq!(
        resume_label(&mut h).as_deref(),
        Some("Resuming…"),
        "the resume out shows"
    );
    h.click_on("phone-sheet-resume");
    let sent = h.host(&saved_remote()).sent();
    assert!(sent.is_empty(), "a resuming row is inert: {sent:?}");

    h.host(&saved_remote()).send(DaemonMessage::SessionUpdated {
        session: session("c2")
            .label("Cleanup")
            .in_repo("r2")
            .status("working")
            .build(),
        request_id: Some(request_id),
    });
    h.cx.run_until_parked();

    let sent = h.host(&saved_remote()).sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::DiscardSession { session_id, .. }] if session_id == "c"
        ),
        "exactly one DiscardSession of the original: {sent:?}"
    );
}

#[gpui::test]
fn a_closed_hosts_sheet_offers_only_cancel(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_host_phone(cx, &dir);
    open_host(&mut h, &saved_remote());
    open_host(&mut h, &den());
    drop_connection(&mut h, &den());
    h.host(&saved_remote()).sent();
    h.host(&den()).sent();

    h.long_press_on(&row(&den(), "a"));

    assert_eq!(sheet(&mut h).as_deref(), Some("a"), "the sheet opens");
    assert!(!drawn(&mut h, "phone-sheet-stop"), "no Stop");
    assert!(!drawn(&mut h, "phone-sheet-resume"), "no Resume");
    assert!(
        drawn(&mut h, "phone-sheet-offline"),
        "the offline line shows"
    );
    assert!(drawn(&mut h, "phone-sheet-cancel"));
    assert!(h.host(&saved_remote()).sent().is_empty());
    assert!(h.host(&den()).sent().is_empty(), "nothing is sent");
}

/// A message only a desktop sends: presence, tabs and pane layout.
fn desktop_only(msg: &ClientMessage) -> bool {
    matches!(
        msg,
        ClientMessage::ReportPresence { .. }
            | ClientMessage::CreateTab { .. }
            | ClientMessage::RearrangeTab { .. }
            | ClientMessage::ListTabs
            | ClientMessage::RehostPanes { .. }
            | ClientMessage::RebindHostSessions { .. }
    )
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

/// The SSH host Home's header specs report on.
const SSH: &str = "me@box";

/// The target [`SSH`] is.
fn ssh_target() -> Target {
    Target::parse_ssh(SSH, None).expect("an SSH host string")
}

/// A connection to `target` its daemon refused, as an SSH host refuses an
/// unknown host key.
fn refused(target: &Target) -> Connection {
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_auth_failed("the host's key is not trusted".to_owned());
    conn
}

/// A connection to `target` that could not be reached, as a host that is
/// switched off leaves it.
fn offline(target: &Target) -> Connection {
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    let _ = conn.on_error("no handshake".to_owned());
    conn
}

/// A phone with [`REMOTE`] open and listing its sessions, and [`SSH`]
/// saved beside it, so Home draws a header per host.
fn phone_with_a_host_open_beside_an_ssh_host<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({
            "saved_hosts": [
                { "name": REMOTE, "port": 8787 },
                { "kind": "ssh", "name": SSH },
            ]
        }),
    );
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    open_host(&mut h, &saved_remote());
    h
}

#[gpui::test]
fn an_unknown_host_key_shows_on_home_while_another_host_is_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone_with_a_host_open_beside_an_ssh_host(cx, &dir);
    let ssh = ssh_target();
    h.host(&ssh).event(NetEvent::HostKeyUnverified {
        fingerprint: "SHA256:abc".to_owned(),
    });
    h.host(&ssh).event(NetEvent::State(refused(&ssh)));

    assert!(drawn(&mut h, "phone-home"), "the open host's Home shows");
    for selector in [
        format!("phone-host-hostkey-{SSH}"),
        format!("phone-host-trust-{SSH}"),
        format!("phone-host-cancel-{SSH}"),
    ] {
        assert!(
            drawn(&mut h, &selector),
            "{selector} shows under the host's header"
        );
    }
}

#[gpui::test]
fn trusting_from_home_saves_the_fingerprint_and_redials(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone_with_a_host_open_beside_an_ssh_host(cx, &dir);
    let ssh = ssh_target();
    h.host(&ssh).event(NetEvent::HostKeyUnverified {
        fingerprint: "SHA256:abc".to_owned(),
    });
    h.host(&ssh).event(NetEvent::State(refused(&ssh)));

    h.click_on(&format!("phone-host-trust-{SSH}"));

    let dialled = h
        .opened_targets()
        .pop()
        .expect("Trust dials the host again");
    assert_eq!(
        dialled.ssh_host().and_then(SshHost::host_fingerprint),
        Some("SHA256:abc"),
        "the redial carries the trusted key"
    );
    assert_eq!(
        saved_ui(&dir)["saved_hosts"][1]["host_fingerprint"],
        "SHA256:abc",
        "the saved record keeps the trusted key"
    );
    assert!(!drawn(&mut h, &format!("phone-host-hostkey-{SSH}")));
}

#[gpui::test]
fn a_stopped_host_on_home_shows_its_reason_and_retry(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone_with_a_host_open_beside_an_ssh_host(cx, &dir);
    let ssh = ssh_target();
    h.host(&ssh).event(NetEvent::State(refused(&ssh)));

    assert!(
        drawn(&mut h, &format!("phone-host-reason-{SSH}")),
        "a stopped host's reason shows under its header"
    );
    assert!(
        drawn(&mut h, &format!("phone-host-retry-{SSH}")),
        "and its Retry"
    );
    let _ = h.host(&ssh).commands();

    h.click_on(&format!("phone-host-retry-{SSH}"));

    let restarts = h
        .host(&ssh)
        .commands()
        .iter()
        .filter(|command| matches!(command, NetCommand::Restart))
        .count();
    assert_eq!(restarts, 1, "Retry restarts the host");
}

#[gpui::test]
fn an_offline_host_on_home_shows_retry_and_redials_now(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone_with_a_host_open_beside_an_ssh_host(cx, &dir);
    let ssh = ssh_target();
    h.host(&ssh).event(NetEvent::State(offline(&ssh)));

    assert!(
        drawn(&mut h, &format!("phone-host-retry-{SSH}")),
        "an offline host offers Retry"
    );
    let _ = h.host(&ssh).commands();

    h.click_on(&format!("phone-host-retry-{SSH}"));

    let restarts = h
        .host(&ssh)
        .commands()
        .iter()
        .filter(|command| matches!(command, NetCommand::Restart))
        .count();
    assert_eq!(restarts, 1, "Retry redials the host now");
}
