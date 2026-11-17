//! The phone's Quick replies page under Settings: a section per set, a row
//! per group, and a group's own page listing its replies. A row opens its
//! page and Back (its button or the gesture) steps out of it, then out of
//! the page, then out of Settings. The page edits what it lists: add,
//! rename, remove and restore groups and replies, saving each change as the
//! whole set in `native-ui.json`.

use std::sync::Arc;

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{BackGesturePhase, Pixels, TestAppContext, point, px};
use protocol::{ChatEntry, ChatScreen, ClientMessage, ComposerState, DaemonMessage};
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::phone::push::FakePushPlatform;
use rustling_tulip_native::target::HostKey;
use serde_json::{Value, json};
use support::{Harness, REMOTE, TestDir, repo, saved_remote, saved_ui, seed_ui, session};

/// The least height of a row or a target.
fn touch_target() -> Pixels {
    px(44.0)
}

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// A phone with [`REMOTE`] saved and paired and `ui`'s fields beside it, no
/// host open: its hosts screen shows with the `Settings` button in the
/// header.
fn phone_saved<'a>(cx: &'a mut TestAppContext, dir: &TestDir, ui: &Value) -> Harness<'a> {
    let mut file = ui.as_object().cloned().unwrap_or_default();
    file.insert(
        "saved_hosts".to_owned(),
        json!([{ "name": REMOTE, "port": 8787 }]),
    );
    seed_ui(dir, &Value::Object(file));
    Harness::phone_with(cx, dir, Arc::new(FakePushPlatform::default()))
}

/// A phone with [`REMOTE`] saved and paired, no host open.
fn phone<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    phone_saved(cx, dir, &json!({}))
}

/// Opens Settings, then its Quick replies page.
fn open_replies(h: &mut Harness<'_>) {
    h.click_on("phone-settings-open");
    h.click_on("phone-settings-row-quick-replies");
}

/// Takes Settings' three Back steps, out of a group page, its page and the
/// list.
fn close_settings(h: &mut Harness<'_>) {
    for _ in 0..3 {
        h.click_on("phone-settings-back");
    }
}

/// A back gesture in `phase` from Settings' leading edge.
fn back_gesture(h: &mut Harness<'_>, phase: BackGesturePhase) {
    let page = h.bounds("phone-settings");
    let at = point(page.origin.x + px(4.0), page.origin.y + px(200.0));
    h.back_gesture(phase, at);
}

/// Types `text` key by key into the focused field.
fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text
        .chars()
        .map(|c| match c {
            ' ' => "space".to_owned(),
            other => other.to_string(),
        })
        .collect();
    h.keys(&keys.join(" "));
}

/// The selector of the saved host's row for session `id`.
fn row(id: &str) -> String {
    let wire = HostKey::of(&saved_remote())
        .wire()
        .expect("a tailnet host has a wire key")
        .to_owned();
    format!("phone-home-row-{wire}::{id}")
}

/// The request ids of the `LoadChat`s for session `id` among `sent`.
fn loads_of(sent: &[ClientMessage], id: &str) -> Vec<Option<String>> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::LoadChat {
                session_id,
                request_id,
            } if session_id == id => Some(request_id.clone()),
            _ => None,
        })
        .collect()
}

/// The daemon's answer to a load of session `id`'s chat: `entries`.
fn chat_reply(id: &str, request_id: Option<String>, entries: Vec<ChatEntry>) -> DaemonMessage {
    DaemonMessage::Chat {
        session_id: id.to_owned(),
        entries,
        hidden: 0,
        screen: ChatScreen {
            composer: ComposerState::Ready,
            prompt: None,
        },
        request_id,
    }
}

/// A phone with [`REMOTE`] open, listing agent session `a`.
fn phone_listing<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = phone(cx, dir);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect_with_features(&[]);
    host.sent();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![
            session("a")
                .label("Alpha")
                .in_repo("r1")
                .status("working")
                .build(),
        ],
    });
    h
}

/// Opens session `a`'s page and answers its chat's load with one message.
fn open_session_a(h: &mut Harness<'_>) {
    h.click_on(&row("a"));
    let sent = h.host(&saved_remote()).sent();
    let request = loads_of(&sent, "a")
        .pop()
        .expect("opening the page loads its chat");
    h.host(&saved_remote())
        .send(chat_reply("a", request, Vec::new()));
}

/// The phone's Quick replies as its dock offers them, flattened by groups.
fn quick_items(h: &mut Harness<'_>) -> Vec<String> {
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.read(cx).quick_reply_items(cx)
    })
}

