//! Remote file fetch specs: on a remote host the client asks the daemon for
//! a file, saves its chunks into the host's download folder on this PC,
//! opens the saved copy by the `:line` rule, and leaves no part file behind
//! when the fetch fails or the connection goes. The local daemon fetches
//! nothing.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use gpui::{Modifiers, TestAppContext};
use protocol::{ClientMessage, DaemonMessage, SessionSnapshot};
use rustling_tulip_native::file_fetch::FetchRequest;
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{Connection, NetEvent};
use support::{Fixture, Harness, Opened, TestDir, session};

const HOST: &str = "box.tail1234.ts.net";

/// A connection to `target` open on `port`, welcomed with the fake daemon's
/// features.
fn open_on(target: Target, port: u16) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    conn.on_socket_open(port);
    conn.on_welcome(support::PROTOCOL, support::daemon_features());
    conn
}

/// [`HOST`] as a target to open on.
fn remote_target() -> Target {
    Target::tailnet(HOST, None)
}

/// A request for `rel` in the repo `r1`'s worktree.
fn request(rel: &str, line: Option<u32>, column: Option<u32>) -> FetchRequest {
    FetchRequest {
        repo_id: "r1".to_owned(),
        repo_name: "repo".to_owned(),
        worktree_path: Some("C:/wt/r1".to_owned()),
        rel_path: rel.to_owned(),
        line,
        column,
        open_when_done: true,
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

/// A harness on the remote host [`HOST`], its downloads under `dir`; returns
/// the download root.
fn on_remote<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> (Harness<'a>, PathBuf) {
    on_remote_with(cx, dir, session("s1").worktree("r1").build())
}

/// [`on_remote`] with `s` alone in pane `p1`.
fn on_remote_with<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    s: SessionSnapshot,
) -> (Harness<'a>, PathBuf) {
    let mut h = Harness::on_target(cx, dir, remote_target(), &Fixture::single(s));
    let root = dir.path().join("remote-files");
    let set = root.clone();
    h.root_update(move |view, _, _| view.set_remote_files_root(set));
    h.sent();
    (h, root)
}

/// Starts fetching `request`; returns the id the `FetchFile` it sent carries.
fn start(h: &mut Harness<'_>, request: FetchRequest) -> String {
    let rel = request.rel_path.clone();
    let returned = h
        .root_update(move |view, _, cx| view.fetch_remote_file(request, cx))
        .expect("a fetch on a remote host starts");
    let sent = h.sent();
    let [
        ClientMessage::FetchFile {
            id,
            repo_id,
            worktree_path,
            path,
        },
    ] = sent.as_slice()
    else {
        panic!("one FetchFile is sent, got {sent:?}");
    };
    assert_eq!(id, &returned);
    assert_eq!(repo_id, "r1");
    assert_eq!(worktree_path.as_deref(), Some("C:/wt/r1"));
    assert_eq!(path, &rel);
    id.clone()
}

fn chunk(id: &str, seq: u64, data: &[u8]) -> DaemonMessage {
    DaemonMessage::FileChunk {
        id: id.to_owned(),
        seq,
        data_b64: base64::engine::general_purpose::STANDARD.encode(data),
    }
}

fn started(id: &str, size: u64) -> DaemonMessage {
    DaemonMessage::FileFetchStarted {
        id: id.to_owned(),
        resolved_path: r"\\?\C:\repos\repo\src\file".to_owned(),
        size,
    }
}

/// Streams `first` then `second` as the daemon would, ending with
/// `FileFetchDone`.
fn stream(h: &mut Harness<'_>, id: &str, first: &[u8], second: &[u8]) {
    let size = u64::try_from(first.len() + second.len()).expect("a small file");
    h.send(started(id, size));
    h.send(chunk(id, 0, first));
    h.send(chunk(id, 1, second));
    h.send(DaemonMessage::FileFetchDone { id: id.to_owned() });
}

#[gpui::test]
fn a_fetched_file_with_a_line_opens_in_vscode(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);

    let id = start(&mut h, request("src/main.rs", Some(12), Some(3)));
    assert!(
        toasts(&mut h).contains(&(format!("Fetching main.rs from {HOST}"), None)),
        "the fetch says it started: {:?}",
        toasts(&mut h)
    );
    stream(&mut h, &id, b"fn main() {\n", b"}\n");

    let saved = root.join(HOST).join("repo").join("src").join("main.rs");
    assert_eq!(
        fs::read(&saved).expect("the file is saved"),
        b"fn main() {\n}\n"
    );
    assert_eq!(names_in(saved.parent().expect("a parent")), ["main.rs"]);
    assert_eq!(
        h.opened(),
        vec![Opened::VsCode {
            path: saved,
            line: 12,
            column: 3
        }]
    );
}

