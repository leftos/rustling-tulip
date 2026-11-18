//! The phone's viewer page: the last file a fetch saved on the phone opens
//! over the page shown, under a header with Back, the file's name, its host
//! and its path on that host; Back or a committed back gesture closes it and
//! shows the page under it again.

use std::sync::Arc;
use std::time::Duration;

use crate::support;

use base64::Engine as _;
use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{BackGesturePhase, Entity, RetainAllImageCache, TestAppContext, point, px};
use protocol::{ClientMessage, DaemonMessage, FEATURE_HOST_PATHS, FEATURE_RELAY_PUSH, PathKind};
use rustling_tulip_native::file_fetch::FetchRequest;
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::phone::push::FakePushPlatform;
use rustling_tulip_native::target::HostKey;
use rustling_tulip_native::{Connection, LinkAction, NetEvent, PhoneFetched, ViewerTab};
use serde_json::json;
use support::{Harness, REMOTE, TestDir, repo, saved_remote, seed_ui, session};

/// The host as the fetch's toasts name it.
const LABEL: &str = "builder@Buildbox";

/// The reserved viewer tab the phone's page reads its file under.
const PHONE_VIEWER_TAB: &str = "phone-viewer";

/// A 2x2 grayscale PNG, so the viewer has a real image to decode.
const TWO_BY_TWO_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x00, 0x00, 0x00, 0x00, 0x57, 0xdd, 0x52,
    0xf8, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x60, 0x80, 0x01, 0x00,
    0x00, 0x0a, 0x00, 0x01, 0xec, 0x24, 0x03, 0xb9, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

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
    open_phone_listing(cx, dir, copies, platform, &[FEATURE_RELAY_PUSH])
}

/// [`open_phone`], its host listing `features` on top of the defaults.
fn open_phone_listing<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    copies: &std::path::Path,
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
    save_bytes(h, copies, name, text.as_bytes());
}

/// [`save`], the copy holding `bytes`.
fn save_bytes(h: &mut Harness<'_>, copies: &std::path::Path, name: &str, bytes: &[u8]) {
    let saved = PhoneFetched {
        host: REMOTE.to_owned(),
        host_path: format!("/abs/{name}"),
        local_path: format!("{REMOTE}/root/abs/{name}"),
        size: u64::try_from(bytes.len()).expect("a small file"),
        host_label: LABEL.to_owned(),
    };
    let path = copies.join(&saved.local_path);
    std::fs::create_dir_all(path.parent().expect("a copy has a folder"))
        .expect("make the copy's folder");
    std::fs::write(&path, bytes).expect("write the copy");
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

/// The image cache the phone's viewer page draws through, while it is open.
fn viewer_cache(h: &mut Harness<'_>) -> Option<Entity<RetainAllImageCache>> {
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.read(cx).viewer_image_cache()
    })
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

/// The host path of the copy [`keep_notes`] keeps.
const NOTES: &str = "/abs/notes.txt";

/// A phone as [`open_phone`] opens it, its host serving host paths.
fn open_phone_serving_paths<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    copies: &std::path::Path,
) -> Harness<'a> {
    open_phone_listing(
        cx,
        dir,
        copies,
        Arc::new(FakePushPlatform::default()),
        &[FEATURE_RELAY_PUSH, FEATURE_HOST_PATHS],
    )
}

/// Keeps `notes.txt`, five bytes, as a copy of [`NOTES`], closes the viewer
/// its save opened and drains what the phone sent so far.
fn keep_notes(h: &mut Harness<'_>, copies: &std::path::Path) {
    save(h, copies, "notes.txt", "text\n");
    h.click_on("phone-viewer-back");
    sent(h);
}

/// Opens the kept copy of [`REMOTE`]'s `host_path` again.
fn open_kept(h: &mut Harness<'_>, host_path: &str) {
    let host_path = host_path.to_owned();
    h.cx.update(|window, cx| {
        let phone = window
            .root::<PhoneRoot>()
            .flatten()
            .expect("the window's root is the phone's");
        phone.update(cx, |phone, cx| {
            phone.open_kept(HostKey::of(&saved_remote()), host_path, cx);
        });
    });
    h.cx.run_until_parked();
}

