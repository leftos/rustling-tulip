//! The phone's host browser: a page over the session page that lists a
//! host folder through the host's listing, folders first, hidden entries
//! left out until Show hidden; a folder row enters it, the parent row goes
//! up, a file row fetches the file and opens it in the viewer, and the next
//! page is asked for as the list nears its end. Back returns to the page
//! under it. A long press on a folder row offers Fetch folder, which fetches
//! the folder whole into the copies folder, keeps it as one folder entry
//! and says how it went on the line under the header.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::support;

use base64::Engine as _;
use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::{TestAppContext, px};
use protocol::{
    ClientMessage, DaemonMessage, DirEntry, FEATURE_HOST_BROWSE, FEATURE_HOST_PATHS, HostOsKind,
    PathKind,
};
use rustling_tulip_native::PhoneFetched;
use rustling_tulip_native::phone::PhoneRoot;
use rustling_tulip_native::phone::push::FakePushPlatform;
use rustling_tulip_native::target::HostKey;
use serde_json::json;
use support::{Harness, REMOTE, TestDir, saved_remote, seed_ui, session};

/// The folder the browser opens at.
const SRC: &str = "/home/me/src";

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// A phone whose [`REMOTE`] is open, listing `s1` and `features` on top of
/// the defaults, its fetched copies under `copies`.
fn open_phone_listing<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    copies: &std::path::Path,
    features: &[&str],
) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone_with(cx, dir, Arc::new(FakePushPlatform::default()));
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

/// [`open_phone_listing`] with a host that serves host paths and browsing.
fn open_phone<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let copies = dir.path().join("copies");
    open_phone_listing(cx, dir, &copies, &[FEATURE_HOST_PATHS, FEATURE_HOST_BROWSE])
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

/// Opens the browser on [`REMOTE`]'s `path`.
fn open_browser(h: &mut Harness<'_>, path: &str) {
    let path = path.to_owned();
    phone_update(h, |phone, cx| {
        phone.open_browser(HostKey::of(&saved_remote()), path, cx);
    });
}

/// What the phone sent [`REMOTE`] since the last call.
fn sent(h: &mut Harness<'_>) -> Vec<ClientMessage> {
    h.host(&saved_remote()).sent()
}

/// The ids of the host info requests among `sent`.
fn info_ids(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::GetHostInfo { id } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

/// The listings among `sent`: id, path, cursor and limit.
fn listings(sent: &[ClientMessage]) -> Vec<(String, String, Option<String>, Option<u32>)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::ListDir {
                id,
                path,
                cursor,
                limit,
            } => Some((id.clone(), path.clone(), cursor.clone(), *limit)),
            _ => None,
        })
        .collect()
}

/// The one listing the phone sent since the last call: its id, path and
/// cursor.
fn one_listing(h: &mut Harness<'_>) -> (String, String, Option<String>) {
    let sent = sent(h);
    let listed = listings(&sent);
    assert_eq!(listed.len(), 1, "one listing: {sent:?}");
    let (id, path, cursor, _) = listed[0].clone();
    (id, path, cursor)
}

/// [`REMOTE`] answers the host info request `id` as a Linux host.
fn answer_info(h: &mut Harness<'_>, id: &str) {
    h.host(&saved_remote()).send(DaemonMessage::HostInfo {
        id: id.to_owned(),
        os: HostOsKind::Linux,
        home: Some("/home/me".to_owned()),
        roots: vec!["/".to_owned()],
    });
}

/// [`REMOTE`] answers the listing `id` of `path` with `entries`.
fn answer_listing(
    h: &mut Harness<'_>,
    id: &str,
    path: &str,
    entries: Vec<DirEntry>,
    next_cursor: Option<&str>,
) {
    h.host(&saved_remote()).send(DaemonMessage::DirListing {
        id: id.to_owned(),
        path: path.to_owned(),
        entries,
        next_cursor: next_cursor.map(str::to_owned),
        error: None,
    });
}