#[gpui::test]
fn a_fetched_file_without_a_line_opens_in_its_default_app(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);

    let id = start(&mut h, request("docs/notes.txt", None, None));
    stream(&mut h, &id, b"first half, ", b"second half");

    let saved = root.join(HOST).join("repo").join("docs").join("notes.txt");
    assert_eq!(
        fs::read(&saved).expect("the file is saved"),
        b"first half, second half"
    );
    assert_eq!(h.opened(), vec![Opened::DefaultApp(saved)]);
}

#[gpui::test]
fn a_daemon_error_leaves_no_part_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);

    let id = start(&mut h, request("src/a.txt", None, None));
    h.send(started(&id, 10));
    h.send(chunk(&id, 0, b"half"));
    h.send(DaemonMessage::FileFetchError {
        id: id.clone(),
        error: "reading the file: access denied".to_owned(),
    });

    let folder = root.join(HOST).join("repo").join("src");
    assert!(
        names_in(&folder).is_empty(),
        "no part file is left: {:?}",
        names_in(&folder)
    );
    let shown = toasts(&mut h);
    assert!(
        shown
            .iter()
            .any(|(title, detail)| title == "Couldn't fetch a.txt"
                && detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains("access denied"))),
        "the failure shows the daemon's error: {shown:?}"
    );
    assert!(h.opened().is_empty(), "nothing opens");
}

#[gpui::test]
fn a_lost_connection_deletes_the_part_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);

    let id = start(&mut h, request("src/a.txt", None, None));
    h.send(started(&id, 10));
    h.send(chunk(&id, 0, b"half"));
    let folder = root.join(HOST).join("repo").join("src");
    assert_eq!(
        names_in(&folder).len(),
        1,
        "the part file is written while the fetch runs"
    );

    h.lose_connection();

    assert!(
        names_in(&folder).is_empty(),
        "the part file is deleted: {:?}",
        names_in(&folder)
    );
    assert!(
        toasts(&mut h)
            .iter()
            .any(|(title, _)| title == "Couldn't fetch a.txt"),
        "the fetch ends with a failure toast: {:?}",
        toasts(&mut h)
    );
    assert!(h.opened().is_empty(), "nothing opens");
}

#[gpui::test]
fn a_file_already_fetching_is_not_fetched_twice(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);

    let id = start(&mut h, request("src/a.txt", None, None));
    let again =
        h.root_update(|view, _, cx| view.fetch_remote_file(request("src/a.txt", None, None), cx));

    assert_eq!(again, None, "the second fetch does not start");
    assert!(h.sent().is_empty(), "nothing more is sent");
    assert!(
        toasts(&mut h).contains(&("Already fetching a.txt".to_owned(), None)),
        "a toast says so: {:?}",
        toasts(&mut h)
    );

    stream(&mut h, &id, b"ab", b"cd");
    let saved = root.join(HOST).join("repo").join("src").join("a.txt");
    assert_eq!(fs::read(&saved).expect("the file is saved"), b"abcd");
    start(&mut h, request("src/a.txt", None, None));
}

/// Clicks the run confirm's button `selector`.
fn press(h: &mut Harness<'_>, selector: &str) {
    let at = h.bounds(selector).center();
    h.click(at, Modifiers::none());
}

#[gpui::test]
fn a_fetched_script_asks_first_then_runs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);

    let id = start(&mut h, request("tools/run.bat", None, None));
    stream(&mut h, &id, b"@echo ", b"off\r\n");

    let saved = root.join(HOST).join("repo").join("tools").join("run.bat");
    assert_eq!(h.opened(), [], "nothing runs before the answer");
    assert!(h.run_confirm().is_some(), "the run confirm asks first");
    press(&mut h, "run-confirm-run");
    assert_eq!(h.opened(), [Opened::DefaultApp(saved)]);
}

#[gpui::test]
fn a_fetched_file_falls_back_when_vscode_is_missing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);
    h.fail_vscode_opens();

    let id = start(&mut h, request("src/main.rs", Some(4), None));
    stream(&mut h, &id, b"fn ", b"main() {}\n");

    let saved = root.join(HOST).join("repo").join("src").join("main.rs");
    assert_eq!(h.opened(), [Opened::DefaultApp(saved)]);
}

