//! The phone's share inbox: items other apps shared wait in its folder, a
//! sheet asks which session they go to when the app comes to the
//! foreground, and the pick opens that session and, once its chat has
//! loaded, sends files up and puts text and links into its message box,
//! unsent.

use crate::support;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::TestAppContext;
use protocol::{
    ChatEntry, ChatOption, ChatPrompt, ChatPromptKind, ChatScreen, ClientMessage, ComposerState,
    DaemonMessage, FEATURE_UPLOAD_FILE, SessionSnapshot, UPLOAD_MAX_BYTES,
};
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::phone::inbox::{LAST_USED_FILE, LastUsed, UI_FOLDER};
use rustling_tulip_native::target::HostKey;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use support::{Harness, REMOTE, TestDir, repo, saved_remote, seed_ui, session, workspace};

/// The saved host's wire key.
fn wire() -> String {
    HostKey::of(&saved_remote())
        .wire()
        .expect("a tailnet host has a wire key")
        .to_owned()
}

/// The selector of the saved host's Home row for session `id`.
fn row(id: &str) -> String {
    format!("phone-home-row-{}::{id}", wire())
}

/// The share inbox folder the phone reads: the UI folder's.
fn inbox(dir: &TestDir) -> PathBuf {
    let inbox = dir.path().join(UI_FOLDER);
    std::fs::create_dir_all(&inbox).expect("make the share inbox");
    inbox
}

/// Now, in milliseconds since the Unix epoch, as a manifest's `created`.
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis()
}

/// Writes a file item `id` named `name` holding `bytes`.
fn share_file(dir: &TestDir, id: &str, name: &str, bytes: &[u8]) {
    let inbox = inbox(dir);
    std::fs::write(inbox.join(format!("{id}.bin")), bytes).expect("write the payload");
    let manifest = json!({
        "v": 1, "id": id, "kind": "file", "name": name,
        "payload": format!("{id}.bin"), "created": now_ms(),
    });
    std::fs::write(inbox.join(format!("{id}.json")), manifest.to_string())
        .expect("write the manifest");
}

/// Writes a text or link item `id` of `kind` holding `text`, shared at
/// `created`.
fn share_text(dir: &TestDir, id: &str, kind: &str, text: &str, created: u128) {
    let manifest = json!({ "v": 1, "id": id, "kind": kind, "text": text, "created": created });
    std::fs::write(inbox(dir).join(format!("{id}.json")), manifest.to_string())
        .expect("write the manifest");
}

/// Whether the inbox still holds the file `name`.
fn kept(dir: &TestDir, name: &str) -> bool {
    Path::exists(&inbox(dir).join(name))
}

/// `a` and `b` working with a mirrored chat, `e` stopped, `n` with no
/// agent, all in r1.
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
        session("n")
            .label("November")
            .in_repo("r1")
            .status("working")
            .no_agent()
            .build(),
    ]
}

/// A phone with the saved [`REMOTE`] open, listing [`listed`], its daemon
/// listing `features` beside the current daemon's.
fn open_phone<'a>(cx: &'a mut TestAppContext, dir: &TestDir, features: &[&str]) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone(cx, dir);
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

/// The app goes to the background and comes back to the foreground.
fn foreground(h: &mut Harness<'_>) {
    h.cx.simulate_visibility_change(gpui::WindowVisibility::Hidden);
    h.cx.run_until_parked();
    h.cx.simulate_visibility_change(gpui::WindowVisibility::Visible);
    h.cx.run_until_parked();
}

/// Reads the phone's root.
fn phone<R>(h: &mut Harness<'_>, read: impl FnOnce(&PhoneRoot) -> R) -> R {
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        read(phone.read(cx))
    })
}

/// The share sheet's rows' session ids, while it is open.
fn sheet_rows(h: &mut Harness<'_>) -> Option<Vec<String>> {
    phone(h, PhoneRoot::share_sheet_rows)
}

/// The share sheet's index of session `id`'s row.
fn sheet_index(h: &mut Harness<'_>, id: &str) -> usize {
    let rows = sheet_rows(h).expect("the share sheet is open");
    rows.iter()
        .position(|row| row == id)
        .expect("the sheet offers the session")
}

/// The share sheet's row for session `id`.
fn sheet_row(h: &mut Harness<'_>, id: &str) -> String {
    format!("phone-share-row-{}", sheet_index(h, id))
}

