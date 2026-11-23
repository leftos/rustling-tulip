//! The iPhone on its side: a session's Terminal page fills the screen with
//! its terminal and key bar and no header, the key bar carrying Back and the
//! session's status. Turning a Chat page on its side shows the terminal and
//! turning back restores the Chat page; the Terminal toggle in the two
//! panes opens the full-screen terminal and Back returns to the panes on
//! the Chat page; a page the user chose stays through the turns. The iPad
//! keeps the toggle switching the session pane's page.

use std::sync::Arc;

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{BackGesturePhase, Bounds, Edges, Pixels, TestAppContext, WindowInsets, point, px};
use protocol::{
    ChatEntry, ChatOption, ChatPrompt, ChatPromptKind, ChatScreen, ClientMessage, ComposerState,
    DaemonMessage, SessionSnapshot,
};
use rustling_tulip_native::phone::push::FakePushPlatform;
use rustling_tulip_native::phone::{PhoneLayout, PhoneRoot};
use rustling_tulip_native::target::HostKey;
use serde_json::json;
use support::{Harness, REMOTE, TestDir, repo, saved_remote, saved_ui, seed_ui, session};

/// The selector of the saved host's row for session `id`.
fn row(id: &str) -> String {
    let wire = HostKey::of(&saved_remote())
        .wire()
        .expect("a tailnet host has a wire key")
        .to_owned();
    format!("phone-home-row-{wire}::{id}")
}

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// Reads the phone's root.
fn phone<R>(h: &mut Harness<'_>, read: impl FnOnce(&PhoneRoot, &gpui::App) -> R) -> R {
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        read(phone.read(cx), cx)
    })
}

/// Runs `change` on the phone's root, then lets the window settle.
fn phone_update<R>(
    h: &mut Harness<'_>,
    change: impl FnOnce(&mut PhoneRoot, &mut gpui::Context<PhoneRoot>) -> R,
) -> R {
    let out = h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.update(cx, change)
    });
    h.cx.run_until_parked();
    out
}

/// The id of the selected session.
fn shown(h: &mut Harness<'_>) -> Option<String> {
    phone(h, |phone, _| {
        phone.shown_session().map(|(_, id)| id.to_owned())
    })
}

/// The Terminal toggle's label, while it shows.
fn toggle(h: &mut Harness<'_>) -> Option<&'static str> {
    phone(h, PhoneRoot::session_toggle)
}

/// Resizes the window to `width` by `height` points.
fn resize(h: &mut Harness<'_>, width: f32, height: f32) {
    h.cx.simulate_resize(gpui::size(px(width), px(height)));
    h.cx.run_until_parked();
}

/// Draws `insets` around the content, or the window's own when `None`.
fn set_insets(h: &mut Harness<'_>, insets: Option<WindowInsets>) {
    phone_update(h, |phone, cx| phone.set_insets_override(insets, cx));
}

/// An iPhone's insets on its side: the home indicator's band at the bottom
/// and the camera's on each side.
fn side_insets() -> WindowInsets {
    WindowInsets {
        safe_area: Edges {
            top: px(0.0),
            right: px(62.0),
            bottom: px(21.0),
            left: px(62.0),
        },
        ..WindowInsets::default()
    }
}

/// Turns the iPhone on its side: 874 by 402 inside its landscape insets.
fn turn_on_side(h: &mut Harness<'_>) {
    resize(h, 874.0, 402.0);
    set_insets(h, Some(side_insets()));
}

/// Turns the iPhone back upright: 402 by 874 inside the window's insets.
fn turn_upright(h: &mut Harness<'_>) {
    resize(h, 402.0, 874.0);
    set_insets(h, None);
}

/// `a` and `b` working with mirrored chats, `e` stopped, all in r1.
fn listed() -> Vec<SessionSnapshot> {
    vec![
        session("a")
            .label("Alpha")
            .in_repo("r1")
            .status("working")
            .build(),
        session("b")
            .label("Bravo")
            .in_repo("r1")
            .status("working")
            .build(),
        session("e")
            .label("Echo")
            .in_repo("r1")
            .status("stopped")
            .build(),
    ]
}

/// A phone with the saved [`REMOTE`] open, listing [`listed`], its push
/// through `platform`.
fn open_phone_with<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    platform: Arc<FakePushPlatform>,
) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone_with(cx, dir, platform);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect_with_features(&[]);
    host.sent();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    host.send(DaemonMessage::Sessions { sessions: listed() });
    h
}

