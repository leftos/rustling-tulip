//! Remote folder fetch specs: a Ctrl+clicked path link on a host that
//! sends folders is first asked what it names; a folder is streamed as one
//! archive, unpacked under the host's and repo's folder on this PC, its
//! toast counting the files and offering Cancel, and no stage is left when
//! the fetch is cancelled, fails or loses its connection. A host too old to
//! send folders says so.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;
use gpui::{Modifiers, TestAppContext};
use protocol::{ClientMessage, DaemonMessage, PathKind, SessionSnapshot};
use rustling_tulip_native::{
    Connection, NetEvent, ToastAction, ToastKind, ToastSpec, toast_action_selector,
};
use serde_json::json;
use support::{
    Fixture, Harness, Opened, REMOTE, TestDir, pane, remote_pane, repo, saved_remote, seed_ui,
    session, split, tab,
};

const HOST: &str = "box.tail1234.ts.net";

/// The member worktree of `session("s1").worktree("r1")`.
const WORKTREE: &str = "C:/wt/x";

/// The longest a spec waits on the unpacker thread before it calls the run
/// hung; a passing run never waits it out.
const HANG_GUARD: Duration = Duration::from_secs(120);

/// The warning about a host too old to send folders.
fn too_old() -> String {
    format!(
        "{HOST} runs an older rustling-tulip that cannot send folders. Update it to fetch folders."
    )
}

/// This PC lists `a1`; tab `t1` splits it (pane `p1`) beside [`REMOTE`]'s
/// session `s` (pane `p2`, focused). [`REMOTE`] is saved and connected with
/// the fake daemon's features and `extra`, listing the repo `r1`; downloads
/// go under `dir`, and the download root is returned.
fn linked_with<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    s: SessionSnapshot,
    extra: &[&str],
) -> (Harness<'a>, PathBuf) {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let id = s.id.clone();
    let grid = split(
        protocol::SplitDirection::Horizontal,
        pane("p1", Some("a1")),
        remote_pane("p2", &id),
    );
    let fixture = Fixture {
        sessions: vec![session("a1").build()],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    h.answer_scrollback("a1", b"");
    let mut host = h.host(&saved_remote());
    host.connect_with_features(extra);
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/repos/r1")],
    });
    host.send(DaemonMessage::Sessions { sessions: vec![s] });
    host.answer_scrollback(&id, b"");
    h.cx.update(|window, _| window.activate_window());
    let at = h.cell_center("p2", 0, 0);
    h.click(at, Modifiers::none());
    assert_eq!(
        h.root(|root, _| root.focused_pane()).as_deref(),
        Some("p2"),
        "the click focuses the remote pane"
    );
    h.sent();
    h.host(&saved_remote()).sent();
    let root = dir.path().join("remote-files");
    let set = root.clone();
    h.root_update(move |view, _, _| view.set_remote_files_root(set));
    h.sent();
    (h, root)
}

/// [`linked_with`] a session on [`WORKTREE`], on a host that sends folders.
fn linked<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> (Harness<'a>, PathBuf) {
    linked_with(
        cx,
        dir,
        session("s1").worktree("r1").build(),
        &[protocol::FEATURE_FOLDER_FETCH],
    )
}

/// [`linked_with`] a session on [`WORKTREE`], on a host too old to send
/// folders.
fn linked_old<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> (Harness<'a>, PathBuf) {
    linked_with(cx, dir, session("s1").worktree("r1").build(), &[])
}

/// [`linked_old`] a session in `C:/wt/x/sub`, so a relative link reads
/// there first, then under the worktree.
fn linked_old_sub<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> (Harness<'a>, PathBuf) {
    linked_with(
        cx,
        dir,
        session("s1").shell("C:/wt/x/sub").worktree("r1").build(),
        &[],
    )
}

/// Writes `text` to `s1`'s pane `p2` and Ctrl+clicks column `col` of its
/// first row.
fn click_link(h: &mut Harness<'_>, text: &str, col: usize) {
    h.host(&saved_remote()).pty("s1", text.as_bytes());
    click_again(h, col);
}

/// Ctrl+clicks column `col` of `p2`'s first row.
fn click_again(h: &mut Harness<'_>, col: usize) {
    let on = h.cell_center("p2", col, 0);
    h.click(on, Modifiers::secondary_key());
}

/// The fetch messages (stats, fetches and cancels) sent to the saved host
/// since the last look.
fn fetch_messages(h: &mut Harness<'_>) -> Vec<ClientMessage> {
    h.host(&saved_remote())
        .sent()
        .into_iter()
        .filter(|msg| {
            matches!(
                msg,
                ClientMessage::StatPath { .. }
                    | ClientMessage::FetchFolder { .. }
                    | ClientMessage::FetchFile { .. }
                    | ClientMessage::CancelFetch { .. }
            )
        })
        .collect()
}

/// The one `StatPath` sent since the last look, checked to ask about `path`
/// in [`WORKTREE`]; returns its id.
fn one_stat(h: &mut Harness<'_>, path: &str) -> String {
    let sent = fetch_messages(h);
    let [
        ClientMessage::StatPath {
            id,
            repo_id,
            worktree_path,
            path: asked,
        },
    ] = sent.as_slice()
    else {
        panic!("one StatPath is sent, got {sent:?}");
    };
    assert_eq!(repo_id, "r1");
    assert_eq!(worktree_path.as_deref(), Some(WORKTREE));
    assert_eq!(asked, path);
    id.clone()
}