/// What the phone sent [`REMOTE`] since the last call.
fn sent(h: &mut Harness<'_>) -> Vec<ClientMessage> {
    h.host(&saved_remote()).sent()
}

/// The ids of the stats about `path` among `sent`.
fn stats_of(sent: &[ClientMessage], path: &str) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::StatHostPath { id, path: asked } if asked == path => Some(id.clone()),
            _ => None,
        })
        .collect()
}

/// The host paths fetched among `sent`.
fn fetched_paths(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::FetchHostFile { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

/// The one stat about [`NOTES`] the phone sent since the last call.
fn notes_stat(h: &mut Harness<'_>) -> String {
    let sent = sent(h);
    let ids = stats_of(&sent, NOTES);
    assert_eq!(ids.len(), 1, "one stat about the copy: {sent:?}");
    assert_eq!(fetched_paths(&sent), Vec::<String>::new(), "and no fetch");
    ids[0].clone()
}

/// [`REMOTE`] answers the stat `id`: a file of `size` bytes.
fn answer(h: &mut Harness<'_>, id: &str, size: u64) {
    h.host(&saved_remote()).send(DaemonMessage::PathStat {
        id: id.to_owned(),
        kind: PathKind::File,
        size,
        modified: Some(1_700_000_000_000),
    });
}

/// The open viewer's file name, host path and whether it may be stale.
fn shown(h: &mut Harness<'_>) -> Option<(String, String, bool)> {
    header(h).map(|(name, _, path, stale)| (name, path, stale))
}

#[gpui::test]
fn reopening_a_fresh_copy_opens_it_without_a_fetch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone_serving_paths(cx, &dir, &copies);
    keep_notes(&mut h, &copies);

    open_kept(&mut h, NOTES);
    let id = notes_stat(&mut h);
    assert_eq!(shown(&mut h), None, "nothing opens before the host answers");
    answer(&mut h, &id, 5);

    assert_eq!(
        shown(&mut h),
        Some(("notes.txt".to_owned(), NOTES.to_owned(), false))
    );
    assert!(
        drawn(&mut h, "viewer-phone-viewer"),
        "the copy's body shows"
    );
    assert!(!drawn(&mut h, "phone-viewer-stale"));
    assert_eq!(fetched_paths(&sent(&mut h)), Vec::<String>::new());
}

#[gpui::test]
fn reopening_a_changed_copy_fetches_it_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone_serving_paths(cx, &dir, &copies);
    keep_notes(&mut h, &copies);

    open_kept(&mut h, NOTES);
    let id = notes_stat(&mut h);
    answer(&mut h, &id, 99);

    assert_eq!(
        fetched_paths(&sent(&mut h)),
        [NOTES],
        "the file is fetched again"
    );
    assert_eq!(shown(&mut h), None, "the old copy does not open");
}

#[gpui::test]
fn reopening_with_the_host_offline_opens_the_copy_as_out_of_date(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone_serving_paths(cx, &dir, &copies);
    keep_notes(&mut h, &copies);
    h.host(&saved_remote())
        .event(NetEvent::State(Connection::for_target(saved_remote())));

    open_kept(&mut h, NOTES);

    assert_eq!(
        shown(&mut h),
        Some(("notes.txt".to_owned(), NOTES.to_owned(), true))
    );
    assert!(drawn(&mut h, "phone-viewer-stale"), "the stale line shows");
    let sent = sent(&mut h);
    assert_eq!(
        stats_of(&sent, NOTES),
        Vec::<String>::new(),
        "nothing is asked"
    );
    assert_eq!(
        fetched_paths(&sent),
        Vec::<String>::new(),
        "nothing is fetched"
    );
}

