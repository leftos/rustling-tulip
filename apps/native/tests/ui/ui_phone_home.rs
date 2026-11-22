//! The phone's Home: every saved host's sessions under their repos, rows
//! that never move and are at least 44 pt tall, a header per host while
//! more than one is listed, and a tap that shows a session page without
//! sending anything; above them the Needs you tray, one card at a time,
//! answered from the card or opened, and swiped between.

use crate::support;

use chrono::{DateTime, TimeDelta, Utc};
use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{
    Bounds, Modifiers, Pixels, Point, ScrollDelta, ScrollEvent, ScrollSource, TestAppContext,
    TouchPhase, point, px,
};
use protocol::{
    AttentionReason, ClientMessage, ContainerRef, DEVICE_NOT_APPROVED, DaemonMessage,
    FEATURE_HOST_LAYOUTS, FEATURE_RESUME_SESSION, PromptChoice, SessionSnapshot,
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

/// `target`'s connection dialling, as it is when the phone redials it on
/// coming back to the foreground.
fn dialling(h: &mut Harness<'_>, target: &Target) {
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_socket_open(8787);
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
fn add_a_host_is_a_44_point_target(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-hosts");

    assert!(
        drawn(&mut h, "phone-host-add-another"),
        "Settings' Hosts page draws it"
    );
    let height = h.bounds("phone-host-add-another").size.height;
    assert!(height >= touch_target(), "Add a host is {height:?} high");
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
            [
                ClientMessage::LoadChat { session_id, .. },
                ClientMessage::WatchDashboard { on: true },
            ] if session_id == "b"
        ),
        "a tap sends its chat's load and the context label's Dashboard watch: {sent:?}"
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
            [
                ClientMessage::UnloadChat { session_id },
                ClientMessage::WatchDashboard { on: false },
            ] if session_id == "a"
        ),
        "Back sends its chat's unload and ends the Dashboard watch: {sent:?}"
    );
}