/// The one `FetchFolder` sent since the last look, checked to ask for
/// `path` in [`WORKTREE`]; returns its id.
fn one_folder_fetch(h: &mut Harness<'_>, path: &str) -> String {
    let sent = fetch_messages(h);
    let [
        ClientMessage::FetchFolder {
            id,
            repo_id,
            worktree_path,
            path: asked,
        },
    ] = sent.as_slice()
    else {
        panic!("one FetchFolder is sent, got {sent:?}");
    };
    assert_eq!(repo_id, "r1");
    assert_eq!(worktree_path.as_deref(), Some(WORKTREE));
    assert_eq!(asked, path);
    id.clone()
}

/// The one `FetchFile` sent since the last look, checked to ask for `path`
/// in [`WORKTREE`]; returns its id.
fn one_file_fetch(h: &mut Harness<'_>, path: &str) -> String {
    let sent = fetch_messages(h);
    let [
        ClientMessage::FetchFile {
            id,
            worktree_path,
            path: asked,
            ..
        },
    ] = sent.as_slice()
    else {
        panic!("one FetchFile is sent, got {sent:?}");
    };
    assert_eq!(worktree_path.as_deref(), Some(WORKTREE));
    assert_eq!(asked, path);
    id.clone()
}

/// The host answers the stat `id` with `kind`.
fn stat(h: &mut Harness<'_>, id: &str, kind: PathKind) {
    h.host(&saved_remote()).send(DaemonMessage::PathStat {
        id: id.to_owned(),
        kind,
        size: 0,
    });
}

/// Clicks the folder link `text` at `col`, answers its stat of `path` as a
/// folder, and returns the folder fetch's id.
fn fetch_folder(h: &mut Harness<'_>, text: &str, col: usize, path: &str) -> String {
    click_link(h, text, col);
    let id = one_stat(h, path);
    stat(h, &id, PathKind::Dir);
    one_folder_fetch(h, path)
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

/// The host's `FolderFetchStarted` for the fetch `id` of `files`.
fn started(id: &str, files: &[(&str, &[u8])]) -> DaemonMessage {
    DaemonMessage::FolderFetchStarted {
        id: id.to_owned(),
        resolved_path: r"\\?\C:\wt\x\src".to_owned(),
        files: u64::try_from(files.len()).expect("a few files"),
        bytes: files
            .iter()
            .map(|(_, data)| u64::try_from(data.len()).expect("a small file"))
            .sum(),
    }
}

/// The archive `packed` cut into three `FileChunk`s of the fetch `id`.
fn chunks(id: &str, packed: &[u8]) -> Vec<DaemonMessage> {
    let third = packed.len().div_ceil(3);
    packed
        .chunks(third)
        .zip(0u64..)
        .map(|(data, seq)| DaemonMessage::FileChunk {
            id: id.to_owned(),
            seq,
            data_b64: base64::engine::general_purpose::STANDARD.encode(data),
        })
        .collect()
}

/// Streams `files` for the fetch `id` as the host would: started, the
/// archive's chunks, then done.
fn stream_folder(h: &mut Harness<'_>, id: &str, files: &[(&str, &[u8])]) {
    let mut host = h.host(&saved_remote());
    host.send(started(id, files));
    for chunk in chunks(id, &archive(files)) {
        host.send(chunk);
    }
    host.send(DaemonMessage::FileFetchDone { id: id.to_owned() });
}

/// Lets the view settle until `done` holds, the unpacker's thread reporting
/// back meanwhile; fails as hung past [`HANG_GUARD`].
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

/// Every toast's title and detail, oldest first.
fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.title.clone(), toast.detail.clone()))
            .collect()
    })
}

/// The id of the toast keyed `key`, if one shows.
fn toast_keyed(h: &mut Harness<'_>, key: &str) -> Option<u64> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .find(|toast| toast.key.as_deref() == Some(key))
            .map(|toast| toast.id)
    })
}

/// How many toasts are titled `title`.
fn titled(h: &mut Harness<'_>, title: &str) -> usize {
    toasts(h).iter().filter(|(t, _)| t == title).count()
}

/// Whether a toast titled `title` shows.
fn shows(h: &mut Harness<'_>, title: &str) -> bool {
    titled(h, title) > 0
}

