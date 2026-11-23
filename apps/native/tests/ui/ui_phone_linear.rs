//! The phone's Linear card: a tap on an issue link in the terminal asks the
//! host for the issue once while it is asked and holds its card, which the
//! reply fills; the keyboard is left alone, a plain URL still opens, and a
//! host with Linear off opens the link instead.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::time::Duration;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{
    BackGesturePhase, Edges, Modifiers, Pixels, Point, ScrollDelta, ScrollEvent, ScrollSource,
    TestAppContext, TouchPhase, WindowInsets, point, px,
};
use protocol::{
    ChatEntry, ChatOption, ChatPrompt, ChatPromptKind, ChatScreen, ClientMessage, ComposerState,
    DaemonMessage,
};
use rustling_tulip_native::keyboard::{clear_keyboard_calls, keyboard_calls};
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::target::HostKey;
use serde_json::json;
use support::{Harness, Opened, REMOTE, TestDir, repo, saved_remote, seed_ui, session};

const ISSUE_URL: &str = "https://linear.app/acme/issue/RT-12/slug";
/// The address `RT-12` is detected with in workspace `acme`, and the one
/// its copy names.
const ISSUE_WEB: &str = "https://linear.app/acme/issue/RT-12";
/// The card sheet's selector.
const SHEET: &str = "phone-linear-sheet";

/// The selector of the saved host's Home row for session `id`.
fn row(id: &str) -> String {
    let wire = HostKey::of(&saved_remote())
        .wire()
        .expect("a tailnet host has a wire key")
        .to_owned();
    format!("phone-home-row-{wire}::{id}")
}

/// The Linear context of a host whose workspace `acme` has the team `RT`
/// and a key.
fn linear() -> DaemonMessage {
    DaemonMessage::LinearContext {
        team_keys: vec!["RT".to_owned()],
        workspace: Some("acme".to_owned()),
        key_configured: true,
    }
}

/// The `LinearStatus` of a host with Linear `enabled` or off.
fn linear_status(enabled: bool) -> DaemonMessage {
    DaemonMessage::LinearStatus {
        enabled,
        key: protocol::LinearKeyState::Stored,
        workspace_name: Some("Acme".to_owned()),
        team_keys: vec!["RT".to_owned()],
        last_error: None,
        key_saved: true,
    }
}

/// The phone on the saved [`REMOTE`]'s session `n`, a shell, on its
/// terminal page, the host listing `features` and [`linear`]; `text` is
/// the terminal's first row, and everything sent so far is drained.
fn terminal_on<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    features: &[&str],
    text: &str,
) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect_with_features(features);
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    let shell = session("n")
        .label("November")
        .in_repo("r1")
        .status("working")
        .no_agent()
        .build();
    host.send(DaemonMessage::Sessions {
        sessions: vec![shell],
    });
    host.send(linear());
    h.click_on(&row("n"));
    h.host(&saved_remote())
        .pty("n", format!("{text}\r\n").as_bytes());
    h.cx.run_until_parked();
    h.host(&saved_remote()).sent();
    // A frame lays the grid out, so its cells have places.
    h.bounds("phone-session-terminal");
    h
}

/// [`terminal_on`] over a host listing `linear_links`.
fn linked<'a>(cx: &'a mut TestAppContext, dir: &TestDir, text: &str) -> Harness<'a> {
    terminal_on(cx, dir, &[protocol::FEATURE_LINEAR_LINKS], text)
}

/// A finger down and straight up on the terminal's cell at `col`, `row`,
/// a second after any earlier tap, so no two make a double tap.
fn tap_cell(h: &mut Harness<'_>, col: usize, row: usize) {
    h.advance(Duration::from_secs(1));
    let at = h
        .root(|root, cx| {
            root.phone_terminal_view()
                .and_then(|view| view.read(cx).cell_center(col, row))
        })
        .expect("the terminal's grid is laid out");
    h.touch(TouchPhase::Started, at);
    h.touch(TouchPhase::Ended, at);
}

/// The lines of the card the phone's terminal holds; `None` while it holds
/// none.
fn card(h: &mut Harness<'_>) -> Option<Vec<String>> {
    h.cx.run_until_parked();
    h.root(|root, cx| {
        root.phone_terminal_view()
            .and_then(|view| view.read(cx).linear_card_lines())
    })
}

/// Whether the held card has the line `line`.
fn card_says(h: &mut Harness<'_>, line: &str) -> bool {
    card(h).is_some_and(|lines| lines.iter().any(|shown| shown == line))
}