#[gpui::test]
fn the_quick_replies_row_opens_its_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);

    h.click_on("phone-settings-open");
    let row = "phone-settings-row-quick-replies";
    assert!(drawn(&mut h, row), "the category list carries the row");
    assert!(h.bounds(row).size.height >= touch_target());

    h.click_on(row);

    assert!(drawn(&mut h, "phone-settings"), "Settings shows");
    assert!(
        drawn(&mut h, "phone-replies-agent"),
        "the agent set's section"
    );
    assert!(drawn(&mut h, "phone-replies-shell"), "and the shell's");
    assert!(!drawn(&mut h, row), "the page takes the list's place");
}

#[gpui::test]
fn the_page_lists_each_sets_groups_in_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);

    let confirm = h.bounds("phone-replies-agent-group-0");
    let common = h.bounds("phone-replies-agent-group-1");
    let shell = h.bounds("phone-replies-shell-group-0");
    assert!(confirm.size.height >= touch_target(), "a group row");
    assert!(drawn(&mut h, "phone-replies-agent-groups"));
    assert!(drawn(&mut h, "phone-replies-shell-groups"));
    assert!(
        confirm.top() < common.top(),
        "the agent set's groups keep their order"
    );
    assert!(
        common.top() < shell.top(),
        "the agent set comes over the shell's"
    );
    assert!(
        !drawn(&mut h, "phone-replies-agent-group-2"),
        "the agent set has two groups"
    );
    assert!(
        !drawn(&mut h, "phone-replies-shell-group-1"),
        "the shell set has one"
    );
}

#[gpui::test]
fn a_group_row_opens_its_replies(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);

    h.click_on("phone-replies-agent-group-1");

    assert!(drawn(&mut h, "phone-replies-group-title"), "its Title row");
    assert!(drawn(&mut h, "phone-replies-replies"), "over its replies");
    for reply in 0..5 {
        let row = format!("phone-replies-reply-{reply}");
        assert!(drawn(&mut h, &row), "{row} shows");
        assert!(h.bounds(&row).size.height >= touch_target());
    }
    assert!(
        !drawn(&mut h, "phone-replies-reply-5"),
        "common holds five replies"
    );
    assert!(
        !drawn(&mut h, "phone-replies-agent-group-1"),
        "the group page takes the set's rows away"
    );
}

#[gpui::test]
fn back_from_a_group_page_returns_to_the_page_then_the_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);
    h.click_on("phone-replies-agent-group-0");

    h.click_on("phone-settings-back");
    assert!(
        drawn(&mut h, "phone-replies-agent-group-0"),
        "the group page pops to its set"
    );
    assert!(!drawn(&mut h, "phone-replies-replies"));

    h.click_on("phone-settings-back");
    assert!(
        drawn(&mut h, "phone-settings-row-quick-replies"),
        "the page pops to the list"
    );

    h.click_on("phone-settings-back");
    assert!(!drawn(&mut h, "phone-settings"), "the list closes");
    assert!(
        drawn(&mut h, &format!("phone-host-row-{REMOTE}")),
        "the hosts screen shows again"
    );
}

#[gpui::test]
fn the_back_gesture_steps_out_of_a_group_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);
    h.click_on("phone-replies-agent-group-0");

    back_gesture(&mut h, BackGesturePhase::Committed);
    assert!(
        drawn(&mut h, "phone-replies-agent-group-0"),
        "the gesture pops the group page"
    );
    assert!(!drawn(&mut h, "phone-replies-replies"));

    back_gesture(&mut h, BackGesturePhase::Committed);
    assert!(
        drawn(&mut h, "phone-settings-row-quick-replies"),
        "then the page"
    );

    back_gesture(&mut h, BackGesturePhase::Committed);
    assert!(!drawn(&mut h, "phone-settings"), "then Settings closes");
    assert!(
        drawn(&mut h, &format!("phone-host-row-{REMOTE}")),
        "the hosts screen shows again"
    );
}

#[gpui::test]
fn saved_groups_show_on_the_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({
        "phone_agent_replies": [{ "title": "mine", "replies": ["run the tests"] }],
    });
    let mut h = phone_saved(cx, &dir, &ui);

    open_replies(&mut h);

    assert!(
        drawn(&mut h, "phone-replies-agent-group-0"),
        "the saved group shows"
    );
    assert!(
        !drawn(&mut h, "phone-replies-agent-group-1"),
        "and the defaults do not"
    );
    assert!(
        drawn(&mut h, "phone-replies-shell-group-0"),
        "the other set keeps its default group"
    );

    h.click_on("phone-replies-agent-group-0");
    assert!(drawn(&mut h, "phone-replies-reply-0"), "its one reply");
    assert!(!drawn(&mut h, "phone-replies-reply-1"));
}