/// [`open_phone_with`] upright in an iPhone's 402 by 874 window.
fn upright<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    upright_with(cx, dir, Arc::new(FakePushPlatform::default()))
}

/// [`upright`] with its push through `platform`.
fn upright_with<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    platform: Arc<FakePushPlatform>,
) -> Harness<'a> {
    let mut h = open_phone_with(cx, dir, platform);
    resize(&mut h, 402.0, 874.0);
    h
}

/// [`open_phone_with`] on its side.
fn on_side<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = open_phone_with(cx, dir, Arc::new(FakePushPlatform::default()));
    turn_on_side(&mut h);
    h
}

/// Whether `inner` lies within `outer`, to the pixel.
fn within(inner: Bounds<Pixels>, outer: Bounds<Pixels>) -> bool {
    let slack = px(0.5);
    inner.origin.x + slack >= outer.origin.x
        && inner.origin.y + slack >= outer.origin.y
        && inner.bottom_right().x <= outer.bottom_right().x + slack
        && inner.bottom_right().y <= outer.bottom_right().y + slack
}

/// A committed back gesture from the window's leading edge.
fn back_gesture(h: &mut Harness<'_>) {
    let at = point(px(4.0), px(200.0));
    for phase in [
        BackGesturePhase::Started,
        BackGesturePhase::Progressed,
        BackGesturePhase::Committed,
    ] {
        h.back_gesture(phase, at);
    }
}

/// Asserts the shown session's terminal fills the screen: no list pane, no
/// header, the key bar carrying Back and the status.
fn assert_full(h: &mut Harness<'_>) {
    assert!(drawn(h, "phone-session-terminal"), "the terminal shows");
    assert!(!drawn(h, "phone-list-pane"), "no list pane");
    assert!(!drawn(h, "phone-home"), "no Home list");
    assert!(!drawn(h, "phone-session-toggle"), "no header toggle");
    assert!(!drawn(h, "phone-session-label"), "no header label");
    let bar = h.bounds("phone-key-bar");
    assert!(drawn(h, "phone-bar-back"), "the key bar carries Back");
    assert!(within(h.bounds("phone-bar-back"), bar), "Back in the bar");
    assert!(drawn(h, "phone-session-status"), "and the status");
    assert!(
        within(h.bounds("phone-session-status"), bar),
        "the status in the bar"
    );
}

#[gpui::test]
fn the_landscape_status_has_room_to_read(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = upright(cx, &dir);
    h.click_on(&row("a"));
    turn_on_side(&mut h);
    assert_full(&mut h);

    let status = h.bounds("phone-session-status");
    let back = h.bounds("phone-bar-back");
    let scroll = h.bounds("phone-key-bar-scroll");
    assert!(
        status.size.width >= px(56.0),
        "the status is at least 56 pt wide: {status:?}"
    );
    assert!(
        status.left() >= back.right(),
        "the status clears Back: {status:?}, Back {back:?}"
    );
    assert!(
        scroll.left() >= status.right(),
        "the status clears the keys: {status:?}, the row {scroll:?}"
    );
}

#[gpui::test]
fn landscape_bar_back_with_the_quick_dock_open_closes_the_dock(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = upright(cx, &dir);
    h.click_on(&row("a"));
    h.click_on("phone-session-toggle");
    turn_on_side(&mut h);
    assert_full(&mut h);
    h.click_on("phone-bar-quick");
    assert!(drawn(&mut h, "phone-terminal-quick"), "the dock opens");

    h.click_on("phone-bar-back");

    assert!(
        !drawn(&mut h, "phone-terminal-quick"),
        "Back closes the dock"
    );
    assert_full(&mut h);

    h.click_on("phone-bar-back");

    assert_panes_on_chat(&mut h);
}