/// The identifiers of every `GetLinearIssues` the host was sent since the
/// last call.
fn asked(h: &mut Harness<'_>) -> Vec<Vec<String>> {
    h.host(&saved_remote())
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::GetLinearIssues { identifiers, .. } => Some(identifiers),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn a_tap_on_an_issue_link_asks_once_and_holds_its_card(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = linked(cx, &dir, "RT-12 now");

    tap_cell(&mut h, 1, 0);

    assert_eq!(asked(&mut h), [vec!["RT-12".to_owned()]], "one ask");
    assert!(
        card_says(&mut h, "Fetching from Linear"),
        "{:?}",
        card(&mut h)
    );
    h.host(&saved_remote())
        .send(support::linear_issues(vec![support::linear_issue(
            "RT-12",
            "Phone card",
        )]));
    assert!(card_says(&mut h, "Phone card"), "{:?}", card(&mut h));
    assert!(card_says(&mut h, "In Progress"), "{:?}", card(&mut h));
    assert!(!card_says(&mut h, "Fetching from Linear"));
    assert_eq!(h.opened(), [], "a tap on an issue opens nothing");
}

#[gpui::test]
fn a_second_tap_while_asked_sends_no_second_ask(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = linked(cx, &dir, "RT-12 now");
    tap_cell(&mut h, 1, 0);
    assert_eq!(asked(&mut h), [vec!["RT-12".to_owned()]]);
    // The card's scrim covers the terminal, so the second tap needs it down.
    h.root_update(|root, _, cx| root.dismiss_linear_card(cx));

    tap_cell(&mut h, 2, 0);

    assert_eq!(
        asked(&mut h),
        Vec::<Vec<String>>::new(),
        "not again while the first is in flight"
    );
    assert!(
        card_says(&mut h, "Fetching from Linear"),
        "{:?}",
        card(&mut h)
    );
}

#[gpui::test]
fn a_tap_on_an_issue_link_leaves_the_keyboard_alone(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = linked(cx, &dir, "RT-12 now");
    clear_keyboard_calls();

    tap_cell(&mut h, 1, 0);

    assert_eq!(
        keyboard_calls(),
        [],
        "a tap on an issue leaves the keyboard"
    );
    assert!(card(&mut h).is_some(), "and holds the card");
}

#[gpui::test]
fn a_host_without_linear_links_is_never_asked(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = terminal_on(cx, &dir, &[], "RT-12 now");

    tap_cell(&mut h, 1, 0);

    assert_eq!(asked(&mut h), Vec::<Vec<String>>::new(), "no linear_links");
    assert_eq!(
        h.opened(),
        [Opened::Url(ISSUE_WEB.to_owned())],
        "a host that cannot answer opens the link"
    );
    assert!(!drawn(&mut h, SHEET), "and raises no card");
}

#[gpui::test]
fn a_tap_on_a_plain_url_still_opens_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let url = "https://example.com/docs";
    let mut h = linked(cx, &dir, &format!("see {url} now"));

    tap_cell(&mut h, 8, 0);

    assert_eq!(h.opened(), [Opened::Url(url.to_owned())]);
    assert_eq!(asked(&mut h), Vec::<Vec<String>>::new());
    assert_eq!(card(&mut h), None, "a URL holds no card");
}

#[gpui::test]
fn a_tap_on_an_issue_link_with_linear_off_opens_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = linked(cx, &dir, &format!("see {ISSUE_URL} now"));
    h.host(&saved_remote()).send(linear_status(false));

    tap_cell(&mut h, 10, 0);

    assert_eq!(h.opened(), [Opened::Url(ISSUE_URL.to_owned())]);
    assert_eq!(asked(&mut h), Vec::<Vec<String>>::new());
    assert_eq!(card(&mut h), None, "Linear off holds no card");
}

#[gpui::test]
fn dismissing_the_card_clears_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = linked(cx, &dir, "RT-12 now");
    tap_cell(&mut h, 1, 0);
    assert!(card(&mut h).is_some(), "the tap holds the card");

    h.root_update(|root, _, cx| root.dismiss_linear_card(cx));

    assert_eq!(card(&mut h), None, "the card is gone");
}

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// Whether `selector` is drawn inside the card sheet.
fn in_sheet(h: &mut Harness<'_>, selector: &str) -> bool {
    let sheet = h.bounds(SHEET);
    drawn(h, selector) && sheet.contains(&h.bounds(selector).center())
}

/// The phone over a host listing `linear_links`, on its terminal showing
/// `RT-12 now`, with the issue's card raised by a tap and still asked.
fn card_up<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = linked(cx, dir, "RT-12 now");
    tap_cell(&mut h, 1, 0);
    asked(&mut h);
    h
}