/// The files and folders directly in `dir`, by name; none when it is missing.
fn names_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| {
            entry
                .expect("read a folder entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// Where the repo `r1`'s files land under the download root `root`.
fn repo_folder(root: &Path) -> PathBuf {
    root.join(HOST).join("r1")
}

/// The stage the fetch `id` unpacks `target` into.
fn stage_of(target: &Path, id: &str) -> PathBuf {
    target.with_file_name(format!(".{id}.part"))
}

const FILES: [(&str, &[u8]); 2] = [("a.txt", b"first file\n"), ("nested/b.rs", b"fn b() {}\n")];

#[gpui::test]
fn a_folder_link_stats_then_fetches_the_folder_and_unpacks_it_under_the_host_and_repo(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let (mut h, root) = linked(cx, &dir);

    click_link(&mut h, "see C:/wt/x/src", 6);
    let stat_id = one_stat(&mut h, "src");
    stat(&mut h, &stat_id, PathKind::Dir);
    let id = one_folder_fetch(&mut h, "src");
    stream_folder(&mut h, &id, &FILES);
    settle_until(&mut h, "the folder to be fetched", |h| {
        shows(h, "Fetched folder src")
    });

    let target = repo_folder(&root).join("src");
    assert_eq!(
        fs::read(target.join("a.txt")).expect("a.txt lands"),
        b"first file\n"
    );
    assert_eq!(
        fs::read(target.join("nested").join("b.rs")).expect("nested/b.rs lands"),
        b"fn b() {}\n"
    );
    assert_eq!(
        names_in(&repo_folder(&root)),
        ["src"],
        "no stage is left beside the folder"
    );
    assert!(h.opened().is_empty(), "a fetched folder does not open");
}

#[gpui::test]
fn a_file_link_stats_then_fetches_the_file_as_before(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = linked(cx, &dir);

    click_link(&mut h, "see C:/wt/x/docs/notes.txt", 6);
    let stat_id = one_stat(&mut h, "docs/notes.txt");
    stat(&mut h, &stat_id, PathKind::File);
    let id = one_file_fetch(&mut h, "docs/notes.txt");
    let mut host = h.host(&saved_remote());
    host.send(DaemonMessage::FileFetchStarted {
        id: id.clone(),
        resolved_path: r"\\?\C:\wt\x\docs\notes.txt".to_owned(),
        size: 5,
    });
    host.send(DaemonMessage::FileChunk {
        id: id.clone(),
        seq: 0,
        data_b64: base64::engine::general_purpose::STANDARD.encode(b"notes"),
    });
    host.send(DaemonMessage::FileFetchDone { id });

    let saved = repo_folder(&root).join("docs").join("notes.txt");
    assert_eq!(fs::read(&saved).expect("the file is saved"), b"notes");
    assert_eq!(h.opened(), [Opened::DefaultApp(saved)]);
}

#[gpui::test]
fn a_missing_stat_tries_the_next_reading(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked_with(
        cx,
        &dir,
        session("s1").shell("C:/wt/x/sub").worktree("r1").build(),
        &[protocol::FEATURE_FOLDER_FETCH],
    );

    click_link(&mut h, "wrote docs/api", 9);
    let first = one_stat(&mut h, "sub/docs/api");
    stat(&mut h, &first, PathKind::Missing);
    let second = one_stat(&mut h, "docs/api");
    assert!(
        !toasts(&mut h)
            .iter()
            .any(|(title, _)| title.starts_with("Couldn't")),
        "a missed reading is not a failure: {:?}",
        toasts(&mut h)
    );
    stat(&mut h, &second, PathKind::Dir);
    one_folder_fetch(&mut h, "docs/api");
}

#[gpui::test]
fn a_missing_root_link_says_the_folder_is_not_on_the_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked(cx, &dir);

    click_link(&mut h, "see C:/wt/x", 6);
    let id = one_stat(&mut h, "");
    stat(&mut h, &id, PathKind::Missing);

    let failures: Vec<(String, Option<String>)> = toasts(&mut h)
        .into_iter()
        .filter(|(title, _)| title.starts_with("Couldn't"))
        .collect();
    assert_eq!(
        failures,
        [(
            "Couldn't fetch r1".to_owned(),
            Some(format!("r1 is not on {HOST}."))
        )],
        "a missing root is named, not called outside the session"
    );
}

#[gpui::test]
fn a_link_click_shows_a_toast_before_any_stat_answers(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked(cx, &dir);

    click_link(&mut h, "see C:/wt/x/docs/notes.txt", 6);
    one_stat(&mut h, "docs/notes.txt");

    assert_eq!(
        toasts(&mut h),
        [(format!("Fetching notes.txt from {HOST}"), None)],
        "the click shows a toast before the host answers"
    );
}

#[gpui::test]
fn a_file_link_updates_that_toast_in_place(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked(cx, &dir);
    let title = format!("Fetching notes.txt from {HOST}");

    click_link(&mut h, "see C:/wt/x/docs/notes.txt", 6);
    let stat_id = one_stat(&mut h, "docs/notes.txt");
    let early = toast_titled(&mut h, &title).expect("the early toast shows");
    stat(&mut h, &stat_id, PathKind::File);
    one_file_fetch(&mut h, "docs/notes.txt");

    assert_eq!(titled(&mut h, &title), 1, "{:?}", toasts(&mut h));
    assert_eq!(
        toast_titled(&mut h, &title),
        Some(early),
        "the file fetch updates the early toast in place"
    );
}

#[gpui::test]
fn a_file_link_toast_shows_done_of_size_and_a_bar_as_it_streams(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked(cx, &dir);
    let title = format!("Fetching notes.txt from {HOST}");

    click_link(&mut h, "see C:/wt/x/docs/notes.txt", 6);
    let stat_id = one_stat(&mut h, "docs/notes.txt");
    let early = toast_titled(&mut h, &title).expect("the early toast shows");
    assert_eq!(
        toasts(&mut h),
        [(title.clone(), None)],
        "no detail before the size is known"
    );
    assert_eq!(
        h.bounds(&format!("toast-progress-{early}")).size.width,
        gpui::px(0.0),
        "no bar before the size is known"
    );

    stat(&mut h, &stat_id, PathKind::File);
    let id = one_file_fetch(&mut h, "docs/notes.txt");
    h.host(&saved_remote()).send(file_started(&id, 100));
    let toast = toast_titled(&mut h, &title).expect("the toast shows the size");
    assert_eq!(
        toasts(&mut h),
        [(title.clone(), Some("0 B of 100 B".to_owned()))],
        "the toast counts the bytes once the size is known"
    );
    assert!(
        h.bounds(&format!("toast-progress-{toast}")).size.width > gpui::px(0.0),
        "the bar is drawn under the detail"
    );
    let empty = h.bounds(&format!("toast-progress-fill-{toast}")).size.width;

    h.host(&saved_remote()).send(file_chunk(&id, 0, &[0u8; 25]));
    assert_eq!(
        toasts(&mut h),
        [(title.clone(), Some("25 B of 100 B".to_owned()))],
        "the detail moves with the bytes"
    );
    let filled = h.bounds(&format!("toast-progress-fill-{toast}")).size.width;
    assert!(
        filled > empty,
        "the fill widens from {empty:?} to {filled:?}"
    );

    h.host(&saved_remote()).send(file_chunk(&id, 1, &[0u8; 75]));
    h.host(&saved_remote())
        .send(DaemonMessage::FileFetchDone { id });
    settle_until(&mut h, "the file's done toast", |h| {
        shows(h, "Fetched notes.txt")
    });
}

#[gpui::test]
fn cancel_on_a_file_link_toast_sends_cancel_and_removes_the_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked_old_sub(cx, &dir);
    let title = format!("Fetching notes.md from {HOST}");

    click_link(&mut h, "wrote docs/notes.md", 9);
    let id = one_file_fetch(&mut h, "sub/docs/notes.md");
    let toast = toast_titled(&mut h, &title).expect("the link's toast shows");
    h.click_on(&toast_action_selector(toast, "cancel"));
    h.cx.run_until_parked();

    let sent = fetch_messages(&mut h);
    assert!(
        matches!(sent.as_slice(), [ClientMessage::CancelFetch { id: cancelled }] if *cancelled == id),
        "the host is told to stop: {sent:?}"
    );
    assert_eq!(toast_titled(&mut h, &title), None, "the toast is gone");
    assert!(
        toasts(&mut h).is_empty(),
        "no other toast shows: {:?}",
        toasts(&mut h)
    );

    h.host(&saved_remote()).send(DaemonMessage::FileFetchError {
        id,
        error: "gone".to_owned(),
    });
    assert!(
        fetch_messages(&mut h).is_empty(),
        "the cancelled link's other readings are not fetched"
    );
}