#[gpui::test]
fn a_stat_that_never_answers_opens_the_copy_as_out_of_date_after_the_timeout(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone_serving_paths(cx, &dir, &copies);
    keep_notes(&mut h, &copies);
    open_kept(&mut h, NOTES);
    notes_stat(&mut h);

    h.advance(Duration::from_millis(4_900));
    assert_eq!(shown(&mut h), None, "the phone still waits");
    h.advance(Duration::from_millis(100));

    assert_eq!(
        shown(&mut h),
        Some(("notes.txt".to_owned(), NOTES.to_owned(), true))
    );
    assert!(drawn(&mut h, "phone-viewer-stale"), "the stale line shows");
}

#[gpui::test]
fn a_late_stat_answer_after_the_timeout_changes_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone_serving_paths(cx, &dir, &copies);
    keep_notes(&mut h, &copies);
    open_kept(&mut h, NOTES);
    let id = notes_stat(&mut h);
    h.advance(Duration::from_secs(5));

    answer(&mut h, &id, 99);

    assert_eq!(
        shown(&mut h),
        Some(("notes.txt".to_owned(), NOTES.to_owned(), true)),
        "the stale copy stays open"
    );
    assert_eq!(
        fetched_paths(&sent(&mut h)),
        Vec::<String>::new(),
        "the late answer fetches nothing"
    );
}

#[gpui::test]
fn a_kept_copy_from_a_windows_host_is_stated_by_its_display_path(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone_serving_paths(cx, &dir, &copies);
    let mut host = h.host(&saved_remote());
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/repos/repo")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").worktree("r1").build()],
    });
    let request = FetchRequest {
        host: HostKey::of(&saved_remote()),
        repo_id: "r1".to_owned(),
        repo_name: "repo".to_owned(),
        worktree_path: Some("C:/wt/r1".to_owned()),
        rel_path: "notes.txt".to_owned(),
        host_path: false,
        line: None,
        column: None,
        open_when_done: true,
        action: LinkAction::Viewer,
    };
    let id = h
        .root_update(move |root, _, cx| root.fetch_remote_file(request, cx))
        .expect("a fetch on a remote host starts");
    let mut host = h.host(&saved_remote());
    host.send(DaemonMessage::FileFetchStarted {
        id: id.clone(),
        resolved_path: r"\\?\C:\repos\repo\notes.txt".to_owned(),
        size: 5,
    });
    host.send(DaemonMessage::FileChunk {
        id: id.clone(),
        seq: 0,
        data_b64: base64::engine::general_purpose::STANDARD.encode(b"text\n"),
    });
    host.send(DaemonMessage::FileFetchDone { id });
    h.click_on("phone-viewer-back");
    sent(&mut h);

    open_kept(&mut h, r"C:\repos\repo\notes.txt");

    let sent = sent(&mut h);
    assert_eq!(
        stats_of(&sent, r"C:\repos\repo\notes.txt").len(),
        1,
        "the copy is stated by the path the host accepts: {sent:?}"
    );
    assert_eq!(
        fetched_paths(&sent),
        Vec::<String>::new(),
        "and not fetched"
    );
}

#[gpui::test]
fn a_changed_copy_whose_refetch_is_refused_opens_as_out_of_date(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone_serving_paths(cx, &dir, &copies);
    // A host path that is not absolute: a fetch by it is refused before
    // anything is sent.
    let saved = PhoneFetched {
        host: REMOTE.to_owned(),
        host_path: "notes.txt".to_owned(),
        local_path: format!("{REMOTE}/root/abs/notes.txt"),
        size: 5,
        host_label: LABEL.to_owned(),
    };
    let path = copies.join(&saved.local_path);
    std::fs::create_dir_all(path.parent().expect("a copy has a folder"))
        .expect("make the copy's folder");
    std::fs::write(&path, "text\n").expect("write the copy");
    h.root_update(|root, _, cx| root.phone_file_saved(saved, cx));
    h.click_on("phone-viewer-back");
    sent(&mut h);

    open_kept(&mut h, "notes.txt");
    let ids = stats_of(&sent(&mut h), "notes.txt");
    assert_eq!(ids.len(), 1, "the phone asks about the copy");
    answer(&mut h, &ids[0], 99);

    assert_eq!(
        shown(&mut h),
        Some(("notes.txt".to_owned(), "notes.txt".to_owned(), true)),
        "the old copy opens as possibly out of date"
    );
    assert!(drawn(&mut h, "phone-viewer-stale"), "the stale line shows");
    assert_eq!(fetched_paths(&sent(&mut h)), Vec::<String>::new());
}