/// An entry named `name` of `kind`, `size` bytes if a file.
fn entry(name: &str, kind: PathKind, size: u64) -> DirEntry {
    DirEntry {
        name: name.to_owned(),
        kind,
        size,
        modified: Some(1_700_000_000_000),
        link: false,
        hidden: name.starts_with('.'),
    }
}

/// Opens the browser on `path`, answers its host info and its first
/// listing with `entries`.
fn browse(h: &mut Harness<'_>, path: &str, entries: Vec<DirEntry>, next_cursor: Option<&str>) {
    open_browser(h, path);
    let sent = sent(h);
    let ids = info_ids(&sent);
    assert_eq!(ids.len(), 1, "one host info request: {sent:?}");
    answer_info(h, &ids[0]);
    let (id, listed, _) = one_listing(h);
    assert_eq!(listed, path);
    answer_listing(h, &id, path, entries, next_cursor);
}

/// The folder and the file the specs list in [`SRC`].
fn docs_and_notes() -> Vec<DirEntry> {
    vec![
        entry("notes.txt", PathKind::File, 2048),
        entry("docs", PathKind::Dir, 0),
    ]
}

#[gpui::test]
fn open_browser_asks_for_the_host_info_then_lists_the_folder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);

    open_browser(&mut h, SRC);
    let first = sent(&mut h);
    let ids = info_ids(&first);
    assert_eq!(ids.len(), 1, "the host info is asked for: {first:?}");
    assert_eq!(
        listings(&first),
        [],
        "nothing is listed before the host answers"
    );
    assert!(drawn(&mut h, "phone-browser"), "the page shows");

    answer_info(&mut h, &ids[0]);

    let listed = listings(&sent(&mut h));
    assert_eq!(listed.len(), 1, "one listing: {listed:?}");
    assert_eq!(listed[0].1, SRC);
    assert_eq!(listed[0].2, None, "from its first page");
    assert_eq!(listed[0].3, Some(200), "in pages of 200");
}

#[gpui::test]
fn the_page_lists_folders_before_files_with_sizes_on_files(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);

    browse(&mut h, SRC, docs_and_notes(), None);

    assert!(
        drawn(&mut h, "phone-browser-path"),
        "the header names the folder"
    );
    assert!(drawn(&mut h, "phone-browser-row-docs"));
    assert!(drawn(&mut h, "phone-browser-row-notes.txt"));
    let folder = h.bounds("phone-browser-row-docs");
    let file = h.bounds("phone-browser-row-notes.txt");
    assert!(folder.top() < file.top(), "the folder comes first");
    assert!(
        drawn(&mut h, "phone-browser-size-notes.txt"),
        "a file shows its size"
    );
    assert!(
        !drawn(&mut h, "phone-browser-size-docs"),
        "a folder shows none"
    );
    assert!(
        drawn(&mut h, "phone-browser-hidden"),
        "with Show hidden in the header"
    );
}

#[gpui::test]
fn tapping_a_folder_row_lists_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    browse(&mut h, SRC, docs_and_notes(), None);

    h.click_on("phone-browser-row-docs");

    let (id, path, cursor) = one_listing(&mut h);
    assert_eq!(path, "/home/me/src/docs");
    assert_eq!(cursor, None);
    answer_listing(
        &mut h,
        &id,
        &path,
        vec![entry("guide.md", PathKind::File, 10)],
        None,
    );
    assert!(
        drawn(&mut h, "phone-browser-row-guide.md"),
        "the folder's rows show"
    );
    assert!(
        !drawn(&mut h, "phone-browser-row-docs"),
        "in the old folder's place"
    );
}