/// [`card_up`] with the host's answer in: the card filled.
fn card_filled<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = card_up(cx, dir);
    h.host(&saved_remote())
        .send(support::linear_issues(vec![support::linear_issue(
            "RT-12",
            "Phone card",
        )]));
    h.cx.run_until_parked();
    h
}

/// A committed back gesture from the page's leading edge.
fn back(h: &mut Harness<'_>) {
    let page = h.bounds("phone-session");
    let at = point(page.origin.x + px(4.0), page.origin.y + px(200.0));
    h.back_gesture(BackGesturePhase::Started, at);
    h.back_gesture(BackGesturePhase::Committed, at);
    h.cx.run_until_parked();
}

/// A downward pan over the card sheet's handle: `steps` movements of `dy`,
/// then the lift.
fn drag_sheet(h: &mut Harness<'_>, dy: Pixels, steps: usize) {
    let at = h.bounds("phone-linear-handle").center();
    let pan = |delta: Pixels, touch_phase: TouchPhase| ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(0.0), delta)),
        modifiers: Modifiers::none(),
        touch_phase,
        source: ScrollSource::Touch,
    };
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Started));
    for _ in 0..steps {
        h.cx.simulate_event(pan(dy, TouchPhase::Moved));
    }
    h.cx.simulate_event(pan(px(0.0), TouchPhase::Ended));
    h.cx.run_until_parked();
}

#[gpui::test]
fn the_tapped_issues_card_shows_as_a_sheet(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = card_up(cx, &dir);

    assert!(drawn(&mut h, SHEET), "the tap raises the sheet");
    assert!(drawn(&mut h, "phone-linear-scrim"));
    assert!(drawn(&mut h, "phone-session-terminal"), "over the terminal");
    assert!(
        in_sheet(&mut h, "linear-card-notice-phone=Fetching from Linear"),
        "while asked it says so"
    );
    assert!(in_sheet(&mut h, "linear-card-id-phone=RT-12"));

    h.host(&saved_remote())
        .send(support::linear_issues(vec![support::linear_issue(
            "RT-12",
            "Phone card",
        )]));

    assert!(in_sheet(&mut h, "linear-card-title-phone=Phone card"));
    assert!(in_sheet(&mut h, "linear-card-state-phone=In Progress"));
    assert!(!drawn(
        &mut h,
        "linear-card-notice-phone=Fetching from Linear"
    ));
    assert!(in_sheet(&mut h, "phone-linear-handle"));
    assert!(in_sheet(&mut h, "phone-linear-open"));
    let (sheet, scrim) = (h.bounds(SHEET), h.bounds("phone-linear-scrim"));
    assert!(
        sheet.bottom() + px(1.0) >= scrim.bottom(),
        "anchored to the bottom: {sheet:?} in {scrim:?}"
    );
    assert!(!drawn(&mut h, "linear-card"), "no desktop tooltip");
}

#[gpui::test]
fn back_takes_the_card_down_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = card_filled(cx, &dir);

    back(&mut h);

    assert!(!drawn(&mut h, SHEET), "the card is down");
    assert_eq!(card(&mut h), None);
    assert!(
        drawn(&mut h, "phone-session-terminal"),
        "the page is unchanged"
    );

    back(&mut h);

    assert!(
        !drawn(&mut h, "phone-session"),
        "a second back leaves the page, as back did before"
    );
}

#[gpui::test]
fn a_tap_on_the_scrim_takes_the_card_down(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = card_filled(cx, &dir);

    h.click_on("phone-linear-scrim");

    assert!(!drawn(&mut h, SHEET));
    assert!(!drawn(&mut h, "phone-linear-scrim"));
    assert_eq!(card(&mut h), None);
    assert!(drawn(&mut h, "phone-session-terminal"));
    assert_eq!(h.opened(), [], "the scrim opens nothing");
}

#[gpui::test]
fn a_swipe_down_takes_the_card_down(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = card_filled(cx, &dir);

    drag_sheet(&mut h, px(60.0), 5);

    assert!(!drawn(&mut h, SHEET));
    assert_eq!(card(&mut h), None);
    assert!(drawn(&mut h, "phone-session-terminal"));
}

#[gpui::test]
fn a_short_drag_leaves_it_up(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = card_filled(cx, &dir);
    let before = h.bounds(SHEET);

    drag_sheet(&mut h, px(4.0), 2);

    assert!(drawn(&mut h, SHEET), "a short slow drag springs back");
    assert_eq!(h.bounds(SHEET), before, "to where it was");
    assert!(card(&mut h).is_some());
}

