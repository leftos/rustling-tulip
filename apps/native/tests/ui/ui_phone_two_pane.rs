//! The phone's two panes: on a window wide enough (the iPad either way, an
//! iPhone on its side) Home's list docks in a 320 pt pane on the left and
//! the selected session's page fills the pane beside it, which says "Pick a
//! session" while none is selected. Settings covers the whole window, the
//! session header has no Back, and the back gesture's last step clears the
//! selection. The prompt sheet and the browser's folder sheet cover the
//! session pane only, and Home's long-press sheet the list pane only.

use std::sync::Arc;

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{BackGesturePhase, Bounds, Edges, Pixels, TestAppContext, WindowInsets, point, px};
use protocol::{
    ChatEntry, ChatOption, ChatPrompt, ChatPromptKind, ChatScreen, ClientMessage, ComposerState,
    DaemonMessage, DirEntry, FEATURE_HOST_BROWSE, FEATURE_HOST_PATHS, HostOsKind, PathKind,
    SessionSnapshot,
};
use rustling_tulip_native::PhoneFetched;
use rustling_tulip_native::phone::push::FakePushPlatform;
use rustling_tulip_native::phone::{PhoneLayout, PhoneRoot};
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, NetEvent};
use serde_json::json;
use support::{Harness, REMOTE, TestDir, repo, saved_remote, seed_ui, session};

/// The list pane's width, in points.
const LIST_WIDTH: f32 = 320.0;

/// The selector of the saved host's row for session `id`.
fn row(id: &str) -> String {
    format!("phone-home-row-{}", leaf(id))
}

/// The selected mark on the saved host's row for session `id`.
fn selected_mark(id: &str) -> String {
    format!("phone-home-row-selected-{}", leaf(id))
}

/// The saved host's key for session `id`, as row selectors carry it.
fn leaf(id: &str) -> String {
    let wire = HostKey::of(&saved_remote())
        .wire()
        .expect("a tailnet host has a wire key")
        .to_owned();
    format!("{wire}::{id}")
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

/// Resizes the window to `width` by `height` points.
fn resize(h: &mut Harness<'_>, width: f32, height: f32) {
    h.cx.simulate_resize(gpui::size(px(width), px(height)));
    h.cx.run_until_parked();
}

/// Draws `insets` (top, right, bottom, left) around the content.
fn set_insets(h: &mut Harness<'_>, top: f32, right: f32, bottom: f32, left: f32) {
    let insets = WindowInsets {
        safe_area: Edges {
            top: px(top),
            right: px(right),
            bottom: px(bottom),
            left: px(left),
        },
        ..WindowInsets::default()
    };
    phone_update(h, |phone, cx| phone.set_insets_override(Some(insets), cx));
}

/// `a` working with a mirrored chat, `e` stopped, both in r1.
fn listed() -> Vec<SessionSnapshot> {
    vec![
        session("a")
            .label("Alpha")
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

/// A phone-sized phone with the saved [`REMOTE`] open, listing [`listed`],
/// its push through `platform`.
fn open_phone_with<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    platform: Arc<FakePushPlatform>,
    features: &[&str],
) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone_with(cx, dir, platform);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect_with_features(features);
    host.sent();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    host.send(DaemonMessage::Sessions { sessions: listed() });
    h
}

/// [`open_phone_with`] in an iPad's landscape window, 1180 by 820.
fn ipad<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    ipad_with(cx, dir, Arc::new(FakePushPlatform::default()))
}

/// [`ipad`] with its push through `platform`.
fn ipad_with<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    platform: Arc<FakePushPlatform>,
) -> Harness<'a> {
    let mut h = open_phone_with(cx, dir, platform, &[]);
    resize(&mut h, 1180.0, 820.0);
    h
}

/// The list pane's bounds.
fn list_pane(h: &mut Harness<'_>) -> Bounds<Pixels> {
    h.bounds("phone-list-pane")
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
    let at = point(px(4.0), px(400.0));
    for phase in [
        BackGesturePhase::Started,
        BackGesturePhase::Progressed,
        BackGesturePhase::Committed,
    ] {
        h.back_gesture(phase, at);
    }
}

#[gpui::test]
fn two_panes_show_the_list_beside_the_empty_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = ipad(cx, &dir);

    assert_eq!(
        phone(&mut h, |phone, _| phone.layout()),
        PhoneLayout::TwoPane
    );
    assert!(drawn(&mut h, "phone-home"), "Home's list shows");
    assert_eq!(list_pane(&mut h).size.width, px(LIST_WIDTH));
    assert!(
        drawn(&mut h, "phone-pane-empty"),
        "the session pane is empty"
    );
    assert!(
        h.bounds("phone-pane-empty").origin.x >= px(LIST_WIDTH),
        "the empty pane lies right of the list"
    );
    assert_eq!(shown(&mut h), None);
}