/// Asserts the two panes show with the selected session on its Chat page.
fn assert_panes_on_chat(h: &mut Harness<'_>) {
    assert!(drawn(h, "phone-list-pane"), "the list pane shows");
    assert!(drawn(h, "phone-session-chat"), "the Chat page shows");
    assert!(!drawn(h, "phone-bar-back"), "no key bar Back");
    assert_eq!(toggle(h), Some("Terminal"), "on the Chat page");
}

#[gpui::test]
fn turning_a_chat_page_on_its_side_fills_the_screen_and_turning_back_restores_it(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = upright(cx, &dir);
    h.click_on(&row("a"));
    assert_eq!(toggle(&mut h), Some("Terminal"), "on the Chat page");

    turn_on_side(&mut h);

    assert_eq!(
        phone(&mut h, |phone, _| phone.layout()),
        PhoneLayout::TerminalFull
    );
    assert_full(&mut h);
    assert_eq!(
        phone(&mut h, PhoneRoot::session_status),
        Some("Working"),
        "the status is the session's"
    );

    turn_upright(&mut h);

    assert_eq!(toggle(&mut h), Some("Terminal"), "the Chat page again");
    assert!(drawn(&mut h, "phone-session-chat"));
    assert!(drawn(&mut h, "phone-session-label"), "the header returns");
    assert!(!drawn(&mut h, "phone-bar-back"));
}

#[gpui::test]
fn the_toggle_on_its_side_fills_the_screen_and_back_returns_to_the_panes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_side(cx, &dir);
    h.click_on(&row("a"));
    assert_panes_on_chat(&mut h);

    h.click_on("phone-session-toggle");

    assert_full(&mut h);

    h.click_on("phone-bar-back");

    assert_eq!(shown(&mut h).as_deref(), Some("a"), "the selection stays");
    assert_panes_on_chat(&mut h);
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
fn no_prompt_sheet_over_the_full_screen_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = upright(cx, &dir);
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
    h.cx.run_until_parked();
    h.click_on("phone-session-toggle");
    assert!(drawn(&mut h, "phone-session-terminal"));

    turn_on_side(&mut h);

    assert_full(&mut h);

    h.host(&saved_remote())
        .send(DaemonMessage::ChatScreenChanged {
            session_id: "a".to_owned(),
            screen: ChatScreen {
                composer: ComposerState::Dialog,
                prompt: Some(question()),
            },
        });
    h.cx.run_until_parked();

    assert!(
        !drawn(&mut h, "phone-prompt-sheet"),
        "no sheet over the full-screen terminal"
    );

    turn_upright(&mut h);
    h.click_on("phone-session-toggle");

    assert!(
        drawn(&mut h, "phone-prompt-sheet"),
        "the Chat page's sheet waits"
    );
}

#[gpui::test]
fn a_terminal_chosen_upright_stays_through_the_turns(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = upright(cx, &dir);
    h.click_on(&row("a"));
    h.click_on("phone-session-toggle");
    assert_eq!(toggle(&mut h), Some("Chat"), "on the Terminal page");

    turn_on_side(&mut h);

    assert_full(&mut h);

    turn_upright(&mut h);

    assert_eq!(toggle(&mut h), Some("Chat"), "the Terminal page stays");
    assert!(drawn(&mut h, "phone-session-terminal"));
    assert!(drawn(&mut h, "phone-session-label"), "under its header");
}

#[gpui::test]
fn the_ipads_toggle_switches_the_session_panes_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone_with(cx, &dir, Arc::new(FakePushPlatform::default()));
    resize(&mut h, 1180.0, 820.0);
    h.click_on(&row("a"));

    h.click_on("phone-session-toggle");

    assert_eq!(
        phone(&mut h, |phone, _| phone.layout()),
        PhoneLayout::TwoPane
    );
    assert_eq!(toggle(&mut h), Some("Chat"), "the Terminal page shows");
    assert!(drawn(&mut h, "phone-session-terminal"));
    assert!(drawn(&mut h, "phone-list-pane"), "beside the list");
    assert!(drawn(&mut h, "phone-session-toggle"), "under its header");
    assert!(!drawn(&mut h, "phone-bar-back"), "no key bar Back");
}