#[gpui::test]
fn adding_a_reply_appends_it_and_saves(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);
    h.click_on("phone-replies-agent-group-0");

    h.click_on("phone-replies-add-reply");
    assert!(drawn(&mut h, "phone-edit-field"), "the editor opens");
    type_text(&mut h, "maybe");
    h.click_on("phone-edit-save");

    assert!(
        drawn(&mut h, "phone-replies-reply-2"),
        "the group page shows the new reply"
    );
    assert_eq!(
        saved_ui(&dir)["phone_agent_replies"][0]["replies"][2],
        json!("maybe")
    );
}

#[gpui::test]
fn editing_a_reply_changes_its_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);
    h.click_on("phone-replies-agent-group-1");

    h.click_on("phone-replies-reply-1");
    assert!(drawn(&mut h, "phone-edit-field"), "the editor opens");
    type_text(&mut h, "push it");
    h.click_on("phone-edit-save");

    assert_eq!(
        saved_ui(&dir)["phone_agent_replies"][1]["replies"][1],
        json!("push it")
    );
    assert!(
        drawn(&mut h, "phone-replies-reply-4"),
        "the other replies stay"
    );
}

#[gpui::test]
fn a_blank_reply_is_refused_with_a_message(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);
    h.click_on("phone-replies-agent-group-0");
    h.click_on("phone-replies-add-reply");

    h.click_on("phone-edit-save");

    assert!(drawn(&mut h, "phone-edit-error"), "the refusal shows");
    assert!(
        saved_ui(&dir).get("phone_agent_replies").is_none(),
        "nothing was saved"
    );
}

#[gpui::test]
fn adding_a_group_with_a_taken_title_is_refused(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);

    h.click_on("phone-replies-agent-add-group");
    type_text(&mut h, "common");
    h.click_on("phone-edit-save");

    assert!(drawn(&mut h, "phone-edit-error"), "the refusal shows");
    assert!(
        saved_ui(&dir).get("phone_agent_replies").is_none(),
        "nothing was saved"
    );
}

#[gpui::test]
fn renaming_a_group_renames_its_dock_ids(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone_listing(cx, &dir);
    open_replies(&mut h);
    h.click_on("phone-replies-agent-group-1");

    h.click_on("phone-replies-group-title");
    type_text(&mut h, "often");
    h.click_on("phone-edit-save");

    assert_eq!(
        saved_ui(&dir)["phone_agent_replies"][1]["title"],
        json!("often")
    );

    close_settings(&mut h);
    open_session_a(&mut h);
    h.click_on("phone-composer-quick");

    assert!(
        drawn(&mut h, "phone-quick-often-0"),
        "the dock ids take the new title"
    );
    assert!(!drawn(&mut h, "phone-quick-common-0"));
}

#[gpui::test]
fn removing_a_reply_drops_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);
    h.click_on("phone-replies-agent-group-1");

    h.click_on("phone-replies-reply-0-remove");

    assert_eq!(
        saved_ui(&dir)["phone_agent_replies"][1]["replies"],
        json!(["commit and push", "retry", "skip", "drastically simplify"])
    );
    assert!(!drawn(&mut h, "phone-replies-reply-4"), "one fewer row");
    assert!(
        drawn(&mut h, "phone-replies-replies"),
        "the group page stays"
    );
    assert!(
        !drawn(&mut h, "phone-edit-field"),
        "the row's tap does not open an editor"
    );
}

#[gpui::test]
fn removing_a_group_drops_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);

    h.click_on("phone-replies-agent-group-0-remove");

    assert_eq!(
        saved_ui(&dir)["phone_agent_replies"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        saved_ui(&dir)["phone_agent_replies"][0]["title"],
        json!("common")
    );
    assert!(
        !drawn(&mut h, "phone-replies-agent-group-1"),
        "the page lists one group"
    );
    assert!(
        drawn(&mut h, "phone-replies-agent-groups"),
        "the page stays"
    );
    assert!(
        !drawn(&mut h, "phone-replies-group-title"),
        "the row's tap does not open a group page"
    );
}