#[gpui::test]
fn a_refused_retry_leaves_no_fetching_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked_old_sub(cx, &dir);
    let title = format!("Fetching notes.md from {HOST}");

    click_link(&mut h, "wrote docs/notes.md", 9);
    let first = one_file_fetch(&mut h, "sub/docs/notes.md");
    h.host(&saved_remote()).send(DaemonMessage::FileFetchError {
        id: first,
        error: "missing".to_owned(),
    });
    one_file_fetch(&mut h, "docs/notes.md");

    click_again(&mut h, 9);
    let second = one_file_fetch(&mut h, "sub/docs/notes.md");
    h.host(&saved_remote()).send(DaemonMessage::FileFetchError {
        id: second,
        error: "missing".to_owned(),
    });

    assert_eq!(
        titled(&mut h, "Already fetching notes.md"),
        1,
        "the second click's next reading is refused: {:?}",
        toasts(&mut h)
    );
    assert_eq!(
        titled(&mut h, &title),
        1,
        "only the fetch in flight keeps its toast: {:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn cancel_on_a_toast_with_no_fetch_dismisses_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked(cx, &dir);
    // A link's progress toast whose fetch already ended: no entry holds its
    // key any more.
    let spec = ToastSpec {
        kind: ToastKind::Info,
        title: format!("Fetching notes.txt from {HOST}"),
        detail: Some("0 B of 8 B".to_owned()),
        key: Some("fetch-group-gone-1".to_owned()),
        sticky: true,
        actions: vec![ToastAction {
            id: "cancel",
            label: "Cancel",
            danger: false,
        }],
    };
    let toast = h.root_update(|root, _, cx| root.push_toast_spec(spec, cx));

    h.click_on(&toast_action_selector(toast, "cancel"));
    h.cx.run_until_parked();

    assert!(
        h.root(|root, _| root.toasts().iter().all(|shown| shown.id != toast)),
        "the toast goes: {:?}",
        toasts(&mut h)
    );
    assert!(
        fetch_messages(&mut h).is_empty(),
        "a key that holds no fetch cancels nothing"
    );
}

#[gpui::test]
fn a_finishing_file_toast_drops_cancel(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked_old(cx, &dir);
    let title = format!("Fetching notes.txt from {HOST}");

    click_link(&mut h, "see C:/wt/x/docs/notes.txt", 6);
    let id = one_file_fetch(&mut h, "docs/notes.txt");
    h.host(&saved_remote()).send(file_started(&id, 8));
    assert_eq!(
        actions_of(&mut h, &title),
        ["cancel"],
        "the streaming toast offers Cancel"
    );

    h.host(&saved_remote())
        .send(file_chunk(&id, 0, b"notes.tx"));
    assert_eq!(
        toasts(&mut h),
        [(title.clone(), Some("8 B of 8 B".to_owned()))],
        "the toast counts the bytes so far"
    );

    h.host(&saved_remote())
        .send(DaemonMessage::FileFetchDone { id });
    assert!(
        actions_of(&mut h, &title).is_empty(),
        "once Done went to the writer the toast offers no Cancel"
    );
    assert_eq!(
        toasts(&mut h),
        [(title, Some("8 B of 8 B".to_owned()))],
        "the toast keeps its detail as it finishes"
    );
}

#[gpui::test]
fn closing_a_file_link_toast_keeps_it_closed_until_the_fetch_ends(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked(cx, &dir);
    let title = format!("Fetching notes.txt from {HOST}");

    click_link(&mut h, "see C:/wt/x/docs/notes.txt", 6);
    let stat_id = one_stat(&mut h, "docs/notes.txt");
    stat(&mut h, &stat_id, PathKind::File);
    let id = one_file_fetch(&mut h, "docs/notes.txt");
    h.host(&saved_remote()).send(file_started(&id, 8));
    assert_eq!(
        toasts(&mut h),
        [(title.clone(), Some("0 B of 8 B".to_owned()))],
        "the toast shows the size before the bytes"
    );
    let toast = toast_titled(&mut h, &title).expect("the file's toast shows");
    h.click_on(&format!("toast-close-{toast}"));
    h.cx.run_until_parked();
    assert_eq!(toast_titled(&mut h, &title), None, "the × closes the toast");

    h.host(&saved_remote())
        .send(file_chunk(&id, 0, b"notes.tx"));
    assert_eq!(
        toast_titled(&mut h, &title),
        None,
        "a chunk does not bring the toast back"
    );

    h.host(&saved_remote())
        .send(DaemonMessage::FileFetchDone { id });
    settle_until(&mut h, "the file's done toast", |h| {
        shows(h, "Fetched notes.txt")
    });
}