#[gpui::test]
fn back_from_a_session_keeps_homes_scroll_offset(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_long_hosts(cx, &dir);
    scroll_list(&mut h, px(200.0));
    let list = h.bounds("phone-home");
    let id = filler_ids()
        .into_iter()
        .find(|id| {
            let bounds = h.bounds(&row(&saved_remote(), id));
            bounds.size.width > px(0.0)
                && bounds.top() >= list.top()
                && bounds.bottom() <= list.bottom()
        })
        .expect("a filler row sits inside the list after the pan");
    let opened = row(&saved_remote(), &id);
    let scrolled = h.bounds(&opened);
    assert_eq!(
        pinned(&mut h),
        Some(wire(&saved_remote())),
        "the pan scrolled the list past the first host's header"
    );

    h.click_on(&opened);
    assert!(drawn(&mut h, "phone-session"), "a tap shows the session");

    h.click_on("phone-session-back");

    assert!(drawn(&mut h, "phone-home"), "Home shows again");
    assert_eq!(h.bounds(&opened), scrolled, "the row keeps its place");
    assert_eq!(
        pinned(&mut h),
        Some(wire(&saved_remote())),
        "and the first host's header is still pinned"
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
fn home_marks_rows_refreshing_until_the_fresh_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_host_phone(cx, &dir);
    open_host(&mut h, &saved_remote());
    open_host(&mut h, &den());
    let remote_line = format!("phone-home-refreshing-{}", wire(&saved_remote()));
    let den_line = format!("phone-home-refreshing-{}", wire(&den()));
    assert!(
        !drawn(&mut h, &remote_line) && !drawn(&mut h, &den_line),
        "a host that listed since the foreground says nothing"
    );

    h.cx.simulate_visibility_change(gpui::WindowVisibility::Hidden);
    h.cx.simulate_visibility_change(gpui::WindowVisibility::Visible);
    h.cx.run_until_parked();
    assert!(
        drawn(&mut h, &remote_line) && drawn(&mut h, &den_line),
        "the rows are the ones from before the foreground until the hosts list again"
    );

    h.host(&saved_remote())
        .send(DaemonMessage::Sessions { sessions: listed() });
    assert!(
        !drawn(&mut h, &remote_line),
        "the fresh list is the one Home draws"
    );
    assert!(drawn(&mut h, &den_line), "the other host still waits");

    dialling(&mut h, &den());
    assert!(
        drawn(&mut h, &den_line),
        "a host on its way waits for its list too"
    );
}

#[gpui::test]
fn refreshing_rows_stay_put_when_the_host_drops(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_host_phone(cx, &dir);
    open_host(&mut h, &saved_remote());
    open_host(&mut h, &den());
    let den_line = format!("phone-home-refreshing-{}", wire(&den()));
    let hosts = [saved_remote(), den()];

    h.cx.simulate_visibility_change(gpui::WindowVisibility::Hidden);
    h.cx.simulate_visibility_change(gpui::WindowVisibility::Visible);
    h.cx.run_until_parked();
    assert!(drawn(&mut h, &den_line), "both hosts wait for their lists");
    let before = row_bounds(&mut h, &hosts);

    drop_connection(&mut h, &den());

    assert!(
        drawn(&mut h, &den_line),
        "a host that has not listed keeps its line when it drops"
    );
    assert_eq!(
        row_bounds(&mut h, &hosts),
        before,
        "a dropped host's rows stay where they were"
    );
}

#[gpui::test]
fn no_refreshing_line_for_a_host_without_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_host_phone(cx, &dir);
    let remote_line = format!("phone-home-refreshing-{}", wire(&saved_remote()));
    let den_line = format!("phone-home-refreshing-{}", wire(&den()));
    h.host(&saved_remote()).connect();
    h.host(&den()).connect();
    assert!(
        !drawn(&mut h, &remote_line) && !drawn(&mut h, &den_line),
        "a cold launch has listed nothing and says nothing"
    );

    open_host(&mut h, &saved_remote());
    h.cx.simulate_visibility_change(gpui::WindowVisibility::Hidden);
    h.cx.simulate_visibility_change(gpui::WindowVisibility::Visible);
    h.cx.run_until_parked();
    assert!(
        drawn(&mut h, &remote_line),
        "a host with rows waits for its list"
    );
    assert!(
        !drawn(&mut h, &den_line),
        "a host with no sessions says nothing, stale or not"
    );
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
            [
                ClientMessage::LoadChat { session_id, .. },
                ClientMessage::WatchDashboard { on: true },
            ] if session_id == "b"
        ),
        "Open sends its chat's load and the context label's Dashboard watch: {sent:?}"
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
fn a_needs_you_option_stays_inside_its_box(cx: &mut TestAppContext) {
    let label = "Use the newest copy and discard the rest of them";
    let description = "keep the newest copy and drop the older ones from the migration's history, \
                       leaving the branch pointer where it is";
    let dir = TestDir::new();
    let mut h = waiting_phone(
        cx,
        &dir,
        vec![waiting(
            "b",
            3,
            question_described(&[label, "Stop"], &[description, description]),
        )],
    );
    h.cx.simulate_resize(gpui::size(px(1180.0), px(820.0)));
    h.cx.run_until_parked();

    assert!(
        drawn(&mut h, "phone-tray-choice-desc-1"),
        "the description draws"
    );
    let choice = h.bounds("phone-tray-choice-1");
    let inside = h.bounds("phone-tray-choice-desc-1");
    assert!(
        inside.right() <= choice.right() && inside.bottom() <= choice.bottom(),
        "the answer's description {inside:?} stays inside its box {choice:?}"
    );

    send_waits(
        &mut h,
        vec![waiting(
            "b",
            3,
            question_described(
                &[label, "Merge both", "Stop", "Ask later"],
                &[description, description, description, description],
            ),
        )],
    );

    assert!(
        drawn(&mut h, "phone-tray-option-desc-1-1"),
        "the read-only description draws"
    );
    let option = h.bounds("phone-tray-option-1-1");
    let inside = h.bounds("phone-tray-option-desc-1-1");
    assert!(
        inside.right() <= option.right() && inside.bottom() <= option.bottom(),
        "the read-only description {inside:?} stays inside its box {option:?}"
    );
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

#[gpui::test]
fn every_row_draws_a_chevron_and_a_tap_anywhere_on_it_opens_the_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);

    for id in IDS {
        let selector = row(&saved_remote(), id);
        let bounds = h.bounds(&selector);
        let chevron = h.bounds(&format!("{selector}-chevron"));
        assert!(
            chevron.size.width > px(0.0),
            "{selector}'s chevron is drawn"
        );
        assert!(
            chevron.origin.x > bounds.center().x
                && chevron.bottom_right().x <= bounds.bottom_right().x,
            "{selector}'s chevron sits at its right edge"
        );
        let left_edge = point(bounds.origin.x + px(2.0), bounds.center().y);
        for (spot, at) in [
            ("left edge", left_edge),
            ("label", bounds.center()),
            ("chevron", chevron.center()),
        ] {
            h.click(at, Modifiers::none());
            assert_eq!(
                shown(&mut h).as_deref(),
                Some(id),
                "a tap on {id}'s {spot} opens it"
            );
            h.click_on("phone-session-back");
            assert_eq!(shown(&mut h), None);
        }
    }
}