#[gpui::test]
fn a_link_open_under_the_download_root_is_still_refused(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);
    let file = root.join(HOST).join("repo").join("run.bat");
    fs::create_dir_all(file.parent().expect("a parent")).expect("make the folder");
    fs::write(&file, "@echo off\r\n").expect("write the file");

    let asked = file.clone();
    h.root_update(move |view, window, cx| view.ask_to_run(asked, window, cx));
    press(&mut h, "run-confirm-run");

    assert_eq!(
        h.opened(),
        [],
        "a path's place does not make it a fetched file"
    );
    assert!(
        toasts(&mut h)
            .iter()
            .any(|(title, _)| title == "Couldn't open"),
        "the remote host's refusal shows: {:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn a_disconnect_after_done_lets_the_file_save_and_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);

    let id = start(&mut h, request("docs/notes.txt", None, None));
    h.send(started(&id, 4));
    h.send(chunk(&id, 0, b"done"));
    h.events_together(vec![
        NetEvent::Message(Box::new(DaemonMessage::FileFetchDone { id: id.clone() })),
        NetEvent::State(Connection::new()),
    ]);

    let saved = root.join(HOST).join("repo").join("docs").join("notes.txt");
    assert_eq!(fs::read(&saved).expect("the file is saved"), b"done");
    assert_eq!(h.opened(), [Opened::DefaultApp(saved)]);
    assert!(
        !toasts(&mut h)
            .iter()
            .any(|(title, _)| title.starts_with("Couldn't fetch")),
        "no failure is reported: {:?}",
        toasts(&mut h)
    );
}

/// The member worktree of `session("s1").worktree("r1")`.
const WORKTREE: &str = "C:/wt/x";

/// Writes `text` to `s1`'s pane and Ctrl+clicks column `col` of its first
/// row.
fn click_link(h: &mut Harness<'_>, text: &str, col: usize) {
    h.pty("s1", text.as_bytes());
    let on = h.cell_center("p1", col, 0);
    h.click(on, Modifiers::secondary_key());
}

/// Ctrl+clicks column `col` of the first row again.
fn click_again(h: &mut Harness<'_>, col: usize) {
    let on = h.cell_center("p1", col, 0);
    h.click(on, Modifiers::secondary_key());
}

/// The `FetchFile`s sent since the last look, as `(id, worktree, path)`;
/// every one names the repo `r1`.
fn fetches_sent(h: &mut Harness<'_>) -> Vec<(String, Option<String>, String)> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::FetchFile {
                id,
                repo_id,
                worktree_path,
                path,
            } => {
                assert_eq!(repo_id, "r1");
                Some((id, worktree_path, path))
            }
            _ => None,
        })
        .collect()
}

/// The one `FetchFile` sent since the last look, checked to read `path` in
/// [`WORKTREE`]; returns its id.
fn one_fetch(h: &mut Harness<'_>, path: &str) -> String {
    let sent = fetches_sent(h);
    let [(id, worktree, sent_path)] = sent.as_slice() else {
        panic!("one FetchFile is sent, got {sent:?}");
    };
    assert_eq!(worktree.as_deref(), Some(WORKTREE));
    assert_eq!(sent_path, path);
    id.clone()
}

fn missing(id: &str, rel: &str) -> DaemonMessage {
    DaemonMessage::FileFetchError {
        id: id.to_owned(),
        error: format!("path \"{rel}\" does not exist under {WORKTREE}"),
    }
}

/// How many toasts are titled `title`.
fn titled(h: &mut Harness<'_>, title: &str) -> usize {
    toasts(h).iter().filter(|(t, _)| t == title).count()
}

/// A shell in `C:/wt/x/sub` on the member worktree [`WORKTREE`], so a
/// relative link reads under `sub` first, then under the worktree.
fn shell_in_worktree() -> SessionSnapshot {
    session("s1").shell("C:/wt/x/sub").worktree("r1").build()
}