#[gpui::test]
fn a_folder_link_replaces_that_toast_with_the_folder_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked(cx, &dir);

    let id = fetch_folder(&mut h, "see C:/wt/x/src", 6, "src");

    assert_eq!(
        toasts(&mut h),
        [(format!("Fetching folder src from {HOST}"), None)],
        "the folder's toast takes over from the early one"
    );
    assert!(
        toast_keyed(&mut h, &id).is_some(),
        "the toast is the fetch's"
    );
}

#[gpui::test]
fn a_missing_link_replaces_that_toast_with_the_warning(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked(cx, &dir);

    click_link(&mut h, "see C:/wt/x", 6);
    let id = one_stat(&mut h, "");
    assert!(
        shows(&mut h, &format!("Fetching r1 from {HOST}")),
        "{:?}",
        toasts(&mut h)
    );
    stat(&mut h, &id, PathKind::Missing);

    assert_eq!(
        toasts(&mut h),
        [(
            "Couldn't fetch r1".to_owned(),
            Some(format!("r1 is not on {HOST}."))
        )],
        "only the warning shows"
    );
}

#[gpui::test]
fn a_link_to_the_worktree_root_fetches_the_whole_worktree(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = linked(cx, &dir);

    let id = fetch_folder(&mut h, "see C:/wt/x", 6, "");
    assert!(
        shows(&mut h, &format!("Fetching folder r1 from {HOST}")),
        "the root folder goes by its repo's name: {:?}",
        toasts(&mut h)
    );
    stream_folder(&mut h, &id, &FILES);
    settle_until(&mut h, "the worktree to be fetched", |h| {
        shows(h, "Fetched folder r1")
    });

    let target = repo_folder(&root);
    assert_eq!(
        fs::read(target.join("a.txt")).expect("a.txt lands"),
        b"first file\n"
    );
    assert_eq!(names_in(&target), ["a.txt", "nested"]);
    assert_eq!(
        names_in(&root.join(HOST)),
        ["r1"],
        "no stage is left beside it"
    );
}

#[gpui::test]
fn the_folder_toast_counts_files_and_shows_progress(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked(cx, &dir);

    let id = fetch_folder(&mut h, "see C:/wt/x/src", 6, "src");
    assert!(
        toasts(&mut h).contains(&(format!("Fetching folder src from {HOST}"), None)),
        "the fetch says it started: {:?}",
        toasts(&mut h)
    );
    assert!(
        toast_keyed(&mut h, &id).is_some(),
        "the toast is the fetch's"
    );

    h.host(&saved_remote()).send(started(&id, &FILES));
    assert!(
        toasts(&mut h).contains(&(
            format!("Fetching folder src from {HOST} (2 files, 21 B)"),
            Some("0 B of 21 B".to_owned())
        )),
        "the toast counts the files and the unpacked bytes: {:?}",
        toasts(&mut h)
    );

    let mut host = h.host(&saved_remote());
    for chunk in chunks(&id, &archive(&FILES)) {
        host.send(chunk);
    }
    host.send(DaemonMessage::FileFetchDone { id: id.clone() });
    settle_until(&mut h, "the folder to be fetched", |h| {
        shows(h, "Fetched folder src")
    });
    assert!(
        toasts(&mut h).contains(&(
            "Fetched folder src".to_owned(),
            Some(format!("2 files from {HOST}"))
        )),
        "{:?}",
        toasts(&mut h)
    );
    assert!(
        !toasts(&mut h)
            .iter()
            .any(|(title, _)| title.starts_with("Fetching folder")),
        "the progress toast became the done toast: {:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn cancel_on_the_folder_toast_sends_cancel_and_deletes_the_stage(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = linked(cx, &dir);

    let id = fetch_folder(&mut h, "see C:/wt/x/src", 6, "src");
    let target = repo_folder(&root).join("src");
    let stage = stage_of(&target, &id);
    h.host(&saved_remote()).send(started(&id, &FILES));
    let first = chunks(&id, &archive(&FILES)).remove(0);
    h.host(&saved_remote()).send(first);
    settle_until(&mut h, "the unpacker to make its stage", |_| stage.is_dir());

    let toast = toast_keyed(&mut h, &id).expect("the folder's toast shows");
    h.click_on(&toast_action_selector(toast, "cancel"));
    h.cx.run_until_parked();

    let sent = fetch_messages(&mut h);
    assert!(
        matches!(sent.as_slice(), [ClientMessage::CancelFetch { id: cancelled }] if *cancelled == id),
        "the host is told to stop: {sent:?}"
    );
    assert_eq!(toast_keyed(&mut h, &id), None, "the toast is gone");
    settle_until(&mut h, "the stage to be deleted", |_| !stage.exists());
    assert!(!target.exists(), "nothing lands");
    assert!(
        !toasts(&mut h)
            .iter()
            .any(|(title, _)| title.starts_with("Couldn't")),
        "a cancel is no failure: {:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn a_long_toast_title_wraps_and_keeps_the_close_button_on_the_card(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked(cx, &dir);
    let spec = ToastSpec {
        kind: ToastKind::Info,
        title: format!("Fetching folder src from builder@{HOST} (20000 files, 1.5 GB)"),
        detail: Some("0 B of 1.5 GB".to_owned()),
        key: Some("long-title".to_owned()),
        sticky: true,
        actions: Vec::new(),
    };
    let toast = h.root_update(|root, _, cx| root.push_toast_spec(spec, cx));

    let card = h.bounds(&format!("toast-{toast}"));
    let close = h.bounds(&format!("toast-close-{toast}"));
    assert!(
        close.origin.x >= card.origin.x
            && close.origin.x + close.size.width <= card.origin.x + card.size.width,
        "the × lies inside the card: card {card:?}, × {close:?}"
    );
}

#[gpui::test]
fn closing_the_folder_toast_keeps_its_progress_closed_until_the_fetch_ends(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let (mut h, root) = linked(cx, &dir);
    let hold = h.root_update(|view, _, _| view.hold_folder_unpackers());

    let id = fetch_folder(&mut h, "see C:/wt/x/src", 6, "src");
    h.host(&saved_remote()).send(started(&id, &FILES));
    let toast = toast_keyed(&mut h, &id).expect("the folder's toast shows");
    h.click_on(&format!("toast-close-{toast}"));
    h.cx.run_until_parked();
    assert_eq!(toast_keyed(&mut h, &id), None, "the × closes the toast");

    let packed = chunks(&id, &archive(&FILES));
    let mut host = h.host(&saved_remote());
    for chunk in packed.iter().cloned() {
        host.send(chunk);
    }
    host.send(DaemonMessage::FileFetchDone { id: id.clone() });
    hold.release_start();
    let target = repo_folder(&root).join("src");
    settle_until(&mut h, "the folder to be unpacked in place", |_| {
        target.is_dir()
    });
    let last = packed.last().cloned().expect("a chunk");
    h.host(&saved_remote()).send(last);
    assert_eq!(
        toast_keyed(&mut h, &id),
        None,
        "more unpacked bytes do not bring the progress back: {:?}",
        toasts(&mut h)
    );

    hold.release_outcome();
    settle_until(&mut h, "the folder's done toast", |h| {
        shows(h, "Fetched folder src")
    });
}

/// The detail and action ids of the toast keyed `key`, if one shows.
fn keyed_detail_and_actions(
    h: &mut Harness<'_>,
    key: &str,
) -> Option<(Option<String>, Vec<&'static str>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .find(|toast| toast.key.as_deref() == Some(key))
            .map(|toast| {
                (
                    toast.detail.clone(),
                    toast.actions.iter().map(|action| action.id).collect(),
                )
            })
    })
}