/// Picks `choice` (a sort row's selector tail) in Home's sort sheet.
fn sort_by(h: &mut Harness<'_>, choice: &str) {
    h.click_on("phone-home-sort");
    h.click_on(&format!("phone-home-sort-{choice}"));
}

/// The top of `target`'s row for session `id`.
fn row_top(h: &mut Harness<'_>, target: &Target, id: &str) -> Pixels {
    h.bounds(&row(target, id)).origin.y
}

/// The note `target` shows under its header while it has no desktop
/// layout.
fn layout_note(target: &Target) -> String {
    format!("phone-home-note-{}", wire(target))
}

#[gpui::test]
fn sorting_a_to_z_reorders_the_list_and_saves_the_choice(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    // The owner dragged r2 above r1: Manual keeps that order, A–Z does not.
    h.host(&saved_remote())
        .send(DaemonMessage::ContainersReordered {
            ordered: vec![
                ContainerRef::Repo("r2".to_owned()),
                ContainerRef::Repo("r1".to_owned()),
            ],
        });
    h.cx.run_until_parked();
    let remote = saved_remote();
    assert!(
        row_top(&mut h, &remote, "b") < row_top(&mut h, &remote, "a"),
        "Manual: r2's rows above r1's"
    );

    h.click_on("phone-home-sort");
    assert!(
        drawn(&mut h, "phone-home-sort-sheet"),
        "the sort sheet opens"
    );
    for choice in ["manual", "alphabetical", "desktop-tabs"] {
        let selector = format!("phone-home-sort-{choice}");
        assert!(drawn(&mut h, &selector), "{selector} is drawn");
        let height = h.bounds(&selector).size.height;
        assert!(height >= touch_target(), "{selector} is {height:?} high");
    }
    assert!(
        drawn(&mut h, "phone-home-sort-manual-check"),
        "the current order is checked"
    );
    assert!(!drawn(&mut h, "phone-home-sort-alphabetical-check"));
    h.click_on("phone-home-sort-alphabetical");

    assert!(
        !drawn(&mut h, "phone-home-sort-sheet"),
        "a choice closes the sheet"
    );
    assert!(
        row_top(&mut h, &remote, "a") < row_top(&mut h, &remote, "b"),
        "A–Z puts r1's rows above r2's"
    );
    assert_eq!(
        saved_ui(&dir)["phone_order"],
        "alphabetical",
        "and saves it"
    );
    h.click_on("phone-home-sort");
    assert!(drawn(&mut h, "phone-home-sort-alphabetical-check"));
}

#[gpui::test]
fn the_needs_you_switch_keeps_the_sort_sheet_open_and_saves(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    let remote = saved_remote();
    h.host(&remote).send(DaemonMessage::Sessions {
        sessions: vec![
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
            session_c(true),
        ],
    });
    h.cx.run_until_parked();

    h.click_on("phone-home-sort");
    assert!(
        drawn(&mut h, "phone-home-sort-needs-you"),
        "the switch sits under the orders"
    );
    assert!(
        !drawn(&mut h, "phone-home-sort-needs-you-tick"),
        "off at first"
    );
    h.click_on("phone-home-sort-needs-you");

    assert!(
        drawn(&mut h, "phone-home-sort-sheet"),
        "the switch leaves the sheet open"
    );
    assert!(
        drawn(&mut h, "phone-home-sort-needs-you-tick"),
        "the switch is on"
    );
    assert_eq!(
        saved_ui(&dir)["phone_needs_you_first"],
        true,
        "and saves it"
    );
    let group = format!("phone-home-group-needs-you-{}", wire(&remote));
    let tops = ["c", "a", "b"].map(|id| row_top(&mut h, &remote, id));
    assert!(drawn(&mut h, &group), "a Needs you group heads the host");
    assert!(h.bounds(&group).origin.y < tops[0], "above the waiting row");
    assert!(
        tops[0] < tops[1] && tops[1] < tops[2],
        "the waiting session first, the rest in Home's order"
    );
}

