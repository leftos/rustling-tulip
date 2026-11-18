//! The phone's viewer page: the last file a fetch saved on the phone opens
//! over the page shown, under a header with Back, the file's name, its host
//! and its path on that host; Back or a committed back gesture closes it and
//! shows the page under it again.

use std::sync::Arc;

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{BackGesturePhase, TestAppContext, point, px};
use protocol::{DaemonMessage, FEATURE_RELAY_PUSH};
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::phone::push::FakePushPlatform;
use rustling_tulip_native::{PhoneFetched, ViewerTab};
use serde_json::json;
use support::{Harness, REMOTE, TestDir, saved_remote, seed_ui, session};

/// The host as the fetch's toasts name it.
const LABEL: &str = "builder@Buildbox";

/// The reserved viewer tab the phone's page reads its file under.
const PHONE_VIEWER_TAB: &str = "phone-viewer";

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// A phone whose [`REMOTE`] is open and listing `s1`, its fetched copies
/// under `copies`.
fn open_phone<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    copies: &std::path::Path,
) -> Harness<'a> {
    open_phone_with(cx, dir, copies, Arc::new(FakePushPlatform::default()))
}

/// [`open_phone`], its push through `platform`.
fn open_phone_with<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    copies: &std::path::Path,
    platform: Arc<FakePushPlatform>,
) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone_with(cx, dir, platform);
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    let mut host = h.host(&saved_remote());
    host.connect_with_features(&[FEATURE_RELAY_PUSH]);
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").build()],
    });
    let copies = copies.to_path_buf();
    h.root_update(move |root, _, _| root.set_remote_files_root(copies));
    h
}

/// Writes `text` as the copy of `name` fetched from [`REMOTE`] under
/// `copies` and hands the phone its saved fetch.
fn save(h: &mut Harness<'_>, copies: &std::path::Path, name: &str, text: &str) {
    let saved = PhoneFetched {
        host: REMOTE.to_owned(),
        host_path: format!("/abs/{name}"),
        local_path: format!("{REMOTE}/root/abs/{name}"),
        size: u64::try_from(text.len()).expect("a small file"),
        host_label: LABEL.to_owned(),
    };
    let path = copies.join(&saved.local_path);
    std::fs::create_dir_all(path.parent().expect("a copy has a folder"))
        .expect("make the copy's folder");
    std::fs::write(&path, text).expect("write the copy");
    h.root_update(|root, _, cx| root.phone_file_saved(saved, cx));
}

/// The viewer page's header: name, host label, host path and whether the
/// copy may be stale; `None` while the page is closed.
fn header(h: &mut Harness<'_>) -> Option<(String, String, String, bool)> {
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.read(cx).viewer_header()
    })
}

/// Whether the root view holds the phone's reserved viewer tab.
fn holds_viewer_tab(h: &mut Harness<'_>) -> bool {
    h.root_update(|root, _, _| root.viewer_tab(PHONE_VIEWER_TAB).is_some())
}

#[gpui::test]
fn a_saved_text_file_opens_in_the_phone_viewer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone(cx, &dir, &copies);

    save(&mut h, &copies, "notes.txt", "first line\nsecond line\n");

    assert!(drawn(&mut h, "phone-viewer"), "the viewer page shows");
    assert!(drawn(&mut h, "viewer-phone-viewer"), "with the file's body");
    assert!(drawn(&mut h, "phone-viewer-back"), "under Back");
    assert!(drawn(&mut h, "phone-viewer-title"));
    assert!(drawn(&mut h, "phone-viewer-host"));
    assert!(drawn(&mut h, "phone-viewer-path"));
    assert!(!drawn(&mut h, "phone-home"), "in Home's place");
    assert_eq!(
        header(&mut h),
        Some((
            "notes.txt".to_owned(),
            LABEL.to_owned(),
            "/abs/notes.txt".to_owned(),
            false
        ))
    );
    assert!(
        !drawn(&mut h, "phone-viewer-stale"),
        "a fresh copy shows no stale line"
    );
}