/// The detail line under session `id`'s share row.
fn sheet_detail(h: &mut Harness<'_>, id: &str) -> String {
    format!("phone-share-row-detail-{}", sheet_index(h, id))
}

/// The share sheet's rows' detail lines, while it is open.
fn sheet_details(h: &mut Harness<'_>) -> Option<Vec<Option<String>>> {
    phone(h, PhoneRoot::share_sheet_row_details)
}

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > gpui::px(0.0)
}

/// Answers every load of session `id`'s chat the host was sent since the
/// last drain, and returns what else it was sent.
fn answer_loads(h: &mut Harness<'_>, id: &str) -> Vec<ClientMessage> {
    let sent = h.host(&saved_remote()).sent();
    let mut rest = Vec::new();
    for msg in sent {
        match msg {
            ClientMessage::LoadChat {
                session_id,
                request_id,
            } if session_id == id => {
                h.host(&saved_remote()).send(DaemonMessage::Chat {
                    session_id,
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
                    request_id,
                });
            }
            other => rest.push(other),
        }
    }
    rest
}

/// The `UploadFile`s among `sent`: session, name and bytes.
fn uploads_in(sent: &[ClientMessage]) -> Vec<(String, String, Vec<u8>)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::UploadFile {
                session_id,
                name,
                data_b64,
                ..
            } => Some((
                session_id.clone(),
                name.clone(),
                B64.decode(data_b64).expect("upload base64"),
            )),
            _ => None,
        })
        .collect()
}

/// Every toast's title and detail, oldest first.
fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.title.clone(), toast.detail.clone()))
            .collect()
    })
}

/// Session `a`'s composer text.
fn composer_text(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, cx| root.chat_composer_text("a", cx))
}

/// Picks session `id` in the share sheet and answers its chat's load.
fn pick_and_load(h: &mut Harness<'_>, id: &str) -> Vec<ClientMessage> {
    let selector = sheet_row(h, id);
    h.click_on(&selector);
    let shown = phone(h, |phone| {
        phone.shown_session().map(|(_, id)| id.to_owned())
    });
    assert_eq!(shown.as_deref(), Some(id), "the pick opens the session");
    answer_loads(h, id);
    h.cx.run_until_parked();
    h.host(&saved_remote()).sent()
}

#[gpui::test]
fn shared_items_raise_the_sheet_most_recent_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut record = LastUsed::default();
    record.stamp(
        &format!("{}/a", wire()),
        UNIX_EPOCH + Duration::from_secs(1_000),
    );
    record.stamp(
        &format!("{}/b", wire()),
        UNIX_EPOCH + Duration::from_secs(2_000),
    );
    record
        .save(&dir.path().join(LAST_USED_FILE))
        .expect("seed the last-used record");
    share_file(&dir, "f1", "notes.txt", b"hello");
    share_text(&dir, "t1", "text", "hi", 1);
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);

    foreground(&mut h);

    assert!(drawn(&mut h, "phone-share-sheet"), "the sheet shows");
    let rows = sheet_rows(&mut h).expect("the share sheet is open");
    assert_eq!(
        rows.get(..2),
        Some(["b".to_owned(), "a".to_owned()].as_slice()),
        "the most recently used session comes first"
    );
    assert_eq!(
        phone(&mut h, PhoneRoot::share_sheet_count).as_deref(),
        Some("2 items")
    );
}

#[gpui::test]
fn picking_a_loaded_session_uploads_the_file_and_removes_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.click_on(&row("a"));
    answer_loads(&mut h, "a");
    share_file(&dir, "f1", "notes.txt", b"hello");

    foreground(&mut h);
    let selector = sheet_row(&mut h, "a");
    h.click_on(&selector);

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(
        uploads_in(&sent),
        [("a".to_owned(), "notes.txt".to_owned(), b"hello".to_vec())],
        "the file goes up as the chat's Attach sends one"
    );
    assert!(
        !kept(&dir, "f1.json") && !kept(&dir, "f1.bin"),
        "the item is removed"
    );
    assert_eq!(sheet_rows(&mut h), None, "the sheet closes");
}