/// Opens `target` as [`open_host`] does, its `Welcome` listing `extra`
/// beside the usual features.
fn open_host_with(h: &mut Harness<'_>, target: &Target, extra: &[&str]) {
    let mut host = h.host(target);
    host.connect_with_features(extra);
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1"), repo("r2", "C:/r2")],
    });
    host.send(DaemonMessage::Sessions { sessions: listed() });
}

/// The host's `HostLayouts` answering `request_id`: its own layout ("box",
/// named after the host) with tab "Build" holding `c` then `a`, and a
/// laptop's layout.
fn host_layouts(request_id: &str) -> DaemonMessage {
    let pane =
        |id: &str, session: &str| json!({ "kind": "pane", "pane_id": id, "session_id": session });
    let tab = |id: &str, name: &str, grid: serde_json::Value| {
        json!({
            "id": id,
            "name": name,
            "content": { "kind": "grid", "grid": grid },
            "created_at": "2026-01-01T00:00:00Z",
        })
    };
    let build = json!({
        "kind": "split",
        "direction": "horizontal",
        "ratio": 0.5,
        "first": pane("p1", "c"),
        "second": pane("p2", "a"),
    });
    serde_json::from_value(json!({
        "type": "host_layouts",
        "host_name": "box",
        "layouts": [
            { "client_id": "desk", "name": "box", "tabs": [tab("t1", "Build", build)] },
            { "client_id": "laptop", "name": "laptop", "tabs": [tab("t9", "Other", pane("p9", "b"))] },
        ],
        "request_id": request_id,
    }))
    .expect("a host layouts reply")
}

#[gpui::test]
fn desktop_tabs_asks_a_host_listing_host_layouts_and_groups_its_rows_by_tab(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = two_host_phone(cx, &dir);
    let (remote, den) = (saved_remote(), den());
    open_host_with(&mut h, &remote, &[FEATURE_HOST_LAYOUTS]);
    open_host(&mut h, &den);
    h.host(&remote).sent();
    h.host(&den).sent();

    sort_by(&mut h, "desktop-tabs");

    let asked: Vec<Option<String>> = h
        .host(&remote)
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ListHostLayouts { request_id } => Some(request_id),
            _ => None,
        })
        .collect();
    assert_eq!(asked.len(), 1, "one ListHostLayouts: {asked:?}");
    let request_id = asked[0].clone().expect("the request carries an id");
    let den_sent = h.host(&den).sent();
    assert!(
        !den_sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::ListHostLayouts { .. })),
        "a host without host_layouts is not asked: {den_sent:?}"
    );
    assert!(
        drawn(&mut h, &layout_note(&den)),
        "a host without host_layouts shows the note"
    );
    assert!(
        drawn(&mut h, &layout_note(&remote)),
        "the asked host shows it until its reply"
    );
    assert_eq!(saved_ui(&dir)["phone_order"], "desktop_tabs");

    h.host(&remote).send(host_layouts(&request_id));
    h.cx.run_until_parked();

    assert!(
        !drawn(&mut h, &layout_note(&remote)),
        "the reply ends the note"
    );
    let tag = wire(&remote);
    let build = format!("phone-home-group-tab-{tag}-t1");
    let rest = format!("phone-home-group-no-tab-{tag}");
    assert!(drawn(&mut h, &build), "the own layout's tab heads its rows");
    assert!(drawn(&mut h, &rest), "the rest sit under Not in a tab");
    let tops = ["c", "a", "b"].map(|id| row_top(&mut h, &remote, id));
    assert!(
        h.bounds(&build).origin.y < tops[0] && tops[0] < tops[1],
        "Build lists c then a, in pane order"
    );
    assert!(
        tops[1] < h.bounds(&rest).origin.y && h.bounds(&rest).origin.y < tops[2],
        "then Not in a tab lists b"
    );
    assert!(
        drawn(&mut h, &layout_note(&den)),
        "the other host keeps its note"
    );
}