#[gpui::test]
fn closing_the_terminal_takes_the_card_down(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = card_filled(cx, &dir);
    assert!(drawn(&mut h, SHEET));

    // As a turn upright closes the terminal; the next frame on the
    // Terminal page opens it afresh, as a turn back on its side does.
    h.root_update(|root, _, cx| root.close_phone_terminal(cx));
    h.cx.run_until_parked();

    assert!(
        drawn(&mut h, "phone-session-terminal"),
        "the terminal is back"
    );
    assert!(!drawn(&mut h, SHEET), "and the old card stays down");
    assert_eq!(card(&mut h), None);
}

/// The phone terminal's rows as drawn.
fn grid(h: &mut Harness<'_>) -> Vec<String> {
    h.cx.run_until_parked();
    h.root(|root, cx| {
        root.phone_terminal_view()
            .map(|view| view.read(cx).grid_text())
    })
    .expect("the terminal is open")
}

/// A touch pan event of `dy` in `phase` at `at`.
fn pan_at(h: &mut Harness<'_>, at: gpui::Point<Pixels>, dy: Pixels, phase: TouchPhase) {
    h.cx.simulate_event(ScrollEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(0.0), dy)),
        modifiers: Modifiers::none(),
        touch_phase: phase,
        source: ScrollSource::Touch,
    });
    h.cx.run_until_parked();
}

#[gpui::test]
fn a_committed_swipe_does_not_scroll_the_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let history = (0..120)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\r\n");
    let mut h = linked(cx, &dir, &format!("{history}\r\nRT-12 now"));
    let row = grid(&mut h)
        .iter()
        .position(|text| text.starts_with("RT-12"))
        .expect("the issue is on screen");
    tap_cell(&mut h, 1, row);
    asked(&mut h);
    assert!(drawn(&mut h, SHEET));
    let at = h.bounds("phone-linear-handle").center();

    pan_at(&mut h, at, px(0.0), TouchPhase::Started);
    for _ in 0..5 {
        pan_at(&mut h, at, px(60.0), TouchPhase::Moved);
    }
    pan_at(&mut h, at, px(0.0), TouchPhase::Ended);
    assert!(!drawn(&mut h, SHEET), "the swipe commits");
    let before = grid(&mut h);
    // The swipe's momentum, at the point the pan began.
    for _ in 0..4 {
        pan_at(&mut h, at, px(40.0), TouchPhase::Moved);
    }
    pan_at(&mut h, at, px(10.0), TouchPhase::Ended);

    assert_eq!(grid(&mut h), before, "the terminal did not scroll");
    let sent = h.host(&saved_remote()).sent();
    assert!(sent.is_empty(), "nothing reached the host: {sent:?}");
}

/// The phone on the saved [`REMOTE`]'s session `a`, an agent's, over a
/// host listing `linear_links` and [`linear`]: its chat loaded with no
/// prompt, then its Terminal page showing `RT-12 now`.
fn chat_terminal<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect_with_features(&[protocol::FEATURE_LINEAR_LINKS]);
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    let agent = session("a")
        .label("Alpha")
        .in_repo("r1")
        .status("working")
        .build();
    host.send(DaemonMessage::Sessions {
        sessions: vec![agent],
    });
    host.send(linear());
    h.click_on(&row("a"));
    let request = h
        .host(&saved_remote())
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::LoadChat {
                session_id,
                request_id,
            } if session_id == "a" => Some(request_id),
            _ => None,
        });
    h.host(&saved_remote()).send(DaemonMessage::Chat {
        session_id: "a".to_owned(),
        entries: vec![ChatEntry::User {
            id: "u1".to_owned(),
            text: "message u1".to_owned(),
            pending: false,
        }],
        hidden: 0,
        screen: ChatScreen {
            composer: ComposerState::Ready,
            prompt: None,
        },
        request_id: request.flatten(),
    });
    h.click_on("phone-session-toggle");
    h.host(&saved_remote()).pty("a", b"RT-12 now\r\n");
    h.cx.run_until_parked();
    h.host(&saved_remote()).sent();
    h.bounds("phone-session-terminal");
    h
}

/// A pending question, `q1`.
fn question() -> ChatPrompt {
    ChatPrompt {
        id: "q1".to_owned(),
        kind: ChatPromptKind::Question,
        subject: "calc.py".to_owned(),
        body: "Which one?".to_owned(),
        options: [(1, "sub"), (2, "div")]
            .iter()
            .map(|(number, label)| ChatOption {
                number: *number,
                label: (*label).to_owned(),
                description: None,
                selected: false,
            })
            .collect(),
        questions: Vec::new(),
        can_amend: true,
        always_allow_tip: None,
        multi: false,
        cursor: None,
        adjust: None,
        preview: None,
    }
}