#[gpui::test]
fn the_parent_row_goes_up_and_is_not_drawn_at_the_root(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    browse(&mut h, SRC, docs_and_notes(), None);

    h.click_on("phone-browser-parent");

    let (id, path, _) = one_listing(&mut h);
    assert_eq!(path, "/home/me");
    answer_listing(
        &mut h,
        &id,
        &path,
        vec![entry("src", PathKind::Dir, 0)],
        None,
    );
    h.click_on("phone-browser-parent");
    let (id, path, _) = one_listing(&mut h);
    assert_eq!(path, "/home");
    answer_listing(&mut h, &id, &path, Vec::new(), None);
    h.click_on("phone-browser-parent");
    let (id, path, _) = one_listing(&mut h);
    assert_eq!(path, "/");
    answer_listing(
        &mut h,
        &id,
        &path,
        vec![entry("home", PathKind::Dir, 0)],
        None,
    );

    assert!(
        !drawn(&mut h, "phone-browser-parent"),
        "a root has no parent row"
    );
}

#[gpui::test]
fn show_hidden_reveals_a_hidden_entry(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    browse(
        &mut h,
        SRC,
        vec![
            entry(".git", PathKind::Dir, 0),
            entry("a.txt", PathKind::File, 1),
        ],
        None,
    );
    assert!(
        !drawn(&mut h, "phone-browser-row-.git"),
        "hidden while Show hidden is off"
    );
    assert!(drawn(&mut h, "phone-browser-row-a.txt"));

    h.click_on("phone-browser-hidden");

    assert!(
        drawn(&mut h, "phone-browser-row-.git"),
        "Show hidden reveals it"
    );
    assert_eq!(
        listings(&sent(&mut h)),
        [],
        "the toggle asks the host nothing"
    );
}

#[gpui::test]
fn scrolling_near_the_end_asks_for_the_next_page_with_the_cursor(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    let many = (0..100)
        .map(|n| entry(&format!("f{n:03}.txt"), PathKind::File, 1))
        .collect();
    browse(&mut h, SRC, many, Some("c2"));
    assert_eq!(
        listings(&sent(&mut h)),
        [],
        "the top of a long page asks for no more"
    );

    h.scroll_into_view("phone-browser-list", "phone-browser-row-f099.txt");

    let (id, path, cursor) = one_listing(&mut h);
    assert_eq!(path, SRC);
    assert_eq!(cursor.as_deref(), Some("c2"), "the next page by its cursor");
    answer_listing(
        &mut h,
        &id,
        SRC,
        vec![entry("g000.txt", PathKind::File, 1)],
        None,
    );
    h.scroll_into_view("phone-browser-list", "phone-browser-row-g000.txt");
    assert_eq!(listings(&sent(&mut h)), [], "the listing ended");
}