/// The selector of the repo or workspace label on `target`'s row for `id`.
fn row_container(target: &Target, id: &str) -> String {
    format!("phone-home-row-container-{}::{id}", wire(target))
}

/// The headings of Home's groups now, in list order.
fn group_names(h: &mut Harness<'_>) -> Vec<String> {
    h.cx.update(|window, cx| {
        window
            .root::<PhoneRoot>()
            .flatten()
            .map(|phone| phone.read(cx).home_group_names(cx))
            .unwrap_or_default()
    })
}

/// The host's `HostLayouts` answering `request_id` for the unnamed-tab
/// spec: its own layout ("box", named after the host) with one tab nobody
/// named holding `c`, which leaves `a` and `b` in no tab; and a laptop's
/// layout.
fn unnamed_tab_layouts(request_id: &str) -> DaemonMessage {
    let pane =
        |id: &str, session: &str| json!({ "kind": "pane", "pane_id": id, "session_id": session });
    let tab = |id: &str, name: &str, grid: serde_json::Value| {
        json!({
            "id": id,
            "name": name,
            "content": { "kind": "grid", "grid": grid },
            "created_at": "2026-01-01T00:00:00Z",
        })
    };
    serde_json::from_value(json!({
        "type": "host_layouts",
        "host_name": "box",
        "layouts": [
            { "client_id": "desk", "name": "box", "tabs": [tab("t2", "", pane("p3", "c"))] },
            { "client_id": "laptop", "name": "laptop", "tabs": [tab("t9", "Other", pane("p9", "b"))] },
        ],
        "request_id": request_id,
    }))
    .expect("a host layouts reply")
}

#[gpui::test]
fn desktop_tabs_rows_show_their_repo_and_an_unnamed_tab_its_automatic_name(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, &dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let remote = saved_remote();
    open_host_with(&mut h, &remote, &[FEATURE_HOST_LAYOUTS]);
    h.cx.run_until_parked();

    for id in ["a", "b", "c"] {
        assert!(
            !drawn(&mut h, &row_container(&remote, id)),
            "Manual's heading already names the repo"
        );
    }

    sort_by(&mut h, "desktop-tabs");
    let request_id = h
        .host(&remote)
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::ListHostLayouts { request_id } => request_id,
            _ => None,
        })
        .expect("the phone asks for the host's layout");
    h.host(&remote).send(unnamed_tab_layouts(&request_id));
    h.cx.run_until_parked();

    assert_eq!(
        group_names(&mut h),
        ["r2", "Not in a tab"],
        "the tab nobody named heads itself by its session's repo"
    );
    for id in ["a", "b", "c"] {
        assert!(
            drawn(&mut h, &row_container(&remote, id)),
            "{id} names the repo it sits under, in a tab or in Not in a tab"
        );
    }
}

/// The selector of the host chip on `target`'s row for session `id`.
fn row_chip(target: &Target, id: &str) -> String {
    format!("phone-home-row-host-{}::{id}", wire(target))
}

/// The selector of `target`'s header pinned over the list's top.
fn pinned_selector(target: &Target) -> String {
    format!("phone-home-pinned-{}", wire(target))
}

/// The tag of the host whose header Home pins over its list now.
fn pinned(h: &mut Harness<'_>) -> Option<String> {
    h.cx.run_until_parked();
    h.cx.update(|window, cx| {
        window
            .root::<PhoneRoot>()
            .flatten()
            .and_then(|phone| phone.read(cx).pinned_host(cx))
    })
}

/// The ids of the idle sessions that make a host's list run past the
/// list's room.
fn filler_ids() -> Vec<String> {
    (0..30).map(|n| format!("f{n:02}")).collect()
}

