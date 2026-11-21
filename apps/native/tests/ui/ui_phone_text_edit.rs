//! The phone composer's touch editing (`TextInput::with_touch_editing`): a
//! double tap or a long press selects a word, a selection draws two handles
//! a finger drags, a drag from the caret moves it, and the edit bar offers
//! Cut, Copy, Paste and Select All. A desktop text box keeps none of it.

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;

use gpui::{Bounds, Modifiers, Pixels, Point, TestAppContext, TouchPhase, point, px};
use protocol::{
    ChatEntry, ChatScreen, ClientMessage, ComposerState, DaemonMessage, SessionSnapshot,
};
use rustling_tulip_native::keyboard::{KeyboardCall, clear_keyboard_calls, keyboard_calls};
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::target::HostKey;
use serde_json::json;
use support::{Fixture, Harness, REMOTE, TestDir, repo, saved_remote, seed_ui, session};

/// The edit bar's buttons, in the bar's order.
const BAR_BUTTONS: [&str; 4] = [
    "text-edit-cut",
    "text-edit-copy",
    "text-edit-paste",
    "text-edit-select-all",
];

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// The edit bar's buttons the last frame drew, in the bar's order.
fn bar_buttons(h: &mut Harness<'_>) -> Vec<&'static str> {
    BAR_BUTTONS
        .into_iter()
        .filter(|selector| drawn(h, selector))
        .collect()
}

/// The saved host's row selector for session `id`.
fn row(id: &str) -> String {
    let wire = HostKey::of(&saved_remote())
        .wire()
        .expect("a tailnet host has a wire key")
        .to_owned();
    format!("phone-home-row-{wire}::{id}")
}

/// A phone showing session `a`'s Chat page with its chat loaded.
fn chat_page<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect_with_features(&[]);
    host.sent();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    let sessions: Vec<SessionSnapshot> = vec![
        session("a")
            .label("Alpha")
            .in_repo("r1")
            .status("working")
            .build(),
    ];
    host.send(DaemonMessage::Sessions { sessions });
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
        .expect("opening the page loads its chat");
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
        request_id: request,
    });
    h
}

/// Taps the composer and types `text` into it.
fn type_text(h: &mut Harness<'_>, text: &str) {
    h.click_on("phone-composer-field");
    let keys: Vec<String> = text
        .chars()
        .map(|c| {
            if c == ' ' {
                "space".to_owned()
            } else {
                c.to_string()
            }
        })
        .collect();
    h.keys(&keys.join(" "));
}

/// Session `a`'s Chat page with `text` typed into its composer.
fn typed<'a>(cx: &'a mut TestAppContext, dir: &TestDir, text: &str) -> Harness<'a> {
    let mut h = chat_page(cx, dir);
    type_text(&mut h, text);
    h
}

/// The composer's text.
fn text(h: &mut Harness<'_>) -> String {
    h.root(|root, cx| root.chat_composer_text("a", cx))
        .expect("the chat has a composer")
}

/// The composer's selection, its caret when empty.
fn selection(h: &mut Harness<'_>) -> std::ops::Range<usize> {
    h.root(|root, cx| root.chat_composer_selection("a", cx))
        .expect("the chat has a composer")
}

/// The composer's text box.
fn field(h: &mut Harness<'_>) -> Bounds<Pixels> {
    h.bounds("text-edit-field")
}

/// A point on the composer's first row, `x` px from its text's left.
fn on_first_row(h: &mut Harness<'_>, x: f32) -> Point<Pixels> {
    let field = field(h);
    point(field.origin.x + px(x), field.origin.y + px(4.0))
}

/// A point on the first letter of the composer's text.
fn first_letter(h: &mut Harness<'_>) -> Point<Pixels> {
    on_first_row(h, 2.0)
}

/// A point on the composer's first row, near its right edge, past a short
/// text's end.
fn past_the_end(h: &mut Harness<'_>) -> Point<Pixels> {
    let field = field(h);
    point(field.right() - px(2.0), field.origin.y + px(4.0))
}

/// A finger down at `from`, moved through the slop to `to`, and lifted.
fn touch_drag(h: &mut Harness<'_>, from: Point<Pixels>, to: Point<Pixels>) {
    h.touch(TouchPhase::Started, from);
    h.touch(TouchPhase::Moved, to);
    h.touch(TouchPhase::Ended, to);
}

#[gpui::test]
fn a_double_tap_on_a_composer_word_selects_it_and_shows_handles(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed(cx, &dir, "hello world");
    let at = first_letter(&mut h);

    h.double_click(at);

    assert_eq!(selection(&mut h), 0..5, "the word under the tap");
    assert!(drawn(&mut h, "text-edit-handle-start"), "the start handle");
    assert!(drawn(&mut h, "text-edit-handle-end"), "the end handle");
    assert!(!drawn(&mut h, "text-edit-bar"), "a double tap opens no bar");
}