#[gpui::test]
fn a_file_row_tap_fetches_it_and_opens_the_viewer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone_listing(
        cx,
        &dir,
        &copies,
        &[FEATURE_HOST_PATHS, FEATURE_HOST_BROWSE],
    );
    browse(&mut h, SRC, docs_and_notes(), None);

    h.click_on("phone-browser-row-notes.txt");

    let fetched: Vec<String> = sent(&mut h)
        .iter()
        .filter_map(|msg| match msg {
            ClientMessage::FetchHostFile { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(fetched, ["/home/me/src/notes.txt"], "the file is fetched");

    // The fetch saves its copy, as the fetch flow hands it to the phone.
    let saved = PhoneFetched {
        host: REMOTE.to_owned(),
        host_path: "/home/me/src/notes.txt".to_owned(),
        local_path: format!("{REMOTE}/root/home/me/src/notes.txt"),
        size: 5,
        host_label: REMOTE.to_owned(),
    };
    let path = copies.join(&saved.local_path);
    std::fs::create_dir_all(path.parent().expect("a copy has a folder")).expect("make its folder");
    std::fs::write(&path, "text\n").expect("write the copy");
    h.root_update(|root, _, cx| root.phone_file_saved(saved, cx));

    assert!(drawn(&mut h, "phone-viewer"), "the viewer opens on it");
    let header = phone_update(&mut h, |phone, _| phone.viewer_header());
    assert_eq!(header.map(|(name, ..)| name).as_deref(), Some("notes.txt"));

    h.click_on("phone-viewer-back");
    assert!(
        drawn(&mut h, "phone-browser"),
        "the viewer's Back returns to the browser"
    );
}

#[gpui::test]
fn a_listing_error_shows_its_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    open_browser(&mut h, "/root");
    let ids = info_ids(&sent(&mut h));
    answer_info(&mut h, &ids[0]);
    let (id, path, _) = one_listing(&mut h);

    h.host(&saved_remote()).send(DaemonMessage::DirListing {
        id,
        path,
        entries: Vec::new(),
        next_cursor: None,
        error: Some("permission denied".to_owned()),
    });

    assert!(drawn(&mut h, "phone-browser-error"));
    let line = phone_update(&mut h, |phone, _| phone.browser_error());
    assert_eq!(line.as_deref(), Some("Can't list /root: permission denied"));
}

#[gpui::test]
fn back_returns_to_the_session_page(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = open_phone(cx, &dir);
    let opened = phone_update(&mut h, |phone, cx| phone.open_session_on("s1", cx));
    assert!(opened, "Home lists s1");
    assert!(drawn(&mut h, "phone-session"));
    browse(&mut h, SRC, docs_and_notes(), None);
    assert!(drawn(&mut h, "phone-browser"), "the browser shows");
    assert!(!drawn(&mut h, "phone-session"), "over the session page");

    h.click_on("phone-browser-back");

    assert!(!drawn(&mut h, "phone-browser"), "the browser closed");
    assert!(
        drawn(&mut h, "phone-session"),
        "the session page shows again"
    );
}

#[gpui::test]
fn a_browser_opened_while_connecting_waits_then_lists(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::phone_with(cx, &dir, Arc::new(FakePushPlatform::default()));
    h.tokens().set(REMOTE, "t1").expect("seed a token");
    // The host is dialled but has not said Welcome yet.
    sent(&mut h);

    open_browser(&mut h, SRC);

    assert!(
        drawn(&mut h, "phone-browser-waiting"),
        "the page waits for the host"
    );
    assert!(!drawn(&mut h, "phone-browser-error"), "and says no error");
    let early = sent(&mut h);
    assert_eq!(listings(&early), [], "nothing is listed: {early:?}");
    assert_eq!(info_ids(&early), Vec::<String>::new(), "nothing is asked");

    let mut features = support::daemon_features();
    features.extend([
        FEATURE_HOST_PATHS.to_owned(),
        FEATURE_HOST_BROWSE.to_owned(),
    ]);
    h.host(&saved_remote()).connect_keeping_sent_with(features);

    let after = sent(&mut h);
    let ids = info_ids(&after);
    assert_eq!(ids.len(), 1, "the Welcome starts the listing: {after:?}");
    answer_info(&mut h, &ids[0]);
    let (id, path, _) = one_listing(&mut h);
    assert_eq!(path, SRC);
    answer_listing(&mut h, &id, SRC, docs_and_notes(), None);
    assert!(
        !drawn(&mut h, "phone-browser-waiting"),
        "the waiting line is gone"
    );
    assert!(drawn(&mut h, "phone-browser-row-docs"), "the rows show");
}

/// The longest a spec waits on the unpacker thread before it calls the run
/// hung; a passing run never waits it out.
const HANG_GUARD: Duration = Duration::from_secs(120);

/// The files the specs' fetched `docs` folder holds, relative to it.
const DOCS_FILES: &[(&str, &[u8])] = &[("guide.md", b"# guide\n\n"), ("sub/notes.txt", b"hello")];

/// The paths of the host folder fetches among `sent`.
fn folder_fetches(sent: &[ClientMessage]) -> Vec<(String, String)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::FetchHostFolder { id, path } => Some((id.clone(), path.clone())),
            _ => None,
        })
        .collect()
}

/// The line under the browser's header, while it shows one.
fn result_line(h: &mut Harness<'_>) -> Option<String> {
    phone_update(h, |phone, _| phone.browser_result())
}