/// Fetches the folder `src`, streaming [`FILES`] through `FileFetchDone`
/// (a spec holds its unpacker first); returns the fetch's id.
fn fetch_streamed(h: &mut Harness<'_>) -> String {
    let id = fetch_folder(h, "see C:/wt/x/src", 6, "src");
    stream_folder(h, &id, &FILES);
    id
}

#[gpui::test]
fn a_finishing_folder_toast_drops_cancel_and_says_finishing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked(cx, &dir);
    let hold = h.root_update(|view, _, _| view.hold_folder_unpackers());

    let id = fetch_streamed(&mut h);

    assert_eq!(
        keyed_detail_and_actions(&mut h, &id),
        Some((Some("Finishing: 0 B of 21 B".to_owned()), Vec::new())),
        "once Done went to the unpacker the toast offers no Cancel"
    );
    drop(hold);
    settle_until(&mut h, "the folder's done toast", |h| {
        shows(h, "Fetched folder src")
    });
}

#[gpui::test]
fn a_finishing_folder_toast_keeps_counting_after_the_last_chunk(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = linked(cx, &dir);
    let hold = h.root_update(|view, _, _| view.hold_folder_unpackers());
    let id = fetch_streamed(&mut h);
    let target = repo_folder(&root).join("src");

    hold.release_start();
    settle_until(&mut h, "the folder to be unpacked in place", |_| {
        target.is_dir()
    });
    h.cx.executor().advance_clock(Duration::from_millis(300));
    h.cx.run_until_parked();

    assert_eq!(
        keyed_detail_and_actions(&mut h, &id).and_then(|(detail, _)| detail),
        Some("Finishing: 21 B of 21 B".to_owned()),
        "the toast shows what the unpacker did after the last chunk: {:?}",
        toasts(&mut h)
    );
    hold.release_outcome();
    settle_until(&mut h, "the folder's done toast", |h| {
        shows(h, "Fetched folder src")
    });
}

#[gpui::test]
fn a_lost_connection_deletes_the_folder_stage(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = linked(cx, &dir);

    let id = fetch_folder(&mut h, "see C:/wt/x/src", 6, "src");
    let target = repo_folder(&root).join("src");
    let stage = stage_of(&target, &id);
    h.host(&saved_remote()).send(started(&id, &FILES));
    let first = chunks(&id, &archive(&FILES)).remove(0);
    h.host(&saved_remote()).send(first);
    settle_until(&mut h, "the unpacker to make its stage", |_| stage.is_dir());

    h.host(&saved_remote())
        .event(NetEvent::State(Connection::for_target(saved_remote())));

    settle_until(&mut h, "the stage to be deleted", |_| !stage.exists());
    assert!(!target.exists(), "nothing lands");
    assert_eq!(toast_keyed(&mut h, &id), None, "the folder's toast is gone");
    let shown = toasts(&mut h);
    assert!(
        shown
            .iter()
            .any(|(title, detail)| title == "Couldn't fetch src"
                && detail.as_deref().is_some_and(
                    |detail| detail.contains(&format!("The connection to {HOST} closed."))
                )),
        "the fetch ends with a failure toast: {shown:?}"
    );
}