/// Both saved hosts open, each listing [`listed`] and an idle session in
/// r1 per [`filler_ids`].
fn two_long_hosts<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = two_host_phone(cx, dir);
    for target in [saved_remote(), den()] {
        open_host(&mut h, &target);
        let mut sessions = listed();
        sessions.extend(
            filler_ids()
                .iter()
                .map(|id| session(id).in_repo("r1").status("idle").build()),
        );
        h.host(&target).send(DaemonMessage::Sessions { sessions });
    }
    h.cx.run_until_parked();
    h
}

/// Scrolls Home's list down by `dy` (up for a negative one) with one pan of
/// the list's own scroll events, as [`flick`] pans the tray's card.
fn scroll_list(h: &mut Harness<'_>, dy: Pixels) {
    let at = h.center("phone-home");
    scroll_list_at(h, at, dy);
}

/// [`scroll_list`] with the pan at `at`.
fn scroll_list_at(h: &mut Harness<'_>, at: Point<Pixels>, dy: Pixels) {
    let pan = |delta: Pixels, touch_phase: TouchPhase| ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(0.0), delta)),
        modifiers: Modifiers::none(),
        touch_phase,
        source: ScrollSource::Touch,
    };
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Started));
    h.cx.simulate_event(pan(-dy, TouchPhase::Moved));
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Ended));
    h.cx.run_until_parked();
}

#[gpui::test]
fn a_pan_from_the_pinned_header_still_scrolls_the_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_long_hosts(cx, &dir);
    scroll_list(&mut h, px(200.0));
    let header = pinned_selector(&saved_remote());
    assert!(drawn(&mut h, &header), "the first host's header is pinned");
    let target = row(&saved_remote(), "f20");
    let before = h.bounds(&target).top();
    let at = h.center(&header);

    scroll_list_at(&mut h, at, px(100.0));

    assert_eq!(
        h.bounds(&target).top(),
        before - px(100.0),
        "the pan from the pinned strip scrolled the list"
    );
}

#[gpui::test]
fn a_long_tray_label_keeps_its_width_beside_the_host_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_host_phone(cx, &dir);
    open_host(&mut h, &saved_remote());
    open_host(&mut h, &den());
    let mut long = waiting("c", 2, question(&["main", "dev"]));
    long.label = "Release plan check across every saved host before the cut, then tag it".into();
    send_waits(&mut h, vec![long]);
    assert!(drawn(&mut h, "phone-tray-host"), "the card names its host");

    let card = h.bounds("phone-tray-card");
    let label = h.bounds("phone-tray-label");
    // The card's 12 pt padding and 1 pt border either side.
    let line_right = card.right() - px(13.0);
    assert!(
        (label.left() - (card.left() + px(13.0))).abs() < px(0.5),
        "the label starts the line: {label:?} in {card:?}"
    );
    assert!(
        (label.right() - line_right).abs() < px(0.5),
        "a label longer than the line keeps the whole line: {label:?} in {card:?}"
    );
    assert!(
        h.bounds("phone-tray-host").left() >= label.right(),
        "the chip gives way to the label"
    );

    let mut short = waiting("c", 2, question(&["main", "dev"]));
    short.label = "Cleanup".into();
    send_waits(&mut h, vec![short]);
    let label = h.bounds("phone-tray-label");
    let chip = h.bounds("phone-tray-host");
    assert!(chip.left() >= label.right(), "the chip follows the label");
    assert!(
        chip.right() <= card.right() - px(13.0) + px(0.5),
        "the chip is no wider than the line leaves it: {chip:?} in {card:?}"
    );
}

#[gpui::test]
fn two_hosts_rows_and_tray_cards_show_the_host_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_host_phone(cx, &dir);
    open_host(&mut h, &saved_remote());
    open_host(&mut h, &den());
    send_waits(&mut h, vec![waiting("c", 2, question(&["main", "dev"]))]);

    for target in [saved_remote(), den()] {
        for id in IDS {
            let chip = row_chip(&target, id);
            assert!(drawn(&mut h, &chip), "{chip} is drawn");
            let chevron = format!("{}-chevron", row(&target, id));
            assert!(
                h.bounds(&chip).right() <= h.bounds(&chevron).left(),
                "{chip} leaves the chevron its place"
            );
        }
    }
    assert_eq!(header(&mut h).as_deref(), Some("Needs you · 1 of 1"));
    assert!(drawn(&mut h, "phone-tray-host"), "the card names its host");
}