/// A phone browsing [`SRC`], its `docs` folder and `notes.txt` file listed,
/// its copies under the returned folder. The unpacker is a real thread that
/// wakes the app, which gpui's test scheduler refuses unless the spec allows
/// parking.
fn browsing<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> (Harness<'a>, std::path::PathBuf) {
    let copies = dir.path().join("copies");
    let mut h = open_phone_listing(cx, dir, &copies, &[FEATURE_HOST_PATHS, FEATURE_HOST_BROWSE]);
    h.cx.executor().allow_parking();
    browse(&mut h, SRC, docs_and_notes(), None);
    (h, copies)
}

/// Long-presses the `docs` row and taps Fetch folder; returns the fetch's
/// id, checked to ask for `docs`' host path.
fn fetch_docs(h: &mut Harness<'_>) -> String {
    h.long_press_on("phone-browser-row-docs");
    h.click_on("phone-browser-sheet-fetch");
    let sent = sent(h);
    let fetches = folder_fetches(&sent);
    assert_eq!(fetches.len(), 1, "one folder fetch: {sent:?}");
    let (id, path) = fetches[0].clone();
    assert_eq!(path, "/home/me/src/docs");
    id
}

/// `files` packed as the daemon packs a folder: regular-file entries named
/// relative to the folder, in a tar inside a zstd frame.
fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
    let encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).expect("a zstd encoder");
    let mut builder = tar::Builder::new(encoder);
    for (name, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(u64::try_from(data.len()).expect("a small file"));
        header.set_mode(0o644);
        builder
            .append_data(&mut header, name, *data)
            .expect("pack a file");
    }
    builder
        .into_inner()
        .expect("end the archive")
        .finish()
        .expect("end the zstd frame")
}

/// The bytes `files` hold in all.
fn total(files: &[(&str, &[u8])]) -> u64 {
    files
        .iter()
        .map(|(_, data)| u64::try_from(data.len()).expect("a small file"))
        .sum()
}

/// Streams `files` as the folder `resolved` for the fetch `id`, as the
/// host would: started, the archive in one chunk, then done.
fn stream_folder(h: &mut Harness<'_>, id: &str, resolved: &str, files: &[(&str, &[u8])]) {
    let mut host = h.host(&saved_remote());
    host.send(DaemonMessage::FolderFetchStarted {
        id: id.to_owned(),
        resolved_path: resolved.to_owned(),
        files: u64::try_from(files.len()).expect("a few files"),
        bytes: total(files),
    });
    host.send(DaemonMessage::FileChunk {
        id: id.to_owned(),
        seq: 0,
        data_b64: base64::engine::general_purpose::STANDARD.encode(archive(files)),
    });
    host.send(DaemonMessage::FileFetchDone { id: id.to_owned() });
}

/// Lets the phone settle until `done` holds, the unpacker's thread
/// reporting back meanwhile; fails as hung past [`HANG_GUARD`].
fn settle_until(h: &mut Harness<'_>, what: &str, mut done: impl FnMut(&mut Harness<'_>) -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        h.cx.run_until_parked();
        if done(h) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "hung: waited {HANG_GUARD:?} for {what}"
        );
        std::thread::yield_now();
    }
}

#[gpui::test]
fn a_long_press_on_a_folder_row_offers_fetch_folder_and_cancel(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = browsing(cx, &dir);

    h.long_press_on("phone-browser-row-docs");

    assert!(drawn(&mut h, "phone-browser-sheet"), "the sheet opens");
    assert!(
        drawn(&mut h, "phone-browser-sheet-fetch"),
        "with Fetch folder"
    );
    assert!(drawn(&mut h, "phone-browser-sheet-cancel"), "and Cancel");
    assert_eq!(
        phone_update(&mut h, |phone, _| phone.browser_path().map(str::to_owned)).as_deref(),
        Some(SRC),
        "the press does not enter the folder"
    );
    assert_eq!(listings(&sent(&mut h)), [], "and lists nothing");
}