#[gpui::test]
fn picking_a_session_not_loaded_opens_it_then_uploads(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    share_file(&dir, "p1", "IMG_1.PNG", b"png bytes");
    foreground(&mut h);

    let selector = sheet_row(&mut h, "a");
    h.click_on(&selector);
    let before = h.host(&saved_remote()).sent();
    assert!(
        uploads_in(&before).is_empty(),
        "nothing goes up before the chat has loaded"
    );
    assert!(kept(&dir, "p1.json"), "the item waits for the chat");
    for msg in before {
        if let ClientMessage::LoadChat {
            session_id,
            request_id,
        } = msg
            && session_id == "a"
        {
            h.host(&saved_remote()).send(DaemonMessage::Chat {
                session_id,
                entries: Vec::new(),
                hidden: 0,
                screen: ChatScreen {
                    composer: ComposerState::Ready,
                    prompt: None,
                },
                request_id,
            });
        }
    }
    h.cx.run_until_parked();

    let sent = h.host(&saved_remote()).sent();
    assert_eq!(
        uploads_in(&sent),
        [(
            "a".to_owned(),
            "IMG_1.PNG".to_owned(),
            b"png bytes".to_vec()
        )],
        "the image goes up once the chat has loaded"
    );
    assert!(!kept(&dir, "p1.json") && !kept(&dir, "p1.bin"));
}

#[gpui::test]
fn shared_text_lands_in_the_message_box_unsent(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    share_text(&dir, "t1", "text", "hello", 1);
    share_text(&dir, "u1", "url", "https://example.com/", 2);
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    foreground(&mut h);

    let sent = pick_and_load(&mut h, "a");

    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("hello\nhttps://example.com/"),
        "each item on a line of its own"
    );
    assert!(
        !sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::ChatSend { .. } | ClientMessage::SendInput { .. }
        )),
        "nothing is sent: {sent:?}"
    );
    assert!(
        !kept(&dir, "t1.json") && !kept(&dir, "u1.json"),
        "the items are removed"
    );
}

#[gpui::test]
fn a_host_without_uploads_keeps_the_file_and_says_so(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    share_file(&dir, "f1", "notes.txt", b"hello");
    share_text(&dir, "t1", "text", "hi", 1);
    let mut h = open_phone(cx, &dir, &[]);
    foreground(&mut h);

    let sent = pick_and_load(&mut h, "a");

    assert!(uploads_in(&sent).is_empty(), "nothing goes up");
    assert!(
        kept(&dir, "f1.json") && kept(&dir, "f1.bin"),
        "the file stays"
    );
    assert_eq!(
        toasts(&mut h),
        vec![(
            "This host can't receive pasted images or files until it is updated".to_owned(),
            None
        )]
    );
    assert_eq!(
        composer_text(&mut h).as_deref(),
        Some("hi"),
        "text still goes in"
    );
    assert!(!kept(&dir, "t1.json"), "and its item is removed");
}

#[gpui::test]
fn a_shared_file_over_the_cap_is_removed_with_the_too_large_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    share_file(&dir, "big", "big.mov", &vec![0; UPLOAD_MAX_BYTES + 1]);
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    foreground(&mut h);

    let sent = pick_and_load(&mut h, "a");

    assert!(
        uploads_in(&sent).is_empty(),
        "a file over the cap is not sent"
    );
    assert_eq!(
        toasts(&mut h),
        vec![(
            "Couldn't send the file".to_owned(),
            Some("big.mov is larger than 8 MiB.".to_owned())
        )]
    );
    assert!(
        !kept(&dir, "big.json") && !kept(&dir, "big.bin"),
        "the item is removed"
    );
}

#[gpui::test]
fn a_tap_outside_the_sheet_discards_the_shared_items(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    share_file(&dir, "f1", "notes.txt", b"hello");
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    foreground(&mut h);
    assert!(drawn(&mut h, "phone-share-sheet"));

    h.click_on("phone-share-scrim");

    assert_eq!(sheet_rows(&mut h), None, "the sheet closes");
    assert!(!drawn(&mut h, "phone-share-sheet"));
    assert!(
        !kept(&dir, "f1.json") && !kept(&dir, "f1.bin"),
        "the items are discarded"
    );
    assert!(
        uploads_in(&h.host(&saved_remote()).sent()).is_empty(),
        "nothing goes up"
    );
    foreground(&mut h);
    assert_eq!(sheet_rows(&mut h), None, "the app does not ask again");
    assert!(!drawn(&mut h, "phone-share-sheet"));
}