#[gpui::test]
fn a_row_tap_shows_the_session_beside_the_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = ipad(cx, &dir);

    h.click_on(&row("a"));

    assert_eq!(shown(&mut h).as_deref(), Some("a"));
    assert!(drawn(&mut h, "phone-session"), "the session page shows");
    assert!(drawn(&mut h, "phone-home"), "Home's list stays");
    assert!(!drawn(&mut h, "phone-pane-empty"), "the empty state leaves");
    assert!(
        h.bounds("phone-session").origin.x >= list_pane(&mut h).bottom_right().x,
        "the page lies in the session pane"
    );
    assert!(
        drawn(&mut h, &selected_mark("a")),
        "the selected row is marked"
    );
    assert!(!drawn(&mut h, &selected_mark("e")), "no other row is");
}

#[gpui::test]
fn a_tap_on_the_selected_row_changes_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = ipad(cx, &dir);
    h.click_on(&row("a"));
    h.click_on("phone-session-toggle");
    let toggle = phone(&mut h, PhoneRoot::session_toggle);
    assert_eq!(toggle, Some("Chat"), "the Terminal page shows");
    h.host(&saved_remote()).sent();

    h.click_on(&row("a"));

    assert_eq!(shown(&mut h).as_deref(), Some("a"));
    assert_eq!(
        phone(&mut h, PhoneRoot::session_toggle),
        Some("Chat"),
        "the page stays on Terminal"
    );
    let loads: Vec<ClientMessage> = h
        .host(&saved_remote())
        .sent()
        .into_iter()
        .filter(|msg| matches!(msg, ClientMessage::LoadChat { .. }))
        .collect();
    assert!(loads.is_empty(), "no chat reloads: {loads:?}");
}

#[gpui::test]
fn settings_cover_the_window_and_back_returns_to_the_panes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = ipad(cx, &dir);
    h.click_on(&row("a"));

    h.click_on("phone-settings-open");

    assert!(drawn(&mut h, "phone-settings"), "Settings shows");
    assert!(
        h.bounds("phone-settings").size.width > px(LIST_WIDTH * 2.0),
        "over the whole window"
    );
    assert!(!drawn(&mut h, "phone-home"), "the list is covered");
    assert!(!drawn(&mut h, "phone-session"), "and the session");

    back_gesture(&mut h);

    assert!(!drawn(&mut h, "phone-settings"), "Settings leaves");
    assert!(drawn(&mut h, "phone-home"));
    assert!(drawn(&mut h, "phone-session"), "the session still shows");
    assert_eq!(shown(&mut h).as_deref(), Some("a"), "the selection is kept");
}

#[gpui::test]
fn the_session_header_has_no_back_in_two_panes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = ipad(cx, &dir);

    h.click_on(&row("a"));

    assert!(drawn(&mut h, "phone-session-label"));
    assert!(!drawn(&mut h, "phone-session-back"));
}

#[gpui::test]
fn a_back_gesture_on_the_chat_page_clears_the_selection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = ipad(cx, &dir);
    h.click_on(&row("a"));

    back_gesture(&mut h);

    assert_eq!(shown(&mut h), None);
    assert!(drawn(&mut h, "phone-pane-empty"), "the pane empties");
    assert!(drawn(&mut h, "phone-home"), "the list stays");
    assert!(!drawn(&mut h, &selected_mark("a")), "no row is marked");
}

#[gpui::test]
fn a_notification_tap_selects_its_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = ipad_with(cx, &dir, platform.clone());

    platform.tap(REMOTE, "e");
    h.cx.run_until_parked();

    assert_eq!(shown(&mut h).as_deref(), Some("e"));
    assert!(drawn(&mut h, "phone-session"));
    assert!(drawn(&mut h, "phone-home"), "the list stays");
}

#[gpui::test]
fn a_notification_note_keeps_the_selection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = ipad_with(cx, &dir, platform.clone());
    h.click_on(&row("a"));

    platform.tap(REMOTE, "gone");
    h.cx.run_until_parked();

    assert_eq!(shown(&mut h).as_deref(), Some("a"), "the session stays");
    assert!(drawn(&mut h, "phone-session"));
    assert!(
        drawn(&mut h, "phone-open-note"),
        "the line shows in the list"
    );
}