#[gpui::test]
fn a_long_press_on_a_word_selects_it_and_opens_the_edit_bar(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed(cx, &dir, "hello world");
    let at = first_letter(&mut h);

    h.long_press_at(at);

    assert_eq!(selection(&mut h), 0..5);
    assert!(drawn(&mut h, "text-edit-bar"));
    assert_eq!(bar_buttons(&mut h), BAR_BUTTONS);
    let (bar, field) = (h.bounds("text-edit-bar"), field(&mut h));
    assert!(
        bar.bottom() <= field.top(),
        "the bar sits above the selection's row: {bar:?} over {field:?}"
    );
}

#[gpui::test]
fn a_long_press_in_an_empty_composer_offers_only_paste(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = chat_page(cx, &dir);
    let at = h.center("phone-composer-field");

    h.long_press_at(at);

    assert!(drawn(&mut h, "text-edit-bar"));
    assert_eq!(bar_buttons(&mut h), ["text-edit-paste"]);
}

#[gpui::test]
fn a_long_press_past_the_text_places_the_caret_and_offers_paste_and_select_all(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = typed(cx, &dir, "hi");
    let at = past_the_end(&mut h);

    h.long_press_at(at);

    assert_eq!(selection(&mut h), 2..2, "the caret at the text's end");
    assert_eq!(
        bar_buttons(&mut h),
        ["text-edit-paste", "text-edit-select-all"]
    );
    assert!(
        !drawn(&mut h, "text-edit-handle-end"),
        "a caret has no handles"
    );
}

#[gpui::test]
fn copy_then_paste_round_trips_through_the_clipboard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed(cx, &dir, "hello world");
    let word = first_letter(&mut h);
    h.long_press_at(word);

    h.click_on("text-edit-copy");

    assert_eq!(h.clipboard().as_deref(), Some("hello"));
    assert!(!drawn(&mut h, "text-edit-bar"), "Copy closes the bar");
    let end = past_the_end(&mut h);
    h.long_press_at(end);
    h.click_on("text-edit-paste");
    assert_eq!(text(&mut h), "hello worldhello");
    assert!(!drawn(&mut h, "text-edit-bar"), "Paste closes the bar");
}

#[gpui::test]
fn the_edit_bar_paste_reads_no_clipboard_where_the_paste_control_pastes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed(cx, &dir, "hello world");
    h.set_clipboard("pasted");
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.update(cx, |phone, _| phone.set_control_pastes(true));
    });
    let end = past_the_end(&mut h);
    h.long_press_at(end);

    h.click_on("text-edit-paste");

    assert_eq!(
        text(&mut h),
        "hello world",
        "the control, not the bar's item, reads the paste"
    );
    assert!(
        drawn(&mut h, "text-edit-bar"),
        "the bar stays open for the control's own tap"
    );
}

#[gpui::test]
fn cut_removes_the_selected_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed(cx, &dir, "hello world");
    let word = first_letter(&mut h);
    h.long_press_at(word);

    h.click_on("text-edit-cut");

    assert_eq!(text(&mut h), " world");
    assert_eq!(h.clipboard().as_deref(), Some("hello"));
    assert!(!drawn(&mut h, "text-edit-bar"), "Cut closes the bar");
}

#[gpui::test]
fn select_all_selects_everything_and_hides_select_all(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed(cx, &dir, "hello world");
    let word = first_letter(&mut h);
    h.long_press_at(word);

    h.click_on("text-edit-select-all");

    assert_eq!(selection(&mut h), 0..11);
    assert_eq!(
        bar_buttons(&mut h),
        ["text-edit-cut", "text-edit-copy", "text-edit-paste"]
    );
}

#[gpui::test]
fn dragging_the_end_handle_extends_the_selection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed(cx, &dir, "hello world");
    let word = first_letter(&mut h);
    h.double_click(word);
    assert_eq!(selection(&mut h), 0..5);
    let handle = h.center("text-edit-handle-end");
    let end = past_the_end(&mut h);
    // The finger holds the knob below the row: aim it as far below the end.
    let to = point(end.x, handle.y);

    touch_drag(&mut h, handle, to);

    assert_eq!(selection(&mut h), 0..11, "the end follows the finger");
    assert!(drawn(&mut h, "text-edit-handle-end"));
}