#[gpui::test]
fn cancel_discards_the_shared_items_and_the_app_does_not_ask_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    share_file(&dir, "f1", "notes.txt", b"hello");
    share_text(&dir, "t1", "text", "hi", 1);
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    foreground(&mut h);
    assert!(drawn(&mut h, "phone-share-sheet"), "the sheet shows");

    h.click_on("phone-share-cancel");

    assert_eq!(sheet_rows(&mut h), None, "the sheet closes");
    assert!(!drawn(&mut h, "phone-share-sheet"));
    assert!(
        !kept(&dir, "f1.json") && !kept(&dir, "f1.bin") && !kept(&dir, "t1.json"),
        "the inbox holds no manifest left"
    );
    assert!(
        uploads_in(&h.host(&saved_remote()).sent()).is_empty(),
        "nothing goes up"
    );
    foreground(&mut h);
    assert_eq!(sheet_rows(&mut h), None, "the app does not ask again");
    assert!(!drawn(&mut h, "phone-share-sheet"));
}

#[gpui::test]
fn a_half_written_item_raises_no_sheet(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let manifest = json!({
        "v": 1, "id": "w1", "kind": "file", "name": "notes.txt",
        "payload": "w1.bin", "created": now_ms(),
    });
    std::fs::write(inbox(&dir).join("w1.json"), manifest.to_string()).expect("write a manifest");
    let tmp = json!({ "v": 1, "id": "t1", "kind": "text", "text": "hi", "created": now_ms() });
    std::fs::write(inbox(&dir).join("t1.json.tmp"), tmp.to_string()).expect("write a tmp manifest");
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);

    foreground(&mut h);

    assert_eq!(
        sheet_rows(&mut h),
        None,
        "no sheet for an item still being written"
    );
    assert!(!drawn(&mut h, "phone-share-sheet"));
    assert!(kept(&dir, "w1.json"), "the item waits for its payload");
}

#[gpui::test]
fn a_second_foreground_with_the_sheet_open_adds_no_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    share_file(&dir, "f1", "notes.txt", b"hello");
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    foreground(&mut h);
    let rows = sheet_rows(&mut h).expect("the share sheet is open");

    share_text(&dir, "t1", "text", "hi", 1);
    foreground(&mut h);

    assert_eq!(sheet_rows(&mut h), Some(rows), "the same rows, once each");
    assert_eq!(
        phone(&mut h, PhoneRoot::share_sheet_count).as_deref(),
        Some("1 item"),
        "the open sheet takes no new items"
    );
}

#[gpui::test]
fn a_new_share_replaces_the_sheet_already_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    share_file(&dir, "a1", "first.txt", b"one");
    share_text(&dir, "a2", "text", "hello", 1);
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    foreground(&mut h);
    assert_eq!(
        phone(&mut h, PhoneRoot::share_sheet_count).as_deref(),
        Some("2 items"),
        "the first share's sheet shows"
    );

    // The extension replaces the pending share: its files go, the new
    // share's arrive.
    for name in ["a1.json", "a1.bin", "a2.json"] {
        std::fs::remove_file(inbox(&dir).join(name)).expect("remove the earlier share's file");
    }
    share_file(&dir, "b1", "second.txt", b"two");

    foreground(&mut h);

    assert_eq!(
        phone(&mut h, PhoneRoot::share_sheet_count).as_deref(),
        Some("1 item"),
        "the sheet holds the new share alone"
    );
    assert!(drawn(&mut h, "phone-share-sheet"));
}

#[gpui::test]
fn the_sheet_offers_only_sessions_with_a_chat(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    share_file(&dir, "f1", "notes.txt", b"hello");
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    foreground(&mut h);

    let mut rows = sheet_rows(&mut h).expect("the share sheet is open");
    rows.sort();

    // The fake host names no agents, so nothing says `e`'s is down and its
    // chat still shows; `n` has no agent, so no chat to take a share.
    assert_eq!(
        rows,
        ["a", "b", "e"],
        "a session with no agent has no chat to take a share"
    );
}

#[gpui::test]
fn a_refused_chat_load_gives_up_and_keeps_the_items(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    share_file(&dir, "f1", "notes.txt", b"hello");
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    foreground(&mut h);
    let selector = sheet_row(&mut h, "a");
    h.click_on(&selector);
    let sent = h.host(&saved_remote()).sent();
    for msg in sent {
        if let ClientMessage::LoadChat {
            session_id,
            request_id,
        } = msg
            && session_id == "a"
        {
            h.host(&saved_remote()).send(DaemonMessage::Error {
                message: "chat is not offered for agent `other`".to_owned(),
                request_id,
                code: Some("unsupported_message".to_owned()),
            });
        }
    }
    h.cx.run_until_parked();

    assert_eq!(
        toasts(&mut h),
        vec![(
            "Couldn't send the shared items: the session's chat could not load.".to_owned(),
            None
        )]
    );
    assert!(
        kept(&dir, "f1.json") && kept(&dir, "f1.bin"),
        "the item stays"
    );
    foreground(&mut h);
    assert!(
        sheet_rows(&mut h).is_some(),
        "the gave-up share no longer holds the inbox: the next foreground asks again"
    );
}