#[gpui::test]
fn an_iphone_on_its_side_shows_two_panes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone_with(cx, &dir, Arc::new(FakePushPlatform::default()), &[]);
    resize(&mut h, 874.0, 402.0);
    set_insets(&mut h, 0.0, 62.0, 21.0, 62.0);

    assert_eq!(
        phone(&mut h, |phone, _| phone.layout()),
        PhoneLayout::TerminalFull
    );
    assert!(drawn(&mut h, "phone-home"));
    assert!(drawn(&mut h, "phone-pane-empty"));
    assert_eq!(
        list_pane(&mut h).origin.x,
        px(62.0),
        "inside the left inset"
    );

    h.click_on(&row("a"));

    assert!(drawn(&mut h, "phone-session"));
    assert!(drawn(&mut h, "phone-home"));
}

#[gpui::test]
fn a_phone_window_keeps_one_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone_with(cx, &dir, Arc::new(FakePushPlatform::default()), &[]);
    resize(&mut h, 402.0, 874.0);

    assert_eq!(
        phone(&mut h, |phone, _| phone.layout()),
        PhoneLayout::Single
    );
    assert!(!drawn(&mut h, "phone-list-pane"), "no list pane");
    assert!(!drawn(&mut h, "phone-pane-empty"), "no empty pane");
    h.click_on(&row("a"));

    assert!(drawn(&mut h, "phone-session"));
    assert!(!drawn(&mut h, "phone-home"), "the page replaces Home");
    assert!(drawn(&mut h, "phone-session-back"), "with its Back");
    assert!(!drawn(&mut h, &selected_mark("a")));
}

/// The session pane's bounds: right of the list pane and its line, to the
/// window's right edge, its full height.
fn session_pane(h: &mut Harness<'_>) -> Bounds<Pixels> {
    let list = list_pane(h);
    let window = h.cx.update(|window, _| window.viewport_size());
    let left = list.bottom_right().x + px(1.0);
    Bounds {
        origin: point(left, px(0.0)),
        size: gpui::size(window.width - left, window.height),
    }
}

/// The list pane's bounds from the window's top to its bottom.
fn list_column(h: &mut Harness<'_>) -> Bounds<Pixels> {
    let list = list_pane(h);
    let height = h.cx.update(|window, _| window.viewport_size().height);
    Bounds {
        origin: point(list.origin.x, px(0.0)),
        size: gpui::size(list.size.width, height),
    }
}

/// A question prompt with two numbered options.
fn question() -> ChatPrompt {
    let option = |number: u32, label: &str| ChatOption {
        number,
        label: label.to_owned(),
        description: None,
        selected: false,
    };
    ChatPrompt {
        id: "q1".to_owned(),
        kind: ChatPromptKind::Question,
        subject: "calc.py".to_owned(),
        body: "Which operator?".to_owned(),
        options: vec![option(1, "sub"), option(2, "div")],
        questions: Vec::new(),
        can_amend: true,
        always_allow_tip: None,
        multi: false,
        cursor: None,
        adjust: None,
    }
}

/// Session `a` selected in two panes, its chat loaded and [`question`]
/// pending.
fn prompted<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = ipad(cx, dir);
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
        })
        .expect("selecting the session loads its chat");
    let mut host = h.host(&saved_remote());
    host.send(DaemonMessage::Chat {
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
        request_id: request,
    });
    host.send(DaemonMessage::ChatScreenChanged {
        session_id: "a".to_owned(),
        screen: ChatScreen {
            composer: ComposerState::Dialog,
            prompt: Some(question()),
        },
    });
    h.cx.run_until_parked();
    h
}

/// The id of the first message among what the phone sent the saved host
/// since the last read that `pick` takes.
fn first_sent(h: &mut Harness<'_>, pick: impl FnMut(ClientMessage) -> Option<String>) -> String {
    h.host(&saved_remote())
        .sent()
        .into_iter()
        .find_map(pick)
        .expect("the phone sent the message")
}