#[gpui::test]
fn a_link_fetch_tries_the_next_reading_after_a_missing_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote_with(cx, &dir, shell_in_worktree());

    click_link(&mut h, "wrote docs/notes.md", 9);
    let first = one_fetch(&mut h, "sub/docs/notes.md");
    h.send(missing(&first, "sub/docs/notes.md"));
    let second = one_fetch(&mut h, "docs/notes.md");
    assert!(
        !toasts(&mut h)
            .iter()
            .any(|(title, _)| title.starts_with("Couldn't")),
        "a missed reading is not a failure: {:?}",
        toasts(&mut h)
    );
    stream(&mut h, &second, b"# Notes", b"\n");

    let saved = root.join(HOST).join("r1").join("docs").join("notes.md");
    assert_eq!(fs::read(&saved).expect("the file is saved"), b"# Notes\n");
    assert_eq!(h.opened(), [Opened::DefaultApp(saved)]);
    assert_eq!(
        titled(&mut h, &format!("Fetching notes.md from {HOST}")),
        1,
        "one toast says the link is fetching: {:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn a_link_fetch_with_a_line_opens_in_vscode_at_the_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);

    click_link(&mut h, r"at C:\wt\x\src\main.rs:12:3 here", 6);
    let id = one_fetch(&mut h, "src/main.rs");
    stream(&mut h, &id, b"fn main() {\n", b"}\n");

    let saved = root.join(HOST).join("r1").join("src").join("main.rs");
    assert_eq!(
        h.opened(),
        [Opened::VsCode {
            path: saved,
            line: 12,
            column: 3
        }]
    );
}

#[gpui::test]
fn a_link_without_a_line_opens_in_the_default_app(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);

    click_link(&mut h, "see C:/wt/x/docs/notes.txt", 6);
    let id = one_fetch(&mut h, "docs/notes.txt");
    stream(&mut h, &id, b"first, ", b"second");

    let saved = root.join(HOST).join("r1").join("docs").join("notes.txt");
    assert_eq!(h.opened(), [Opened::DefaultApp(saved)]);
}

#[gpui::test]
fn every_reading_missing_shows_one_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote_with(cx, &dir, shell_in_worktree());

    click_link(&mut h, "wrote docs/notes.md", 9);
    let first = one_fetch(&mut h, "sub/docs/notes.md");
    h.send(missing(&first, "sub/docs/notes.md"));
    let second = one_fetch(&mut h, "docs/notes.md");
    h.send(missing(&second, "docs/notes.md"));

    assert!(fetches_sent(&mut h).is_empty(), "no reading is left to try");
    let failures: Vec<(String, Option<String>)> = toasts(&mut h)
        .into_iter()
        .filter(|(title, _)| title.starts_with("Couldn't"))
        .collect();
    assert_eq!(
        failures,
        [(
            "Couldn't fetch notes.md".to_owned(),
            Some(format!(
                "path \"docs/notes.md\" does not exist under {WORKTREE}"
            ))
        )],
        "one toast names the last reading's error"
    );
    assert_eq!(titled(&mut h, &format!("Fetching notes.md from {HOST}")), 1);
    assert!(h.opened().is_empty(), "nothing opens");
    assert!(names_in(&root).is_empty(), "nothing is written");
}

#[gpui::test]
fn a_path_outside_every_root_toasts_without_sending(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = on_remote(cx, &dir);

    click_link(&mut h, r"see D:\elsewhere\a.rs", 6);

    assert!(
        fetches_sent(&mut h).is_empty(),
        "nothing is asked of the host"
    );
    assert_eq!(
        toasts(&mut h),
        [(
            "Couldn't fetch a.rs".to_owned(),
            Some("Not inside a repo or worktree of this session.".to_owned())
        )]
    );
    assert!(h.opened().is_empty(), "nothing opens");
}

#[gpui::test]
fn a_second_click_on_a_fetching_file_says_already_fetching(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = on_remote(cx, &dir);

    click_link(&mut h, "see C:/wt/x/docs/notes.txt", 6);
    one_fetch(&mut h, "docs/notes.txt");
    click_again(&mut h, 6);

    assert!(
        fetches_sent(&mut h).is_empty(),
        "the file is not asked for twice"
    );
    assert_eq!(titled(&mut h, "Already fetching notes.txt"), 1);
}

#[gpui::test]
fn the_local_daemon_fetches_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("s1").worktree("r1").build()),
    );
    h.set_connection(open_on(Target::Local, 4242));
    let root = dir.path().join("remote-files");
    let set = root.clone();
    h.root_update(move |view, _, _| view.set_remote_files_root(set));
    h.sent();

    let id = h.root_update(|view, _, cx| view.fetch_remote_file(request("a.txt", None, None), cx));

    assert_eq!(id, None, "no fetch starts on the local daemon");
    assert!(h.sent().is_empty(), "nothing is sent");
    assert!(
        toasts(&mut h)
            .iter()
            .any(|(title, _)| title == "Couldn't fetch a.txt"),
        "a toast says why: {:?}",
        toasts(&mut h)
    );
    assert!(!root.exists(), "no download folder is made");
}