#[gpui::test]
fn one_host_shows_no_host_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    send_waits(&mut h, vec![waiting("c", 2, question(&["main", "dev"]))]);

    for id in IDS {
        assert!(
            drawn(&mut h, &row(&saved_remote(), id)),
            "{id}'s row is drawn"
        );
        let chip = row_chip(&saved_remote(), id);
        assert!(!drawn(&mut h, &chip), "{chip} is not drawn");
    }
    assert_eq!(header(&mut h).as_deref(), Some("Needs you · 1 of 1"));
    assert!(!drawn(&mut h, "phone-tray-host"), "the card names no host");
}

#[gpui::test]
fn the_scrolled_past_host_header_is_pinned_at_the_list_top(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_long_hosts(cx, &dir);
    let (first, second) = (saved_remote(), den());
    let list_top = h.bounds("phone-home").top();
    let second_top = h
        .bounds(&format!("phone-home-host-{}", wire(&second)))
        .top()
        - list_top;
    assert!(
        second_top > px(500.0),
        "the second host's header starts below the room: {second_top:?}"
    );

    assert_eq!(pinned(&mut h), None, "nothing is pinned at rest");
    assert!(!drawn(&mut h, &pinned_selector(&first)));
    assert!(!drawn(&mut h, &pinned_selector(&second)));

    scroll_list(&mut h, px(60.0));

    assert_eq!(pinned(&mut h), Some(wire(&first)), "the first host pins");
    assert!(drawn(&mut h, &pinned_selector(&first)));
    assert!(!drawn(&mut h, &pinned_selector(&second)));
    assert_eq!(
        h.bounds(&pinned_selector(&first)).top(),
        h.bounds("phone-home").top(),
        "at the list's top"
    );

    scroll_list(&mut h, second_top - px(70.0));

    assert_eq!(
        pinned(&mut h),
        Some(wire(&first)),
        "the first host stays pinned while the second's header is below the top"
    );

    scroll_list(&mut h, px(30.0));

    assert_eq!(pinned(&mut h), Some(wire(&second)), "the second host pins");
    assert!(drawn(&mut h, &pinned_selector(&second)));
    assert!(!drawn(&mut h, &pinned_selector(&first)));
    assert_eq!(
        h.bounds(&pinned_selector(&second)).top(),
        h.bounds("phone-home").top(),
        "at the list's top"
    );

    scroll_list(&mut h, px(-10_000.0));

    assert_eq!(pinned(&mut h), None, "back at the top nothing is pinned");
    assert!(!drawn(&mut h, &pinned_selector(&first)));
    assert!(!drawn(&mut h, &pinned_selector(&second)));
}

#[gpui::test]
fn a_tray_appearing_without_a_scroll_re_checks_the_pin(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_long_hosts(cx, &dir);
    let first = saved_remote();
    let scrolled = px(150.0);
    scroll_list(&mut h, scrolled);
    assert_eq!(pinned(&mut h), Some(wire(&first)), "the first host pins");

    let mut sessions: Vec<SessionSnapshot> = listed().into_iter().filter(|s| s.id != "c").collect();
    sessions.push(waiting("c", 2, question(&["main", "dev"])));
    sessions.extend(
        filler_ids()
            .iter()
            .map(|id| session(id).in_repo("r1").status("idle").build()),
    );
    h.host(&first).send(DaemonMessage::Sessions { sessions });
    h.cx.run_until_parked();

    let list_top = h.bounds("phone-home").top();
    let tray_height = h.bounds("phone-tray").size.height;
    let header_top = h.bounds(&format!("phone-home-host-{}", wire(&first))).top() - list_top;
    assert_eq!(
        header_top,
        tray_height - scrolled,
        "the first host's header sits under the {tray_height:?} tray scrolled {scrolled:?}"
    );
    assert_eq!(
        pinned(&mut h),
        None,
        "the first host's header at {header_top:?} below the list's top is wholly visible"
    );
    assert!(
        !drawn(&mut h, &pinned_selector(&first)),
        "no pinned header covers the tray's card at {scrolled:?}"
    );
}