#[gpui::test]
fn opening_a_path_with_no_copy_fetches_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone_serving_paths(cx, &dir, &copies);
    keep_notes(&mut h, &copies);

    open_kept(&mut h, "/abs/other.txt");

    let sent = sent(&mut h);
    assert_eq!(
        fetched_paths(&sent),
        ["/abs/other.txt"],
        "the file is fetched"
    );
    assert_eq!(stats_of(&sent, "/abs/other.txt"), Vec::<String>::new());
    assert_eq!(shown(&mut h), None, "nothing opens before it is saved");
}

#[gpui::test]
fn a_saved_image_shows_scaled_to_fit(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone(cx, &dir, &copies);

    save_bytes(&mut h, &copies, "shot.png", TWO_BY_TWO_PNG);

    assert!(drawn(&mut h, "phone-viewer"), "the viewer page shows");
    assert!(drawn(&mut h, "phone-viewer-image"), "the image shows");
    assert!(
        !drawn(&mut h, "viewer-phone-viewer"),
        "the text body does not"
    );
    let page = h.bounds("phone-viewer");
    let image = h.bounds("phone-viewer-image");
    assert!(
        image.size.width <= page.size.width && image.size.height <= page.size.height,
        "the image fits the page: {image:?} within {page:?}"
    );
}

#[gpui::test]
fn a_saved_binary_file_shows_a_card_with_its_name_and_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone(cx, &dir, &copies);

    save_bytes(&mut h, &copies, "blob.bin", b"\x00binary\x00");

    assert!(drawn(&mut h, "phone-viewer"), "the viewer page shows");
    assert!(drawn(&mut h, "phone-viewer-card"), "the card shows");
    assert!(drawn(&mut h, "phone-viewer-card-name"), "with the name");
    assert!(drawn(&mut h, "phone-viewer-card-size"), "and its size");
    assert!(
        !drawn(&mut h, "viewer-phone-viewer"),
        "the text body does not"
    );
}

#[gpui::test]
fn a_reopened_image_uses_a_fresh_image_cache(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone(cx, &dir, &copies);

    save_bytes(&mut h, &copies, "shot.png", TWO_BY_TWO_PNG);
    assert!(drawn(&mut h, "phone-viewer-image"), "the image shows");
    let first = viewer_cache(&mut h).expect("the open page draws through a cache");
    h.click_on("phone-viewer-back");
    assert!(!drawn(&mut h, "phone-viewer"), "the viewer page is gone");

    save_bytes(&mut h, &copies, "shot.png", TWO_BY_TWO_PNG);

    assert!(drawn(&mut h, "phone-viewer-image"), "the image shows again");
    let second = viewer_cache(&mut h).expect("the reopened page draws through a cache");
    assert_ne!(first, second, "each open draws through its own cache");
}

#[gpui::test]
fn a_broken_image_shows_the_card(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone(cx, &dir, &copies);

    save_bytes(&mut h, &copies, "broken.png", b"not an image\n");

    assert!(drawn(&mut h, "phone-viewer"), "the viewer page shows");
    assert!(
        drawn(&mut h, "phone-viewer-card"),
        "the card stands in for the image"
    );
    assert!(
        !drawn(&mut h, "viewer-phone-viewer"),
        "the text body does not"
    );
}