#[gpui::test]
fn a_share_waiting_at_start_raises_the_sheet_once_sessions_arrive(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    share_file(&dir, "f1", "notes.txt", b"hello");

    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.cx.run_until_parked();

    let mut rows = sheet_rows(&mut h).expect("the sheet opens once the host lists sessions");
    rows.sort();
    assert_eq!(rows, ["a", "b", "e"]);
    assert!(drawn(&mut h, "phone-share-sheet"));
}

#[gpui::test]
fn no_share_sheet_over_a_pending_question(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.click_on(&row("a"));
    answer_loads(&mut h, "a");
    let question = ChatPrompt {
        id: "q1".to_owned(),
        kind: ChatPromptKind::Question,
        subject: "calc.py".to_owned(),
        body: "Which one?".to_owned(),
        options: [(1, "sub"), (2, "div")]
            .into_iter()
            .map(|(number, label)| ChatOption {
                number,
                label: label.to_owned(),
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
    };
    h.host(&saved_remote())
        .send(DaemonMessage::ChatScreenChanged {
            session_id: "a".to_owned(),
            screen: ChatScreen {
                composer: ComposerState::Dialog,
                prompt: Some(question),
            },
        });
    h.cx.run_until_parked();
    assert!(drawn(&mut h, "phone-prompt-sheet"), "the question is up");
    share_file(&dir, "f1", "notes.txt", b"hello");

    foreground(&mut h);

    assert_eq!(sheet_rows(&mut h), None, "no share sheet over the question");
    assert!(kept(&dir, "f1.json"), "the item waits");
}

#[gpui::test]
fn a_share_row_names_its_repo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.host(&saved_remote()).send(DaemonMessage::Sessions {
        sessions: vec![
            session("a")
                .label("Alpha")
                .in_repo("r1")
                .status("working")
                .build(),
        ],
    });
    h.cx.run_until_parked();
    share_file(&dir, "f1", "notes.txt", b"hello");

    foreground(&mut h);

    assert_eq!(
        sheet_details(&mut h),
        Some(vec![Some(format!("r1 · {REMOTE}"))]),
        "the row names the repo it sits under, then its host"
    );
    let selector = sheet_detail(&mut h, "a");
    assert!(drawn(&mut h, &selector), "{selector} is drawn");
}

#[gpui::test]
fn a_share_row_names_its_workspace(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.host(&saved_remote()).send(DaemonMessage::Workspaces {
        workspaces: vec![workspace("ws1", &[])],
    });
    h.host(&saved_remote()).send(DaemonMessage::Sessions {
        sessions: vec![
            session("w")
                .label("Whiskey")
                .in_workspace("ws1")
                .status("working")
                .build(),
        ],
    });
    h.cx.run_until_parked();
    share_file(&dir, "f1", "notes.txt", b"hello");

    foreground(&mut h);

    assert_eq!(
        sheet_details(&mut h),
        Some(vec![Some(format!("ws1 · {REMOTE}"))]),
        "the row names the workspace it sits under, then its host"
    );
    let selector = sheet_detail(&mut h, "w");
    assert!(drawn(&mut h, &selector), "{selector} is drawn");
}

#[gpui::test]
fn a_share_row_whose_repo_repeats_its_title_has_no_detail(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.host(&saved_remote()).send(DaemonMessage::Sessions {
        sessions: vec![
            session("r1")
                .label("r1")
                .in_repo("r1")
                .status("working")
                .build(),
        ],
    });
    h.cx.run_until_parked();
    share_file(&dir, "f1", "notes.txt", b"hello");

    foreground(&mut h);

    let details = sheet_details(&mut h).expect("the share sheet is open");
    let detail = details
        .first()
        .cloned()
        .flatten()
        .expect("a remote row still names its host");
    assert!(
        !detail.contains("r1"),
        "a repo that only repeats the title is left out: {detail}"
    );
    assert_eq!(detail, REMOTE, "the row names its host alone");
    let selector = sheet_detail(&mut h, "r1");
    assert!(drawn(&mut h, &selector), "{selector} is drawn");
}