#[gpui::test]
fn restore_confirm_is_dropped_on_leaving_the_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_agent_replies": [{ "title": "mine", "replies": ["run the tests"] }] });
    let mut h = phone_saved(cx, &dir, &ui);
    open_replies(&mut h);

    h.click_on("phone-replies-agent-restore");
    assert!(
        drawn(&mut h, "phone-replies-agent-restore-confirm"),
        "Restore asks first"
    );

    h.click_on("phone-settings-back");
    h.click_on("phone-settings-back");
    assert!(!drawn(&mut h, "phone-settings"), "Settings closes");

    open_replies(&mut h);

    assert!(
        !drawn(&mut h, "phone-replies-agent-restore-confirm"),
        "the question does not come back with the page"
    );
    assert_eq!(
        saved_ui(&dir)["phone_agent_replies"][0]["title"],
        json!("mine"),
        "nothing was restored"
    );
}

#[gpui::test]
fn the_first_edit_of_the_defaults_saves_the_whole_set(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);
    h.click_on("phone-replies-agent-group-1");

    h.click_on("phone-replies-reply-0-remove");

    let saved = saved_ui(&dir);
    assert_eq!(
        saved["phone_agent_replies"][0],
        json!({ "title": "confirm", "replies": ["yes", "no"] }),
        "the untouched default group is saved beside the edit"
    );
    assert_eq!(saved["phone_agent_replies"][1]["title"], json!("common"));
}

#[gpui::test]
fn restore_asks_then_brings_back_the_defaults(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_agent_replies": [{ "title": "mine", "replies": ["run the tests"] }] });
    let mut h = phone_saved(cx, &dir, &ui);
    open_replies(&mut h);

    h.click_on("phone-replies-agent-restore");
    assert!(
        drawn(&mut h, "phone-replies-agent-restore-confirm"),
        "Restore asks first"
    );

    h.click_on("phone-replies-agent-restore-confirm");

    assert!(
        saved_ui(&dir).get("phone_agent_replies").is_none(),
        "the field goes: the defaults show again"
    );
    assert!(
        drawn(&mut h, "phone-replies-agent-group-1"),
        "both default groups are back"
    );
}

#[gpui::test]
fn cancel_keeps_the_saved_replies(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = json!({ "phone_agent_replies": [{ "title": "mine", "replies": ["run the tests"] }] });
    let mut h = phone_saved(cx, &dir, &ui);
    open_replies(&mut h);

    h.click_on("phone-replies-agent-restore");
    h.click_on("phone-replies-agent-restore-cancel");

    assert_eq!(
        saved_ui(&dir)["phone_agent_replies"][0]["title"],
        json!("mine")
    );
    assert!(!drawn(&mut h, "phone-replies-agent-restore-confirm"));
}

#[gpui::test]
fn restore_is_dimmed_while_the_defaults_show(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);

    h.click_on("phone-replies-agent-restore");

    assert!(
        !drawn(&mut h, "phone-replies-agent-restore-confirm"),
        "a set already showing the defaults asks nothing"
    );
    assert!(saved_ui(&dir).get("phone_agent_replies").is_none());
}

#[gpui::test]
fn back_from_an_editor_discards_the_draft(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);

    h.click_on("phone-replies-agent-add-group");
    type_text(&mut h, "mine");
    h.click_on("phone-settings-back");

    assert!(
        drawn(&mut h, "phone-replies-agent-groups"),
        "the page is back"
    );
    assert!(
        !drawn(&mut h, "phone-replies-agent-group-2"),
        "no group was added"
    );
    assert!(saved_ui(&dir).get("phone_agent_replies").is_none());
}

#[gpui::test]
fn enter_in_the_field_saves(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone(cx, &dir);
    open_replies(&mut h);
    h.click_on("phone-replies-agent-group-0");
    h.click_on("phone-replies-add-reply");

    type_text(&mut h, "maybe");
    h.keys("enter");

    assert!(!drawn(&mut h, "phone-edit-field"), "the editor closes");
    assert_eq!(
        saved_ui(&dir)["phone_agent_replies"][0]["replies"][2],
        json!("maybe")
    );
}

#[gpui::test]
fn the_dock_shows_an_edited_reply(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = phone_listing(cx, &dir);
    open_replies(&mut h);
    h.click_on("phone-replies-agent-group-1");
    h.click_on("phone-replies-reply-1");
    type_text(&mut h, "push it");
    h.click_on("phone-edit-save");

    close_settings(&mut h);
    open_session_a(&mut h);
    h.click_on("phone-composer-quick");

    assert!(
        drawn(&mut h, "phone-quick-common-1"),
        "the edited reply's slot"
    );
    assert_eq!(
        quick_items(&mut h)[3],
        "push it",
        "the dock offers the new text"
    );
}