#[gpui::test]
fn a_question_waits_under_the_card_until_it_goes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_terminal(cx, &dir);
    assert!(drawn(&mut h, "phone-session-terminal"));
    tap_cell(&mut h, 1, 0);
    assert!(drawn(&mut h, SHEET), "the tap raises the card");

    h.host(&saved_remote())
        .send(DaemonMessage::ChatScreenChanged {
            session_id: "a".to_owned(),
            screen: ChatScreen {
                composer: ComposerState::Dialog,
                prompt: Some(question()),
            },
        });

    assert!(drawn(&mut h, SHEET), "the card stays up");
    assert!(
        !drawn(&mut h, "phone-prompt-sheet"),
        "the question waits under it"
    );

    back(&mut h);

    assert!(!drawn(&mut h, SHEET), "back takes the card down");
    assert!(
        drawn(&mut h, "phone-prompt-sheet"),
        "and the question rises"
    );
}

#[gpui::test]
fn open_opens_the_issue_and_takes_the_card_down(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = card_filled(cx, &dir);

    h.click_on("phone-linear-open");

    assert_eq!(h.opened(), [Opened::Url(ISSUE_WEB.to_owned())]);
    assert!(!drawn(&mut h, SHEET));
    assert_eq!(card(&mut h), None);
}

/// The phone on the saved [`REMOTE`]'s session `a`, an agent's, on its chat
/// page, over a host listing `features`; `context` is what the host sends
/// as its Linear context, when it sends one. Its chat is loaded with
/// answer `a1` reading `text`, and everything sent so far is drained.
fn chat_page<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    features: &[&str],
    text: &str,
    context: Option<DaemonMessage>,
) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect_with_features(features);
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    let agent = session("a")
        .label("Alpha")
        .in_repo("r1")
        .status("working")
        .build();
    let shell = session("b")
        .label("Bravo")
        .in_repo("r1")
        .status("idle")
        .no_agent()
        .build();
    host.send(DaemonMessage::Sessions {
        sessions: vec![agent, shell],
    });
    if let Some(context) = context {
        host.send(context);
    }
    h.click_on(&row("a"));
    let request = h
        .host(&saved_remote())
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::LoadChat {
                session_id,
                request_id,
            } if session_id == "a" => Some(request_id),
            _ => None,
        });
    h.host(&saved_remote()).send(DaemonMessage::Chat {
        session_id: "a".to_owned(),
        entries: vec![ChatEntry::Answer {
            id: "a1".to_owned(),
            text: text.to_owned(),
            live: false,
        }],
        hidden: 0,
        screen: ChatScreen {
            composer: ComposerState::Ready,
            prompt: None,
        },
        request_id: request.flatten(),
    });
    h.cx.run_until_parked();
    h.host(&saved_remote()).sent();
    h.bounds("phone-session-chat");
    h
}

/// [`chat_page`] over a host whose Linear context is out and that lists
/// `linear_links`.
fn chat_on<'a>(cx: &'a mut TestAppContext, dir: &TestDir, text: &str) -> Harness<'a> {
    chat_page(
        cx,
        dir,
        &[protocol::FEATURE_LINEAR_LINKS],
        text,
        Some(linear()),
    )
}

/// The first point, scanning answer `a1`'s row from its left edge at its
/// middle height, that the phone's chat maps to a byte in `bytes`.
fn on_bytes(h: &mut Harness<'_>, bytes: std::ops::Range<usize>) -> Point<Pixels> {
    let bounds = h.bounds("chat-row-phone-a1");
    let y = bounds.top() + bounds.size.height / 2.0;
    let mut x = bounds.left();
    let mut found = None;
    while found.is_none() && x < bounds.right() {
        let at = point(x, y);
        if let Some((entry, 0, byte)) = h.root(|root, _| root.phone_chat_hit(at))
            && entry == "a1"
            && bytes.contains(&byte)
        {
            found = Some(at);
        }
        x += px(1.0);
    }
    found.expect("answer a1 draws those bytes")
}

/// A finger down and straight up on answer `a1`'s bytes `bytes`, a second
/// after any earlier tap, so no two make a double tap.
fn tap_answer(h: &mut Harness<'_>, bytes: std::ops::Range<usize>) {
    h.advance(Duration::from_secs(1));
    let at = on_bytes(h, bytes);
    h.touch(TouchPhase::Started, at);
    h.touch(TouchPhase::Ended, at);
    h.cx.run_until_parked();
}

/// Runs `drive` on the window's phone root, then lets it settle.
fn phone_update(
    h: &mut Harness<'_>,
    drive: impl FnOnce(&mut PhoneRoot, &mut gpui::Context<PhoneRoot>),
) {
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.update(cx, drive);
    });
    h.cx.run_until_parked();
}