#[gpui::test]
fn cancel_closes_the_folder_sheet_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = browsing(cx, &dir);
    h.long_press_on("phone-browser-row-docs");

    h.click_on("phone-browser-sheet-cancel");

    assert!(!drawn(&mut h, "phone-browser-sheet"), "the sheet closes");
    let sent = sent(&mut h);
    assert_eq!(folder_fetches(&sent), [], "nothing is fetched: {sent:?}");
    assert_eq!(listings(&sent), [], "nothing is listed");
    assert_eq!(result_line(&mut h), None, "and no line shows");
}

#[gpui::test]
fn fetch_folder_fetches_the_folder_by_its_host_path_and_says_fetching(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = browsing(cx, &dir);

    fetch_docs(&mut h);

    assert!(!drawn(&mut h, "phone-browser-sheet"), "the sheet closes");
    assert_eq!(result_line(&mut h).as_deref(), Some("Fetching docs…"));
    assert!(drawn(&mut h, "phone-browser-result"), "under the header");
    let header = h.bounds("phone-browser-path");
    let line = h.bounds("phone-browser-result");
    let row = h.bounds("phone-browser-row-docs");
    assert!(
        header.bottom() <= line.top() && line.bottom() <= row.top(),
        "between the header and the rows"
    );
}

#[gpui::test]
fn a_fetched_folder_lands_under_its_host_path_as_one_folder_entry(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, copies) = browsing(cx, &dir);
    let id = fetch_docs(&mut h);

    stream_folder(&mut h, &id, "/home/me/src/docs", DOCS_FILES);
    settle_until(&mut h, "the folder's result line", |h| {
        result_line(h).is_some_and(|line| line.starts_with("Fetched"))
    });

    let folder = copies.join(REMOTE).join("root/home/me/src/docs");
    assert_eq!(
        std::fs::read(folder.join("guide.md")).expect("the folder's file"),
        DOCS_FILES[0].1
    );
    assert_eq!(
        std::fs::read(folder.join("sub/notes.txt")).expect("its subfolder's file"),
        DOCS_FILES[1].1
    );
    let index: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("fetched.json")).expect("the index is written"),
    )
    .expect("the index parses");
    let entries = index["entries"].as_array().expect("the index's entries");
    assert_eq!(entries.len(), 1, "one entry for the folder: {index}");
    assert_eq!(entries[0]["folder"], json!(true));
    assert_eq!(entries[0]["host_path"], json!("/home/me/src/docs"));
    assert_eq!(
        entries[0]["local_path"],
        json!(format!("{REMOTE}/root/home/me/src/docs"))
    );
    assert_eq!(entries[0]["size"], json!(total(DOCS_FILES)));
    assert_eq!(result_line(&mut h).as_deref(), Some("Fetched docs · 14 B"));
    assert!(!drawn(&mut h, "phone-viewer"), "a folder opens nothing");
    assert!(drawn(&mut h, "phone-browser"), "the browser stays");
}