#[gpui::test]
fn settings_cover_the_full_terminal_and_back_returns_to_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = upright(cx, &dir);
    h.click_on(&row("a"));
    turn_on_side(&mut h);
    assert_full(&mut h);

    phone_update(&mut h, PhoneRoot::open_settings);

    assert!(drawn(&mut h, "phone-settings"), "Settings shows");
    assert!(
        !drawn(&mut h, "phone-session-terminal"),
        "over the terminal"
    );

    back_gesture(&mut h);

    assert!(!drawn(&mut h, "phone-settings"), "Settings leaves");
    assert_full(&mut h);
}

#[gpui::test]
fn a_turn_under_settings_keeps_the_focused_field(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = upright(cx, &dir);
    h.click_on(&row("a"));
    phone_update(&mut h, PhoneRoot::open_settings_replies);
    h.click_on("phone-replies-agent-add-group");
    h.keys("m i");

    turn_on_side(&mut h);
    h.keys("n");
    h.click_on("phone-edit-save");

    let saved = saved_ui(&dir)
        .get("phone_agent_replies")
        .map(ToString::to_string)
        .unwrap_or_default();
    assert!(
        saved.contains("\"min\""),
        "the key after the turn lands in the field: {saved}"
    );
}

#[gpui::test]
fn a_toggle_on_its_side_wins_over_the_turn_back(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = upright(cx, &dir);
    h.click_on(&row("a"));
    turn_on_side(&mut h);
    assert_full(&mut h);
    h.click_on("phone-bar-back");
    assert_panes_on_chat(&mut h);

    h.click_on("phone-session-toggle");
    assert_full(&mut h);
    turn_upright(&mut h);

    assert_eq!(toggle(&mut h), Some("Chat"), "the Terminal page stays");
    assert!(drawn(&mut h, "phone-session-terminal"));
}

#[gpui::test]
fn a_session_opened_on_its_side_shows_on_chat_in_the_panes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = upright_with(cx, &dir, platform.clone());
    h.click_on(&row("a"));
    turn_on_side(&mut h);
    assert_full(&mut h);

    platform.tap(REMOTE, "b");
    h.cx.run_until_parked();

    assert_eq!(shown(&mut h).as_deref(), Some("b"));
    assert_panes_on_chat(&mut h);

    turn_upright(&mut h);

    assert_eq!(shown(&mut h).as_deref(), Some("b"));
    assert_eq!(toggle(&mut h), Some("Terminal"), "still on the Chat page");
}

/// [`listed`] with `a` waiting on a question of three described options.
fn waiting() -> Vec<SessionSnapshot> {
    let mut sessions = listed();
    sessions[0] = session("a")
        .label("Alpha")
        .in_repo("r1")
        .status("awaiting_input")
        .pending_input(json!({
            "kind": "question",
            "questions": [{
                "question": "Which branch?",
                "header": "Migration",
                "options": ["main", "dev", "next"],
                "option_descriptions": [
                    "the trunk",
                    "the feature line",
                    "the next release's branch",
                ],
                "multi_select": false,
            }],
        }))
        .build();
    sessions
}

#[gpui::test]
fn the_tray_card_leaves_room_for_home_in_landscape(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = on_side(cx, &dir);
    h.host(&saved_remote()).send(DaemonMessage::Sessions {
        sessions: waiting(),
    });
    h.cx.run_until_parked();

    assert!(drawn(&mut h, "phone-tray-card"), "the card shows");
    let pane = h.bounds("phone-list-pane");
    let card = h.bounds("phone-tray-card");
    assert!(
        card.size.height <= pane.size.height * 0.55,
        "the card {card:?} leaves the landscape list pane {pane:?} room"
    );
}

#[gpui::test]
fn an_ended_session_stays_on_chat_on_its_side(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = upright(cx, &dir);
    h.click_on(&row("e"));
    assert!(drawn(&mut h, "phone-session-chat"), "on its Chat page");

    turn_on_side(&mut h);

    assert!(drawn(&mut h, "phone-list-pane"), "the two panes show");
    assert!(drawn(&mut h, "phone-session-chat"), "the Chat page stays");
    assert!(!drawn(&mut h, "phone-session-terminal"));
    assert!(!drawn(&mut h, "phone-bar-back"));
}