/// Opens the browser on the saved host's `/src` in two panes, listing one
/// folder, `docs`.
fn browsing<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = open_phone_with(
        cx,
        dir,
        platform,
        &[FEATURE_HOST_PATHS, FEATURE_HOST_BROWSE],
    );
    resize(&mut h, 1180.0, 820.0);
    h.click_on(&row("a"));
    h.host(&saved_remote()).sent();
    phone_update(&mut h, |phone, cx| {
        phone.open_browser(HostKey::of(&saved_remote()), "/src".to_owned(), cx);
    });
    let info = first_sent(&mut h, |msg| match msg {
        ClientMessage::GetHostInfo { id } => Some(id),
        _ => None,
    });
    h.host(&saved_remote()).send(DaemonMessage::HostInfo {
        id: info,
        os: HostOsKind::Linux,
        home: Some("/home/me".to_owned()),
        roots: vec!["/".to_owned()],
    });
    let listing = first_sent(&mut h, |msg| match msg {
        ClientMessage::ListDir { id, .. } => Some(id),
        _ => None,
    });
    h.host(&saved_remote()).send(DaemonMessage::DirListing {
        id: listing,
        path: "/src".to_owned(),
        entries: vec![DirEntry {
            name: "docs".to_owned(),
            kind: PathKind::Dir,
            size: 0,
            modified: Some(1_700_000_000_000),
            link: false,
            hidden: false,
        }],
        next_cursor: None,
        error: None,
    });
    h
}

#[gpui::test]
fn the_prompt_sheet_covers_the_session_pane_only(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = prompted(cx, &dir);

    assert!(drawn(&mut h, "phone-prompt-sheet"), "the sheet rises");
    let pane = session_pane(&mut h);
    for layer in ["phone-prompt-sheet", "phone-prompt-scrim"] {
        let bounds = h.bounds(layer);
        assert!(within(bounds, pane), "{layer} {bounds:?} lies in {pane:?}");
    }

    h.click_on(&row("e"));

    assert_eq!(shown(&mut h).as_deref(), Some("e"), "the list stays usable");
}

#[gpui::test]
fn the_browsers_folder_sheet_covers_the_session_pane_only(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = browsing(cx, &dir);
    assert!(
        drawn(&mut h, "phone-home"),
        "the list stays beside the browser"
    );

    h.long_press_on("phone-browser-row-docs");

    assert!(drawn(&mut h, "phone-browser-sheet"), "the sheet opens");
    let pane = session_pane(&mut h);
    for layer in ["phone-browser-sheet", "phone-browser-sheet-scrim"] {
        let bounds = h.bounds(layer);
        assert!(within(bounds, pane), "{layer} {bounds:?} lies in {pane:?}");
    }
}

#[gpui::test]
fn homes_sheet_opens_beside_a_selected_session_over_the_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = ipad(cx, &dir);
    h.click_on(&row("a"));

    h.long_press_on(&row("e"));

    let sheet = phone(&mut h, |phone, _| phone.sheet_session().map(str::to_owned));
    assert_eq!(sheet.as_deref(), Some("e"), "the hold opens e's sheet");
    assert_eq!(shown(&mut h).as_deref(), Some("a"), "a stays selected");
    assert!(drawn(&mut h, "phone-session"));
    let column = list_column(&mut h);
    for layer in ["phone-sheet", "phone-sheet-scrim"] {
        let bounds = h.bounds(layer);
        assert!(
            within(bounds, column),
            "{layer} {bounds:?} lies in {column:?}"
        );
    }
}

/// Opens Settings from the list pane, turns two panes into one, and checks
/// Settings is on top: one back gesture shows `page` (tagged by its Back
/// button) again, another the session page.
fn settings_on_top_after_the_turn(h: &mut Harness<'_>, page: &str) {
    assert!(drawn(h, page), "{page} shows in the session pane");
    h.click_on("phone-settings-open");

    resize(h, 402.0, 874.0);

    assert!(drawn(h, "phone-settings"), "Settings shows");
    assert!(!drawn(h, page), "not {page}");

    back_gesture(h);

    assert!(!drawn(h, "phone-settings"), "Settings leaves");
    assert!(drawn(h, page), "{page} shows again");

    back_gesture(h);

    assert!(!drawn(h, page), "{page} leaves");
    assert!(drawn(h, "phone-session"), "the session page shows");
    assert!(!drawn(h, "phone-settings"), "Settings stays closed");
}

#[gpui::test]
fn settings_stay_over_the_browser_when_two_panes_turn_to_one(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = browsing(cx, &dir);

    settings_on_top_after_the_turn(&mut h, "phone-browser-back");
}

#[gpui::test]
fn settings_stay_over_the_viewer_when_two_panes_turn_to_one(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = ipad(cx, &dir);
    let copies = dir.path().join("copies");
    let root = copies.clone();
    h.root_update(move |root_view, _, _| root_view.set_remote_files_root(root));
    h.click_on(&row("a"));
    let saved = PhoneFetched {
        host: REMOTE.to_owned(),
        host_path: "/abs/notes.txt".to_owned(),
        local_path: format!("{REMOTE}/root/abs/notes.txt"),
        size: 5,
        host_label: "box".to_owned(),
    };
    let path = copies.join(&saved.local_path);
    std::fs::create_dir_all(path.parent().expect("a copy has a folder"))
        .expect("make the copy's folder");
    std::fs::write(&path, "text\n").expect("write the copy");
    h.root_update(|root, _, cx| root.phone_file_saved(saved, cx));
    h.cx.run_until_parked();

    settings_on_top_after_the_turn(&mut h, "phone-viewer-back");
}