#[gpui::test]
fn a_host_refusal_shows_the_hosts_reason(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = linked(cx, &dir);

    let id = fetch_folder(&mut h, "see C:/wt/x/src", 6, "src");
    h.host(&saved_remote()).send(DaemonMessage::FileFetchError {
        id: id.clone(),
        error: "the folder has more than 20000 files".to_owned(),
    });

    assert!(
        toasts(&mut h).contains(&(
            "Couldn't fetch src".to_owned(),
            Some("the folder has more than 20000 files".to_owned())
        )),
        "the host's reason is the detail: {:?}",
        toasts(&mut h)
    );
    assert_eq!(toast_keyed(&mut h, &id), None, "the folder's toast is gone");
    assert!(names_in(&root).is_empty(), "nothing is written");
}

#[gpui::test]
fn a_host_without_folder_fetch_reports_a_root_link_as_too_old(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = linked_old(cx, &dir);

    click_link(&mut h, "see C:/wt/x", 6);

    assert!(
        fetch_messages(&mut h).is_empty(),
        "nothing is asked of the host"
    );
    assert_eq!(
        toasts(&mut h),
        [("Couldn't fetch r1".to_owned(), Some(too_old()))]
    );
    assert!(names_in(&root).is_empty(), "nothing is written");
}

#[gpui::test]
fn an_old_hosts_directory_error_reports_the_folder_as_too_old(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked_old(cx, &dir);

    click_link(&mut h, "see C:/wt/x/src", 6);
    let id = one_file_fetch(&mut h, "src");
    h.host(&saved_remote()).send(DaemonMessage::FileFetchError {
        id,
        error: "C:/wt/x/src is a directory".to_owned(),
    });

    let failures: Vec<(String, Option<String>)> = toasts(&mut h)
        .into_iter()
        .filter(|(title, _)| title.starts_with("Couldn't"))
        .collect();
    assert_eq!(
        failures,
        [("Couldn't fetch src".to_owned(), Some(too_old()))],
        "the warning replaces the daemon's error"
    );
}

#[gpui::test]
fn an_old_hosts_directory_error_on_an_early_reading_reports_too_old(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked_with(
        cx,
        &dir,
        session("s1").shell("C:/wt/x/sub").worktree("r1").build(),
        &[],
    );

    click_link(&mut h, "wrote docs/api", 9);
    let id = one_file_fetch(&mut h, "sub/docs/api");
    h.host(&saved_remote()).send(DaemonMessage::FileFetchError {
        id,
        error: "C:/wt/x/sub/docs/api is a directory".to_owned(),
    });

    assert!(
        fetch_messages(&mut h).is_empty(),
        "the link's other readings are not tried"
    );
    let failures: Vec<(String, Option<String>)> = toasts(&mut h)
        .into_iter()
        .filter(|(title, _)| title.starts_with("Couldn't"))
        .collect();
    assert_eq!(
        failures,
        [("Couldn't fetch api".to_owned(), Some(too_old()))],
        "the first reading's folder says the host is too old"
    );
}

#[gpui::test]
fn a_second_click_on_a_fetching_folder_says_already_fetching(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked(cx, &dir);

    fetch_folder(&mut h, "see C:/wt/x/src", 6, "src");
    click_again(&mut h, 6);
    let again = one_stat(&mut h, "src");
    stat(&mut h, &again, PathKind::Dir);

    assert!(
        fetch_messages(&mut h).is_empty(),
        "the folder is not asked for twice"
    );
    assert_eq!(titled(&mut h, "Already fetching src"), 1);
}

/// The id of the toast titled `title`, if one shows.
fn toast_titled(h: &mut Harness<'_>, title: &str) -> Option<u64> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .find(|toast| toast.title == title)
            .map(|toast| toast.id)
    })
}

/// The action ids of the toast titled `title`; none when it is not shown.
fn actions_of(h: &mut Harness<'_>, title: &str) -> Vec<&'static str> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .find(|toast| toast.title == title)
            .map(|toast| toast.actions.iter().map(|action| action.id).collect())
            .unwrap_or_default()
    })
}

/// The host sends `data` as the file the fetch `id` asked for.
fn stream_file(h: &mut Harness<'_>, id: &str, data: &[u8]) {
    let mut host = h.host(&saved_remote());
    host.send(DaemonMessage::FileFetchStarted {
        id: id.to_owned(),
        resolved_path: r"\\?\C:\wt\x\file".to_owned(),
        size: u64::try_from(data.len()).expect("a small file"),
    });
    host.send(DaemonMessage::FileChunk {
        id: id.to_owned(),
        seq: 0,
        data_b64: base64::engine::general_purpose::STANDARD.encode(data),
    });
    host.send(DaemonMessage::FileFetchDone { id: id.to_owned() });
}

/// The host's `FileFetchStarted` for the fetch `id` of a `size`-byte file.
fn file_started(id: &str, size: u64) -> DaemonMessage {
    DaemonMessage::FileFetchStarted {
        id: id.to_owned(),
        resolved_path: r"\\?\C:\wt\x\docs\notes.txt".to_owned(),
        size,
    }
}

/// The host's `FileChunk` `seq` of `data` for the fetch `id`.
fn file_chunk(id: &str, seq: u64, data: &[u8]) -> DaemonMessage {
    DaemonMessage::FileChunk {
        id: id.to_owned(),
        seq,
        data_b64: base64::engine::general_purpose::STANDARD.encode(data),
    }
}