#[gpui::test]
fn tapping_a_file_then_fetching_its_folder_leaves_one_entry(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, copies) = browsing(cx, &dir);
    h.click_on("phone-browser-row-docs");
    let (id, path, _) = one_listing(&mut h);
    answer_listing(
        &mut h,
        &id,
        &path,
        vec![entry("guide.md", PathKind::File, 9)],
        None,
    );
    h.click_on("phone-browser-row-guide.md");
    // The host sends the file whole, and the phone saves and opens it.
    let sent = sent(&mut h);
    let file_id = sent
        .iter()
        .find_map(|msg| match msg {
            ClientMessage::FetchHostFile { id, path } if path == "/home/me/src/docs/guide.md" => {
                Some(id.clone())
            }
            _ => None,
        })
        .expect("the file is fetched");
    let data = DOCS_FILES[0].1;
    let mut host = h.host(&saved_remote());
    host.send(DaemonMessage::FileFetchStarted {
        id: file_id.clone(),
        resolved_path: "/home/me/src/docs/guide.md".to_owned(),
        size: u64::try_from(data.len()).expect("a small file"),
    });
    host.send(DaemonMessage::FileChunk {
        id: file_id.clone(),
        seq: 0,
        data_b64: base64::engine::general_purpose::STANDARD.encode(data),
    });
    host.send(DaemonMessage::FileFetchDone { id: file_id });
    settle_until(&mut h, "the file's viewer", |h| drawn(h, "phone-viewer"));
    let file = copies.join(REMOTE).join("root/home/me/src/docs/guide.md");
    assert!(file.exists(), "the file's copy is saved");
    let before: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("fetched.json")).expect("the index is written"),
    )
    .expect("the index parses");
    assert_eq!(
        before["entries"].as_array().map(Vec::len),
        Some(1),
        "the file is kept first: {before}"
    );
    h.click_on("phone-viewer-back");
    h.click_on("phone-browser-parent");
    let (id, path, _) = one_listing(&mut h);
    answer_listing(&mut h, &id, &path, docs_and_notes(), None);

    let id = fetch_docs(&mut h);
    stream_folder(&mut h, &id, "/home/me/src/docs", DOCS_FILES);
    settle_until(&mut h, "the folder's result line", |h| {
        result_line(h).is_some_and(|line| line.starts_with("Fetched"))
    });

    let index: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("fetched.json")).expect("the index is written"),
    )
    .expect("the index parses");
    let entries = index["entries"].as_array().expect("the index's entries");
    assert_eq!(entries.len(), 1, "the folder replaced the file: {index}");
    assert_eq!(entries[0]["folder"], json!(true));
    assert_eq!(
        entries[0]["local_path"],
        json!(format!("{REMOTE}/root/home/me/src/docs"))
    );
}

#[gpui::test]
fn a_refused_folder_fetch_says_why(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = browsing(cx, &dir);
    fetch_docs(&mut h);

    // The same folder again, while its first fetch still runs.
    h.long_press_on("phone-browser-row-docs");
    h.click_on("phone-browser-sheet-fetch");

    let sent = sent(&mut h);
    assert_eq!(
        folder_fetches(&sent),
        [],
        "nothing more is fetched: {sent:?}"
    );
    assert_eq!(
        result_line(&mut h).as_deref(),
        Some("Couldn't fetch docs: Already fetching docs")
    );
    assert!(drawn(&mut h, "phone-browser-result"));
}

#[gpui::test]
fn the_result_line_clears_on_entering_another_folder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = browsing(cx, &dir);
    fetch_docs(&mut h);
    assert!(result_line(&mut h).is_some());

    h.click_on("phone-browser-row-docs");
    let (id, path, _) = one_listing(&mut h);
    answer_listing(&mut h, &id, &path, Vec::new(), None);

    assert_eq!(result_line(&mut h), None, "the line clears");
    assert!(!drawn(&mut h, "phone-browser-result"));
}

#[gpui::test]
fn a_long_press_on_a_file_row_offers_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = browsing(cx, &dir);

    h.long_press_on("phone-browser-row-notes.txt");

    assert!(!drawn(&mut h, "phone-browser-sheet"), "no sheet opens");
    let sent = sent(&mut h);
    assert_eq!(folder_fetches(&sent), [], "no folder is fetched: {sent:?}");
    assert_eq!(result_line(&mut h), None);
}

#[gpui::test]
fn a_host_without_host_browse_never_gets_a_list_dir(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let copies = dir.path().join("copies");
    let mut h = open_phone_listing(cx, &dir, &copies, &[FEATURE_HOST_PATHS]);

    open_browser(&mut h, SRC);

    let sent = sent(&mut h);
    assert_eq!(listings(&sent), [], "no listing: {sent:?}");
    assert_eq!(
        info_ids(&sent),
        Vec::<String>::new(),
        "and no host info request"
    );
    assert!(drawn(&mut h, "phone-browser-error"), "the page says why");
}