#[gpui::test]
fn a_tap_on_an_issue_in_the_phone_chat_asks_once_and_raises_the_sheet(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_on(cx, &dir, "RT-12 now");

    tap_answer(&mut h, 0..5);

    assert_eq!(asked(&mut h), [vec!["RT-12".to_owned()]], "one ask");
    assert!(drawn(&mut h, SHEET), "the tap raises the sheet");
    assert!(
        in_sheet(&mut h, "linear-card-notice-phone=Fetching from Linear"),
        "while asked it says so"
    );
    assert!(in_sheet(&mut h, "linear-card-id-phone=RT-12"));
    assert_eq!(h.opened(), [], "a tap on an issue opens nothing");

    h.host(&saved_remote())
        .send(support::linear_issues(vec![support::linear_issue(
            "RT-12",
            "Phone card",
        )]));

    assert!(in_sheet(&mut h, "linear-card-title-phone=Phone card"));
    assert!(!drawn(
        &mut h,
        "linear-card-notice-phone=Fetching from Linear"
    ));
}

#[gpui::test]
fn a_chat_issue_tap_on_a_host_without_linear_links_opens_the_link(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_page(cx, &dir, &[], "RT-12 now", Some(linear()));

    tap_answer(&mut h, 0..5);

    assert_eq!(asked(&mut h), Vec::<Vec<String>>::new(), "no linear_links");
    assert_eq!(
        h.opened(),
        [Opened::Url(ISSUE_WEB.to_owned())],
        "a host that cannot answer opens the link"
    );
    assert!(!drawn(&mut h, SHEET), "and raises no card");
}

#[gpui::test]
fn a_chat_issue_tap_with_linear_off_opens_the_link(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_on(cx, &dir, "RT-12 now");
    h.host(&saved_remote()).send(linear_status(false));
    h.cx.run_until_parked();

    tap_answer(&mut h, 0..5);

    assert_eq!(
        asked(&mut h),
        Vec::<Vec<String>>::new(),
        "Linear off asks nothing"
    );
    assert_eq!(
        h.opened(),
        [],
        "and the identifier is no link to open either"
    );
    assert!(!drawn(&mut h, SHEET), "and no card rises");
}

#[gpui::test]
fn a_chat_card_sheet_goes_with_the_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_on(cx, &dir, "RT-12 now");
    tap_answer(&mut h, 0..5);
    assert!(drawn(&mut h, SHEET), "the tap raises the sheet");

    phone_update(&mut h, |phone, cx| {
        assert!(phone.open_session_on("b", cx), "Home lists b");
    });

    assert!(drawn(&mut h, "phone-session"), "another session's page");
    assert!(!drawn(&mut h, SHEET), "the sheet went with the session");
}

#[gpui::test]
fn a_chat_card_survives_the_page_toggle(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_on(cx, &dir, "RT-12 now");
    tap_answer(&mut h, 0..5);
    assert!(drawn(&mut h, SHEET), "the tap raises the sheet");

    phone_update(&mut h, PhoneRoot::tap_session_toggle);

    assert!(
        drawn(&mut h, "phone-session-terminal"),
        "the Terminal page shows"
    );
    assert!(drawn(&mut h, SHEET), "and the sheet stays over it");

    phone_update(&mut h, PhoneRoot::tap_session_toggle);

    assert!(drawn(&mut h, "phone-session-chat"), "the Chat page again");
    assert!(drawn(&mut h, SHEET), "and the sheet too");
}

#[gpui::test]
fn back_takes_a_chat_card_down_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_on(cx, &dir, "RT-12 now");
    tap_answer(&mut h, 0..5);
    assert!(drawn(&mut h, SHEET), "the tap raises the sheet");

    back(&mut h);

    assert!(!drawn(&mut h, SHEET), "the sheet is down");
    assert!(drawn(&mut h, "phone-session-chat"), "the page is unchanged");
}

/// The selector of the session header's title chip.
const TITLE_CHIP: &str = "phone-session-title-chip";

/// The phone on the saved [`REMOTE`]'s shell session `n`, titled `label`,
/// on its Terminal page, the host listing `features` and [`linear`];
/// everything sent so far is drained.
fn titled_shell<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    features: &[&str],
    label: &str,
) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect_with_features(features);
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    let shell = session("n")
        .label(label)
        .in_repo("r1")
        .status("working")
        .no_agent()
        .build();
    host.send(DaemonMessage::Sessions {
        sessions: vec![shell],
    });
    host.send(linear());
    h.click_on(&row("n"));
    h.cx.run_until_parked();
    h.host(&saved_remote()).sent();
    h
}