/// Clicks the file link `text` at `col` on a host that sends folders,
/// answers its stat of `path` as a file and streams `notes` for it.
fn fetch_file(h: &mut Harness<'_>, text: &str, col: usize, path: &str) {
    click_link(h, text, col);
    let stat_id = one_stat(h, path);
    stat(h, &stat_id, PathKind::File);
    let id = one_file_fetch(h, path);
    stream_file(h, &id, b"notes");
    settle_until(h, "the fetched file to open", |h| !h.opened().is_empty());
}

#[gpui::test]
fn a_fetched_file_offers_the_whole_folder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = linked(cx, &dir);

    fetch_file(&mut h, "see C:/wt/x/docs/notes.txt", 6, "docs/notes.txt");

    assert!(
        toasts(&mut h).contains(&("Fetched notes.txt".to_owned(), Some(format!("from {HOST}")))),
        "the file's done toast shows: {:?}",
        toasts(&mut h)
    );
    assert_eq!(actions_of(&mut h, "Fetched notes.txt"), ["whole-folder"]);
    assert!(
        !toasts(&mut h)
            .iter()
            .any(|(title, _)| title.starts_with("Fetching")),
        "the done toast replaced the Fetching toast: {:?}",
        toasts(&mut h)
    );
    let saved = repo_folder(&root).join("docs").join("notes.txt");
    assert_eq!(
        h.opened(),
        [Opened::DefaultApp(saved)],
        "the file still opens"
    );
}

#[gpui::test]
fn pressing_whole_folder_fetches_the_files_parent(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked(cx, &dir);
    fetch_file(&mut h, "see C:/wt/x/docs/notes.txt", 6, "docs/notes.txt");
    let toast = toast_titled(&mut h, "Fetched notes.txt").expect("the done toast shows");

    h.click_on(&toast_action_selector(toast, "whole-folder"));
    h.cx.run_until_parked();

    let id = one_folder_fetch(&mut h, "docs");
    assert!(
        toast_titled(&mut h, "Fetched notes.txt").is_none(),
        "the file's toast goes: {:?}",
        toasts(&mut h)
    );
    assert!(
        toast_keyed(&mut h, &id).is_some(),
        "the folder's toast shows: {:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn pressing_whole_folder_on_a_root_file_fetches_the_root(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked(cx, &dir);
    fetch_file(&mut h, "see C:/wt/x/notes.txt", 6, "notes.txt");
    let toast = toast_titled(&mut h, "Fetched notes.txt").expect("the done toast shows");

    h.click_on(&toast_action_selector(toast, "whole-folder"));
    h.cx.run_until_parked();

    one_folder_fetch(&mut h, "");
    assert!(
        shows(&mut h, &format!("Fetching folder r1 from {HOST}")),
        "the repo root is fetched: {:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn whole_folder_on_a_disconnected_host_says_offline_not_too_old(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = linked(cx, &dir);
    fetch_file(&mut h, "see C:/wt/x/docs/notes.txt", 6, "docs/notes.txt");
    h.host(&saved_remote())
        .event(NetEvent::State(Connection::for_target(saved_remote())));
    let toast = toast_titled(&mut h, "Fetched notes.txt").expect("the done toast still shows");

    h.click_on(&toast_action_selector(toast, "whole-folder"));
    h.cx.run_until_parked();

    assert!(
        fetch_messages(&mut h).is_empty(),
        "nothing is asked of the host"
    );
    let failures: Vec<(String, Option<String>)> = toasts(&mut h)
        .into_iter()
        .filter(|(title, _)| title.starts_with("Couldn't"))
        .collect();
    assert_eq!(
        failures,
        [(
            "Couldn't fetch docs".to_owned(),
            Some(format!("{HOST} is offline."))
        )],
        "a host that is reconnecting is not too old"
    );
}

#[gpui::test]
fn a_host_without_folder_fetch_offers_no_whole_folder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = linked_old(cx, &dir);

    click_link(&mut h, "see C:/wt/x/docs/notes.txt", 6);
    let id = one_file_fetch(&mut h, "docs/notes.txt");
    stream_file(&mut h, &id, b"notes");
    settle_until(&mut h, "the fetched file to open", |h| {
        !h.opened().is_empty()
    });

    assert!(
        !toasts(&mut h)
            .iter()
            .any(|(title, _)| title.starts_with("Fetched")),
        "no done toast: {:?}",
        toasts(&mut h)
    );
    let saved = repo_folder(&root).join("docs").join("notes.txt");
    assert_eq!(fs::read(&saved).expect("the file is saved"), b"notes");
    assert_eq!(h.opened(), [Opened::DefaultApp(saved)], "the file opens");
}

#[gpui::test]
fn a_fetched_folder_reveals_in_explorer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = linked(cx, &dir);
    let id = fetch_folder(&mut h, "see C:/wt/x/src", 6, "src");
    stream_folder(&mut h, &id, &FILES);
    settle_until(&mut h, "the folder to be fetched", |h| {
        shows(h, "Fetched folder src")
    });
    assert_eq!(actions_of(&mut h, "Fetched folder src"), ["reveal"]);
    let toast = toast_keyed(&mut h, &id).expect("the folder's done toast shows");

    h.click_on(&toast_action_selector(toast, "reveal"));
    h.cx.run_until_parked();

    assert_eq!(
        h.opened(),
        [Opened::Reveal(repo_folder(&root).join("src"))],
        "the folder shows in Explorer"
    );
    assert!(
        shows(&mut h, "Fetched folder src"),
        "the toast stays until it times out"
    );
}