#[gpui::test]
fn a_drag_from_the_caret_moves_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed(cx, &dir, "hello world");
    let start = on_first_row(&mut h, 0.0);
    h.click(start, Modifiers::none());
    assert_eq!(selection(&mut h), 0..0, "the caret at the start");
    let end = past_the_end(&mut h);

    let caret = on_first_row(&mut h, 1.0);
    touch_drag(&mut h, caret, end);

    assert_eq!(selection(&mut h), 11..11, "the caret follows the finger");
    assert_eq!(text(&mut h), "hello world", "a caret drag edits nothing");
}

#[gpui::test]
fn a_tap_on_the_caret_raises_the_keyboard_and_opens_the_edit_bar(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed(cx, &dir, "hello world");
    let start = on_first_row(&mut h, 0.0);
    h.click(start, Modifiers::none());
    assert_eq!(selection(&mut h), 0..0, "the caret at the start");
    // Past the double-tap time, so the next tap is a tap of its own.
    let interval = gpui::GestureTuning::default().multi_tap_interval;
    h.advance(interval + std::time::Duration::from_millis(1));
    clear_keyboard_calls();

    let caret = on_first_row(&mut h, 1.0);
    h.touch(TouchPhase::Started, caret);
    h.touch(TouchPhase::Ended, caret);

    assert_eq!(
        keyboard_calls(),
        [KeyboardCall::Raise],
        "the keyboard comes up"
    );
    assert!(drawn(&mut h, "text-edit-bar"), "the bar opens");
    assert_eq!(selection(&mut h), 0..0, "the caret stays");
}

#[gpui::test]
fn a_long_press_on_the_last_word_beside_the_caret_selects_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed(cx, &dir, "hello world");
    // The text's end, where the caret sits: the end handle's stem once all
    // of it is selected.
    let end = past_the_end(&mut h);
    h.long_press_at(end);
    h.click_on("text-edit-select-all");
    let text_end = h.center("text-edit-handle-end").x - px(1.0);
    h.click(end, Modifiers::none());
    assert_eq!(selection(&mut h), 11..11, "the caret at the text's end");
    let first = first_letter(&mut h);
    let on_last_letter = point(text_end - px(3.0), first.y);

    h.long_press_at(on_last_letter);

    assert_eq!(selection(&mut h), 6..11, "the last word");
    assert!(drawn(&mut h, "text-edit-bar"));
}

#[gpui::test]
fn typing_closes_the_edit_bar(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed(cx, &dir, "hello world");
    let word = first_letter(&mut h);
    h.long_press_at(word);
    assert!(drawn(&mut h, "text-edit-bar"));

    h.keys("x");

    assert_eq!(text(&mut h), "x world", "the key replaces the word");
    assert!(!drawn(&mut h, "text-edit-bar"));
}

#[gpui::test]
fn a_tap_elsewhere_in_the_composer_closes_the_edit_bar(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = typed(cx, &dir, "hello world");
    let word = first_letter(&mut h);
    h.long_press_at(word);
    assert!(drawn(&mut h, "text-edit-bar"));

    let end = past_the_end(&mut h);
    h.click(end, Modifiers::none());

    assert!(!drawn(&mut h, "text-edit-bar"));
    assert_eq!(selection(&mut h), 11..11, "the tap places the caret");
}

// ── A desktop text box keeps none of it ────────────────────────────────────

/// A desktop window with `s1`'s chat open in pane `p1`.
fn desktop_chat<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let fixture = Fixture::single(session("s1").build());
    let mut h = Harness::with(cx, dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    h.click_on("pane-view-chat-p1");
    let request = h
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::LoadChat {
                session_id,
                request_id,
            } if session_id == "s1" => request_id,
            _ => None,
        })
        .expect("the toggle asks for the chat");
    h.send(DaemonMessage::Chat {
        session_id: "s1".to_owned(),
        entries: vec![],
        hidden: 0,
        screen: ChatScreen {
            composer: ComposerState::Ready,
            prompt: None,
        },
        request_id: Some(request),
    });
    h.sent();
    h
}

#[gpui::test]
fn a_double_click_in_a_desktop_text_input_opens_no_edit_bar(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = desktop_chat(cx, &dir);
    h.click_on("chat-composer");
    h.keys("h e l l o space w o r l d");
    assert_eq!(
        h.root(|root, cx| root.chat_composer_text("s1", cx))
            .as_deref(),
        Some("hello world")
    );
    let composer = h.bounds("chat-composer");
    let at = point(composer.origin.x + px(16.0), composer.center().y);

    h.double_click(at);

    assert!(!drawn(&mut h, "text-edit-field"), "touch editing is off");
    assert!(!drawn(&mut h, "text-edit-bar"));
    assert!(!drawn(&mut h, "text-edit-handle-start"));
    assert!(!drawn(&mut h, "text-edit-handle-end"));
}