#[gpui::test]
fn a_list_that_shrinks_under_its_offset_drops_the_stale_pin(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_long_hosts(cx, &dir);
    let first = saved_remote();
    scroll_list(&mut h, px(400.0));
    assert_eq!(pinned(&mut h), Some(wire(&first)), "the first host pins");

    for target in [saved_remote(), den()] {
        h.host(&target)
            .send(DaemonMessage::Sessions { sessions: listed() });
    }
    h.cx.run_until_parked();

    assert_eq!(
        pinned(&mut h),
        None,
        "the shrunk list fits: nothing is pinned"
    );
    for target in [saved_remote(), den()] {
        assert!(
            !drawn(&mut h, &pinned_selector(&target)),
            "no stale pin of {} covers the first host's header",
            wire(&target)
        );
    }
    assert_eq!(
        h.bounds(&format!("phone-home-host-{}", wire(&first))).top(),
        h.bounds("phone-home").top(),
        "the first host's own header is at the list's top"
    );
}

#[gpui::test]
fn a_tap_on_the_pinned_header_opens_no_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_long_hosts(cx, &dir);
    scroll_list(&mut h, px(200.0));
    let header = pinned_selector(&saved_remote());
    assert!(drawn(&mut h, &header), "the first host's header is pinned");
    let at = h.center(&header);
    let ids: Vec<String> = IDS
        .iter()
        .map(|id| (*id).to_owned())
        .chain(filler_ids())
        .collect();
    assert!(
        ids.iter()
            .any(|id| h.bounds(&row(&saved_remote(), id)).contains(&at)),
        "a row lies under the pinned header"
    );

    h.tap_on(&header);

    assert_eq!(shown(&mut h), None, "the tap opens no session");
}

/// The sessions `target` was sent an `AnswerPrompt` for since the last look.
fn answered(h: &mut Harness<'_>, target: &Target) -> Vec<String> {
    h.host(target)
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::AnswerPrompt { session_id, .. } => Some(session_id),
            _ => None,
        })
        .collect()
}

/// Two open hosts listing one session id `b`, each waiting on the same
/// question; `den`'s wait is the longer, so its card shows first.
fn two_hosts_asking_for_one_session<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = two_host_phone(cx, dir);
    open_host(&mut h, &saved_remote());
    open_host(&mut h, &den());
    h.host(&den()).send(DaemonMessage::Sessions {
        sessions: vec![waiting("b", 9, question(&["main", "dev"]))],
    });
    send_waits(&mut h, vec![waiting("b", 3, question(&["main", "dev"]))]);
    assert_eq!(
        header(&mut h).as_deref(),
        Some("Needs you · 1 of 2"),
        "both hosts' cards are in the tray"
    );
    h.host(&saved_remote()).sent();
    h.host(&den()).sent();
    h
}

#[gpui::test]
fn a_tray_answer_goes_to_the_host_whose_card_was_tapped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts_asking_for_one_session(cx, &dir);

    h.click_on("phone-tray-choice-1");

    assert_eq!(
        answered(&mut h, &den()),
        ["b"],
        "the answer goes to the host whose card showed"
    );
    assert!(
        answered(&mut h, &saved_remote()).is_empty(),
        "the other host listing the same session id is sent nothing"
    );
}

#[gpui::test]
fn opening_an_ended_tray_card_clears_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    h.host(&saved_remote()).send(DaemonMessage::Attention {
        session_id: "b".to_owned(),
        reason: AttentionReason::Stopped,
    });
    send_waits(
        &mut h,
        vec![
            session("b")
                .label("Bravo")
                .in_repo("r2")
                .status("stopped")
                .status_since(minutes_ago(2))
                .build(),
        ],
    );
    assert_eq!(header(&mut h).as_deref(), Some("Needs you · 1 of 1"));
    assert!(
        drawn(&mut h, "phone-tray-open"),
        "an ended card offers Open"
    );

    h.click_on("phone-tray-open");
    assert!(drawn(&mut h, "phone-session"), "Open shows the session");

    h.click_on("phone-session-back");

    assert!(drawn(&mut h, "phone-home"), "Back returns to Home");
    assert_eq!(header(&mut h), None, "the opened ended card is gone");
}
