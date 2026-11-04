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
use rustling_tulip_native::{Connection, NetEvent, toast_action_selector};
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