#[gpui::test]
fn a_saved_markdown_file_renders(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone(cx, &dir, &copies);

    save(&mut h, &copies, "readme.md", "# Title\n\nSome *words*.\n");

    assert!(drawn(&mut h, "phone-viewer"), "the viewer page shows");
    assert!(
        drawn(&mut h, "viewer-md-phone-viewer"),
        "the Markdown is rendered"
    );
}

#[gpui::test]
fn back_closes_the_viewer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone(cx, &dir, &copies);
    save(&mut h, &copies, "notes.txt", "text\n");
    assert!(drawn(&mut h, "phone-viewer"), "the viewer page shows");

    h.click_on("phone-viewer-back");

    assert!(!drawn(&mut h, "phone-viewer"), "the viewer page is gone");
    assert!(drawn(&mut h, "phone-home"), "Home shows again");
    assert_eq!(header(&mut h), None);
    assert!(!holds_viewer_tab(&mut h), "the reserved tab is dropped");
}

#[gpui::test]
fn the_back_gesture_closes_the_viewer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone(cx, &dir, &copies);
    save(&mut h, &copies, "notes.txt", "text\n");
    assert!(drawn(&mut h, "phone-viewer"), "the viewer page shows");
    let page = h.bounds("phone-viewer");
    let at = point(page.origin.x + px(4.0), page.origin.y + px(200.0));

    h.back_gesture(BackGesturePhase::Committed, at);

    assert!(!drawn(&mut h, "phone-viewer"), "the viewer page is gone");
    assert!(drawn(&mut h, "phone-home"), "Home shows again");
    assert_eq!(header(&mut h), None);
    assert!(!holds_viewer_tab(&mut h), "the reserved tab is dropped");
}

#[gpui::test]
fn a_tap_on_a_line_in_the_phone_viewer_selects_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone(cx, &dir, &copies);
    save(&mut h, &copies, "notes.txt", "one\ntwo\nthree\n");
    assert!(drawn(&mut h, "viewer-line-phone-viewer-2"), "line 2 shows");

    h.click_on("viewer-line-phone-viewer-2");

    assert_eq!(
        h.root(|root, _| root
            .viewer_tab(PHONE_VIEWER_TAB)
            .and_then(ViewerTab::selected_lines)),
        None,
        "the phone's viewer selects nothing"
    );
}

#[gpui::test]
fn a_link_tap_in_the_phone_viewer_opens_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone(cx, &dir, &copies);
    save(&mut h, &copies, "other.md", "# Other\n");
    save(
        &mut h,
        &copies,
        "readme.md",
        "[other](other.md) and [site](https://example.com/page)\n",
    );
    assert!(
        drawn(&mut h, "viewer-md-phone-viewer"),
        "the Markdown shows"
    );

    for href in ["other.md", "https://example.com/page"] {
        h.root_update(move |root, window, cx| {
            root.follow_md_href(PHONE_VIEWER_TAB, href, window, cx);
        });
        assert_eq!(h.opened(), [], "{href} opens nothing");
        assert_eq!(h.run_confirm(), None, "{href} asks nothing");
    }
}

#[gpui::test]
fn a_notification_tap_closes_the_viewer_and_shows_the_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let platform = Arc::new(FakePushPlatform::default());
    let mut h = open_phone_with(cx, &dir, &copies, platform.clone());
    save(&mut h, &copies, "notes.txt", "text\n");
    assert!(drawn(&mut h, "phone-viewer"), "the viewer page shows");

    platform.tap(REMOTE, "s1");
    h.cx.run_until_parked();

    assert!(drawn(&mut h, "phone-session"), "the session page shows");
    assert!(!drawn(&mut h, "phone-viewer"), "the viewer closed for it");
    assert_eq!(header(&mut h), None);
    assert!(!holds_viewer_tab(&mut h), "the reserved tab is dropped");
}

#[gpui::test]
fn a_layout_reconcile_keeps_the_phone_viewer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone(cx, &dir, &copies);
    save(&mut h, &copies, "notes.txt", "text\n");

    h.root_update(|root, _, cx| root.reconcile_viewer_tabs(cx));

    assert!(holds_viewer_tab(&mut h), "the reserved tab survives");
    assert!(drawn(&mut h, "phone-viewer"), "the viewer page shows");
    assert!(drawn(&mut h, "viewer-phone-viewer"), "with the file's body");
}