/// The saved host's connection drops.
fn drop_host(h: &mut Harness<'_>) {
    let target: Target = saved_remote();
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_socket_open(8787);
    let _ = conn.on_closed("dropped".to_owned());
    h.host(&target).event(NetEvent::State(conn));
    h.cx.run_until_parked();
}

#[gpui::test]
fn the_panes_stay_while_the_selected_sessions_host_disconnects(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = ipad(cx, &dir);
    h.click_on(&row("a"));

    drop_host(&mut h);

    assert_eq!(shown(&mut h).as_deref(), Some("a"), "the selection stays");
    assert!(drawn(&mut h, "phone-list-pane"), "the list pane stays");
    assert!(drawn(&mut h, "phone-session"), "the session page stays");
    assert!(drawn(&mut h, "phone-session-label"));
}

#[gpui::test]
fn the_folder_sheet_stays_in_the_session_pane_while_its_host_disconnects(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = browsing(cx, &dir);
    h.long_press_on("phone-browser-row-docs");
    assert!(drawn(&mut h, "phone-browser-sheet"), "the sheet opens");

    drop_host(&mut h);

    assert!(drawn(&mut h, "phone-list-pane"), "the list pane stays");
    let pane = session_pane(&mut h);
    for layer in ["phone-browser-sheet", "phone-browser-sheet-scrim"] {
        let bounds = h.bounds(layer);
        assert!(within(bounds, pane), "{layer} {bounds:?} lies in {pane:?}");
    }
}

#[gpui::test]
fn settings_stay_on_top_when_two_panes_turn_to_one(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = ipad(cx, &dir);
    h.click_on(&row("a"));
    h.click_on("phone-settings-open");

    resize(&mut h, 402.0, 874.0);

    assert!(drawn(&mut h, "phone-settings"), "Settings shows");
    assert!(!drawn(&mut h, "phone-session"), "not the session page");

    back_gesture(&mut h);

    assert!(!drawn(&mut h, "phone-settings"), "Settings leaves");
    assert!(drawn(&mut h, "phone-session"), "the session page shows");
    assert_eq!(shown(&mut h).as_deref(), Some("a"), "the selection is kept");

    back_gesture(&mut h);

    assert_eq!(shown(&mut h), None);
    assert!(drawn(&mut h, "phone-home"), "Home shows");
    assert!(!drawn(&mut h, "phone-settings"), "Settings stays closed");
}

#[gpui::test]
fn the_list_pane_offers_the_sort_button_and_its_choice_applies(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = ipad(cx, &dir);
    // e moves to r2, which the owner dragged above r1: Manual keeps that
    // order, A–Z does not.
    let sessions = vec![
        session("a")
            .label("Alpha")
            .in_repo("r1")
            .status("working")
            .build(),
        session("e")
            .label("Echo")
            .in_repo("r2")
            .status("stopped")
            .build(),
    ];
    let mut host = h.host(&saved_remote());
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1"), repo("r2", "C:/r2")],
    });
    host.send(DaemonMessage::ContainersReordered {
        ordered: vec![
            protocol::ContainerRef::Repo("r2".to_owned()),
            protocol::ContainerRef::Repo("r1".to_owned()),
        ],
    });
    host.send(DaemonMessage::Sessions { sessions });
    h.cx.run_until_parked();
    let pane = list_pane(&mut h);
    assert!(drawn(&mut h, "phone-home-sort"), "the sort button shows");
    assert!(
        within(h.bounds("phone-home-sort"), pane),
        "in the list pane's header"
    );
    assert!(
        h.bounds(&row("e")).origin.y < h.bounds(&row("a")).origin.y,
        "Manual: r2's row above r1's"
    );

    h.click_on("phone-home-sort");
    assert!(
        within(h.bounds("phone-home-sort-sheet"), pane),
        "the sort sheet covers the list pane only"
    );
    h.click_on("phone-home-sort-alphabetical");

    assert!(!drawn(&mut h, "phone-home-sort-sheet"));
    assert!(
        h.bounds(&row("a")).origin.y < h.bounds(&row("e")).origin.y,
        "A–Z puts r1's row above r2's"
    );
}