#[gpui::test]
fn the_session_header_draws_the_titles_identifier_as_a_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = titled_shell(
        cx,
        &dir,
        &[protocol::FEATURE_LINEAR_LINKS],
        "RT-12 Fix the login loop",
    );

    assert!(drawn(&mut h, TITLE_CHIP), "the header draws the chip");
    assert!(drawn(&mut h, "phone-session-label"), "and the title");
    let (chip, label) = (h.bounds(TITLE_CHIP), h.bounds("phone-session-label"));
    assert!(
        chip.right() <= label.left() + px(1.0),
        "the chip leads the title: {chip:?} before {label:?}"
    );
}

#[gpui::test]
fn a_tap_on_the_header_chip_raises_the_sheet(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = titled_shell(
        cx,
        &dir,
        &[protocol::FEATURE_LINEAR_LINKS],
        "RT-12 Fix the login loop",
    );

    h.click_on(TITLE_CHIP);

    assert_eq!(asked(&mut h), [vec!["RT-12".to_owned()]], "one ask");
    assert!(drawn(&mut h, SHEET), "the tap raises the sheet");
    assert!(in_sheet(&mut h, "linear-card-id-phone=RT-12"));
    assert_eq!(h.opened(), [], "a tap on a chip opens nothing");
}

#[gpui::test]
fn a_title_without_a_team_identifier_has_no_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = titled_shell(cx, &dir, &[protocol::FEATURE_LINEAR_LINKS], "November");

    assert!(!drawn(&mut h, TITLE_CHIP), "no identifier, no chip");
    assert!(drawn(&mut h, "phone-session-label"), "the title alone");
}

#[gpui::test]
fn a_header_chip_tap_on_a_host_without_linear_links_opens_the_link(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = titled_shell(cx, &dir, &[], "RT-12 Fix the login loop");

    assert!(
        drawn(&mut h, TITLE_CHIP),
        "the context alone draws the chip"
    );

    h.click_on(TITLE_CHIP);

    assert_eq!(asked(&mut h), Vec::<Vec<String>>::new(), "no linear_links");
    assert_eq!(
        h.opened(),
        [Opened::Url(ISSUE_WEB.to_owned())],
        "a host that cannot answer opens the link"
    );
    assert!(!drawn(&mut h, SHEET), "and raises no card");
}

/// The selector of the saved host's Home row chip for session `id`.
fn row_chip(id: &str) -> String {
    format!("phone-home-row-chip-{id}")
}

/// The phone on Home, the saved [`REMOTE`] listing one shell session `n`
/// titled `label`, the host listing `features` and [`linear`];
/// everything sent so far is drained.
fn titled_home<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    features: &[&str],
    label: &str,
) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect_with_features(features);
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    let shell = session("n")
        .label(label)
        .in_repo("r1")
        .status("working")
        .no_agent()
        .build();
    host.send(DaemonMessage::Sessions {
        sessions: vec![shell],
    });
    host.send(linear());
    h.host(&saved_remote()).sent();
    h
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

/// The id of the session whose row sheet is open.
fn sheet_session(h: &mut Harness<'_>) -> Option<String> {
    h.cx.update(|window, cx| {
        window
            .root::<PhoneRoot>()
            .flatten()
            .and_then(|phone| phone.read(cx).sheet_session().map(str::to_owned))
    })
}

/// A committed back gesture from Home's leading edge.
fn back_over_home(h: &mut Harness<'_>) {
    let page = h.bounds("phone-home");
    let at = point(page.origin.x + px(4.0), page.origin.y + px(200.0));
    h.back_gesture(BackGesturePhase::Started, at);
    h.back_gesture(BackGesturePhase::Committed, at);
    h.cx.run_until_parked();
}

#[gpui::test]
fn a_home_row_draws_the_titles_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = titled_home(
        cx,
        &dir,
        &[protocol::FEATURE_LINEAR_LINKS],
        "RT-12 Fix the login loop",
    );

    assert!(drawn(&mut h, &row_chip("n")), "the row draws the chip");
    let (chip, line) = (h.bounds(&row_chip("n")), h.bounds(&row("n")));
    assert!(line.contains(&chip.center()), "the chip sits on the row");
    assert!(
        chip.left() < line.center().x,
        "the chip leads the title: {chip:?} in {line:?}"
    );
}

#[gpui::test]
fn a_tap_on_the_row_chip_raises_the_sheet_and_does_not_open_the_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = titled_home(
        cx,
        &dir,
        &[protocol::FEATURE_LINEAR_LINKS],
        "RT-12 Fix the login loop",
    );

    h.tap_on(&row_chip("n"));

    assert_eq!(shown(&mut h), None, "a tap on the chip opens no session");
    assert_eq!(asked(&mut h), [vec!["RT-12".to_owned()]], "one ask");
    assert!(drawn(&mut h, SHEET), "the tap raises the sheet");
    assert!(in_sheet(&mut h, "linear-card-id-phone=RT-12"));
    assert_eq!(h.opened(), [], "a tap on a chip opens nothing");
}

#[gpui::test]
fn a_tap_beside_the_chip_still_opens_the_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = titled_home(
        cx,
        &dir,
        &[protocol::FEATURE_LINEAR_LINKS],
        "RT-12 Fix the login loop",
    );
    let line = h.bounds(&row("n"));
    let at = point(line.left() + line.size.width * 0.75, line.center().y);

    h.touch(TouchPhase::Started, at);
    h.touch(TouchPhase::Ended, at);

    assert_eq!(
        shown(&mut h).as_deref(),
        Some("n"),
        "a tap beside the chip opens the session"
    );
    assert!(!drawn(&mut h, SHEET), "and raises no card");
}

#[gpui::test]
fn a_long_press_on_the_row_chip_still_opens_the_row_sheet(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = titled_home(
        cx,
        &dir,
        &[protocol::FEATURE_LINEAR_LINKS],
        "RT-12 Fix the login loop",
    );

    h.long_press_on(&row_chip("n"));

    assert_eq!(
        sheet_session(&mut h).as_deref(),
        Some("n"),
        "the hold opens the row sheet"
    );
    assert_eq!(shown(&mut h), None, "and opens no session");
    assert!(!drawn(&mut h, SHEET), "and raises no card");
}

#[gpui::test]
fn back_takes_the_home_card_down(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = titled_home(
        cx,
        &dir,
        &[protocol::FEATURE_LINEAR_LINKS],
        "RT-12 Fix the login loop",
    );
    h.tap_on(&row_chip("n"));
    assert!(drawn(&mut h, SHEET), "the chip raises the card");

    back_over_home(&mut h);

    assert!(!drawn(&mut h, SHEET), "the card is down");
    assert!(drawn(&mut h, "phone-home"), "Home is unchanged");
    assert_eq!(shown(&mut h), None, "and no session opened");
}

#[gpui::test]
fn the_home_card_sheet_clears_the_bottom_safe_area(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = titled_home(
        cx,
        &dir,
        &[protocol::FEATURE_LINEAR_LINKS],
        "RT-12 Fix the login loop",
    );
    // The harness's window reports no insets: draw a phone's 62 pt top one
    // and the home indicator's 34 pt band at the bottom.
    let (top, indicator, pad) = (62.0, 34.0, 16.0);
    let insets = WindowInsets {
        safe_area: Edges {
            top: px(top),
            bottom: px(indicator),
            ..Edges::default()
        },
        ..WindowInsets::default()
    };
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.update(cx, |phone, cx| phone.set_insets_override(Some(insets), cx));
    });
    h.cx.run_until_parked();

    h.tap_on(&row_chip("n"));
    assert!(drawn(&mut h, SHEET), "the chip raises the card");

    let open = h.bounds("phone-linear-open");
    let window_bottom = px(h.window_height());
    assert!(
        open.bottom() + px(indicator + pad) <= window_bottom + px(1.0),
        "Open in Linear clears the home indicator's band: {open:?} in a window {window_bottom:?} tall"
    );
}

#[gpui::test]
fn a_title_without_a_team_identifier_has_no_row_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = titled_home(cx, &dir, &[protocol::FEATURE_LINEAR_LINKS], "November");

    assert!(!drawn(&mut h, &row_chip("n")), "no identifier, no chip");
    assert!(drawn(&mut h, &row("n")), "the row alone");
}

#[gpui::test]
fn a_chat_identifier_links_once_the_linear_context_arrives(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_page(
        cx,
        &dir,
        &[protocol::FEATURE_LINEAR_LINKS],
        "RT-12 now",
        None,
    );

    tap_answer(&mut h, 0..5);
    assert_eq!(asked(&mut h), Vec::<Vec<String>>::new(), "no link yet");
    assert!(!drawn(&mut h, SHEET), "and no card rises");

    h.host(&saved_remote()).send(linear());
    h.cx.run_until_parked();

    tap_answer(&mut h, 0..5);

    assert_eq!(
        asked(&mut h),
        [vec!["RT-12".to_owned()]],
        "the identifier links once the context arrives"
    );
    assert!(drawn(&mut h, SHEET), "and raises the sheet");
}
