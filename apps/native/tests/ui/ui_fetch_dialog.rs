//! Fetch file… specs: on a remote host with a pane focused, the sessions
//! toolbar's ⋯ menu offers a dialog that fetches one typed path from the
//! focused session's repo or worktree, scoped to the one holding its cwd. A
//! pasted absolute path under the root is made relative, one outside it or
//! with `..` is refused in the dialog; progress shows in the dialog, the
//! file opens when it is saved, Esc or Cancel stop the fetch and delete the
//! part, and errors show in the dialog, never as a toast.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use gpui::{Modifiers, TestAppContext, point, px};
use protocol::{ClientMessage, DaemonMessage, SessionSnapshot};
use rustling_tulip_native::RootView;
use rustling_tulip_native::file_fetch::FetchRequest;
use rustling_tulip_native::target::Target;
use support::{Fixture, Harness, Opened, TestDir, session};

const HOST: &str = "box.tail1234.ts.net";
const ROW: &str = "sidebar-more-fetch-file";

/// [`HOST`] as a target to open on.
fn remote_target() -> Target {
    Target::tailnet(HOST, None)
}

/// A harness over `fixture` on the remote host [`HOST`], its downloads
/// under `dir`; returns the download root.
fn remote_fixture<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    fixture: &Fixture,
) -> (Harness<'a>, PathBuf) {
    // Text files open externally, as these specs pin the external route;
    // `ui_viewer` covers the viewer's.
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "general": { "open_text_externally": true } }"#,
    )
    .expect("seed native-ui.json");
    let mut h = Harness::on_target(cx, dir, remote_target(), fixture);
    let root = dir.path().join("remote-files");
    let set = root.clone();
    h.root_update(move |view, _, _| view.set_remote_files_root(set));
    h.sent();
    (h, root)
}

/// [`remote_fixture`] with `s` alone in pane `p1`.
fn on_remote_with<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    s: SessionSnapshot,
) -> (Harness<'a>, PathBuf) {
    remote_fixture(cx, dir, &Fixture::single(s))
}

/// A session on the worktree `C:/wt/x` of the repo `r1`.
fn on_remote<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> (Harness<'a>, PathBuf) {
    on_remote_with(cx, dir, session("s1").worktree("r1").build())
}

/// Types `text` into the field that has the keyboard, one keystroke per
/// character.
fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
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
        resolved_path: r"C:\wt\x\src\a.rs".to_owned(),
        size,
    }
}

/// Whether the element tagged `selector` was ever painted in this spec's
/// window: one never drawn has no bounds.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

fn is_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.fetch_dialog_open())
}

fn error(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.fetch_dialog_error())
}

fn progress(h: &mut Harness<'_>) -> Option<(String, f32)> {
    h.root(|root, _| root.fetch_dialog_progress())
}

/// The `FetchFile` messages sent since the last look, as `(id, repo id,
/// worktree, path)`.
fn fetches_sent(h: &mut Harness<'_>) -> Vec<(String, String, Option<String>, String)> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::FetchFile {
                id,
                repo_id,
                worktree_path,
                path,
            } => Some((id, repo_id, worktree_path, path)),
            _ => None,
        })
        .collect()
}

/// Opens the dialog, types `text` and presses Enter; returns the id of the
/// one `FetchFile` that sent.
fn fetch(h: &mut Harness<'_>, text: &str) -> String {
    h.open_fetch_dialog_from_more();
    type_text(h, text);
    h.keys("enter");
    match fetches_sent(h).as_slice() {
        [(id, ..)] => id.clone(),
        other => panic!("one FetchFile is sent, got {other:?}"),
    }
}

fn no_failure_toast(h: &mut Harness<'_>) {
    let shown = toasts(h);
    assert!(
        !shown
            .iter()
            .any(|(title, _)| title.starts_with("Couldn't fetch")
                || title.starts_with("Fetching")
                || title.starts_with("Already fetching")),
        "the dialog speaks, not a toast: {shown:?}"
    );
}

#[gpui::test]
fn the_more_menu_offers_fetch_file_only_on_a_remote_host_with_a_focused_session(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let (mut h, _) = on_remote(cx, &dir);
    h.click_on("sidebar-more");
    assert!(
        drawn(&mut h, ROW),
        "a remote host with a focused pane offers it"
    );
    h.click_on(ROW);
    assert!(is_open(&mut h));
    assert_eq!(
        h.root(RootView::fetch_dialog_focus).as_deref(),
        Some("fetch-path")
    );
}

#[gpui::test]
fn the_more_menu_has_no_fetch_file_on_the_local_daemon(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.click_on("sidebar-more");
    assert!(
        !drawn(&mut h, ROW),
        "the local daemon's files are on this PC"
    );
}

#[gpui::test]
fn the_more_menu_has_no_fetch_file_without_a_focused_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let unfocused = Fixture {
        sessions: vec![session("s1").worktree("r1").build()],
        ..Fixture::default()
    };
    let (mut h, _) = remote_fixture(cx, &dir, &unfocused);
    h.click_on("sidebar-more");
    assert!(
        !drawn(&mut h, ROW),
        "no focused pane, no session to fetch from"
    );
}

#[gpui::test]
fn the_dialog_scopes_to_the_member_holding_the_cwd(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let s = session("s1")
        .members(&[("r1", "main", "C:/wt/a"), ("r2", "main", "C:/wt/b")])
        .cwd("C:/wt/b/src")
        .build();
    let (mut h, _) = on_remote_with(cx, &dir, s);
    h.open_fetch_dialog_from_more();
    assert_eq!(
        h.root(|root, _| root.fetch_dialog_scope()).as_deref(),
        Some("r2")
    );
    assert!(drawn(&mut h, "fetch-scope-0") && drawn(&mut h, "fetch-scope-1"));
    assert_eq!(h.root(|root, _| root.fetch_dialog_caption()), None);

    h.click_on("fetch-scope-0");
    assert_eq!(
        h.root(|root, _| root.fetch_dialog_scope()).as_deref(),
        Some("r1")
    );
    h.keys("right");
    assert_eq!(
        h.root(|root, _| root.fetch_dialog_scope()).as_deref(),
        Some("r2"),
        "Right steps through the scope row"
    );
    h.click_on("fetch-path");
    type_text(&mut h, "a.rs");
    h.keys("enter");
    assert_eq!(
        fetches_sent(&mut h)
            .into_iter()
            .map(|(_, repo, wt, path)| (repo, wt, path))
            .collect::<Vec<_>>(),
        [(
            "r2".to_owned(),
            Some("C:/wt/b".to_owned()),
            "a.rs".to_owned()
        )]
    );
}

#[gpui::test]
fn a_single_member_shows_a_caption_not_buttons(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = on_remote(cx, &dir);
    h.open_fetch_dialog_from_more();
    assert_eq!(
        h.root(|root, _| root.fetch_dialog_caption()).as_deref(),
        Some("r1")
    );
    assert!(drawn(&mut h, "fetch-caption"));
    assert!(!drawn(&mut h, "fetch-scope-0"));
}

#[gpui::test]
fn a_session_with_no_repo_folder_says_so_and_cannot_fetch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = on_remote_with(cx, &dir, session("s1").in_repo("r1").build());
    h.open_fetch_dialog_from_more();
    assert_eq!(
        h.root(|root, _| root.fetch_dialog_caption()).as_deref(),
        Some("No repo for this session")
    );
    type_text(&mut h, "a.rs");
    assert!(!h.root(RootView::fetch_dialog_can_submit));
    h.keys("enter");
    assert!(fetches_sent(&mut h).is_empty(), "nothing is fetched");
}

#[gpui::test]
fn enter_sends_fetch_file_for_the_scope_and_path_with_the_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);
    h.open_fetch_dialog_from_more();
    assert!(
        !h.root(RootView::fetch_dialog_can_submit),
        "nothing typed yet"
    );
    type_text(&mut h, "src/main.rs:42");
    assert!(h.root(RootView::fetch_dialog_can_submit));
    h.keys("enter");
    let sent = fetches_sent(&mut h);
    let [(id, repo_id, worktree_path, path)] = sent.as_slice() else {
        panic!("one FetchFile is sent, got {sent:?}");
    };
    assert_eq!(repo_id, "r1");
    assert_eq!(worktree_path.as_deref(), Some("C:/wt/x"));
    assert_eq!(path, "src/main.rs");

    h.send(started(id, 3));
    h.send(chunk(id, 0, b"fn\n"));
    h.send(DaemonMessage::FileFetchDone { id: id.clone() });
    let saved = root.join(HOST).join("r1").join("src").join("main.rs");
    assert_eq!(
        h.opened(),
        vec![Opened::VsCode {
            path: saved,
            line: 42,
            column: 1
        }]
    );
}

#[gpui::test]
fn a_pasted_absolute_path_under_the_root_is_made_relative(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = on_remote(cx, &dir);
    h.open_fetch_dialog_from_more();
    type_text(&mut h, "C:/WT/x/./src/a.rs");
    h.keys("enter");
    assert_eq!(
        fetches_sent(&mut h)
            .into_iter()
            .map(|(_, _, _, path)| path)
            .collect::<Vec<_>>(),
        ["src/a.rs"]
    );
}

#[gpui::test]
fn a_path_outside_the_root_is_an_inline_error_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = on_remote(cx, &dir);
    h.open_fetch_dialog_from_more();
    type_text(&mut h, "C:/other/a.rs:3");
    assert!(!h.root(RootView::fetch_dialog_can_submit));
    h.keys("enter");
    assert_eq!(
        error(&mut h).as_deref(),
        Some("C:/other/a.rs is outside C:/wt/x. Give a path under it.")
    );
    assert!(drawn(&mut h, "fetch-error"));
    assert!(fetches_sent(&mut h).is_empty(), "nothing is fetched");
    assert!(is_open(&mut h));
}

#[gpui::test]
fn a_parent_path_is_refused_inline(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = on_remote(cx, &dir);
    h.open_fetch_dialog_from_more();
    type_text(&mut h, "src/../a.rs");
    h.keys("enter");
    assert_eq!(
        error(&mut h).as_deref(),
        Some("A path can't contain \"..\".")
    );
    assert!(fetches_sent(&mut h).is_empty(), "nothing is fetched");
}

#[gpui::test]
fn progress_shows_waiting_then_bytes_of_size_as_chunks_arrive(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = on_remote(cx, &dir);
    let id = fetch(&mut h, "src/a.rs");
    assert_eq!(
        progress(&mut h).map(|(text, _)| text).as_deref(),
        Some("Waiting for box.tail1234.ts.net…")
    );

    h.send(started(&id, 10));
    h.send(chunk(&id, 0, b"abcd"));
    let (text, fraction) = progress(&mut h).expect("progress while fetching");
    assert_eq!(text, "4 B of 10 B");
    assert!((fraction - 0.4).abs() < 1e-6);
    let track = h.bounds("fetch-progress").size.width;
    let fill = h.bounds("fetch-progress-fill").size.width;
    assert!(
        (fill - track * 0.4).abs() < px(1.0),
        "the fill is 40% of the track: {fill:?} of {track:?}"
    );
}

#[gpui::test]
fn done_closes_the_dialog_and_opens_the_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);
    let id = fetch(&mut h, "src/a.rs");
    h.send(started(&id, 2));
    h.send(chunk(&id, 0, b"ok"));
    h.send(DaemonMessage::FileFetchDone { id: id.clone() });

    let saved = root.join(HOST).join("r1").join("src").join("a.rs");
    assert_eq!(h.opened(), vec![Opened::DefaultApp(saved.clone())]);
    assert_eq!(fs::read(&saved).expect("the file is saved"), b"ok");
    assert!(!is_open(&mut h), "the save closes the dialog");
    no_failure_toast(&mut h);
}

/// Starts a fetch of `src/a.rs` with half of it written; returns its id and
/// the folder its part file is in.
fn half_fetched(h: &mut Harness<'_>, root: &Path) -> (String, PathBuf) {
    let id = fetch(h, "src/a.rs");
    h.send(started(&id, 10));
    h.send(chunk(&id, 0, b"half"));
    let folder = root.join(HOST).join("r1").join("src");
    assert_eq!(names_in(&folder).len(), 1, "the part file is being written");
    (id, folder)
}

/// The fetch `id` was cancelled: `CancelFetch` went out, the part is gone,
/// nothing opened and no toast said anything.
fn assert_cancelled(h: &mut Harness<'_>, id: &str, folder: &Path) {
    assert!(!is_open(h));
    assert!(
        h.sent().iter().any(|msg| matches!(
            msg,
            ClientMessage::CancelFetch { id: cancelled } if cancelled == id
        )),
        "the daemon is told to stop"
    );
    h.send(chunk(id, 1, b"late"));
    assert!(
        names_in(folder).is_empty(),
        "the part file is deleted: {:?}",
        names_in(folder)
    );
    assert!(h.opened().is_empty(), "nothing opens");
    assert!(
        toasts(h).is_empty(),
        "a cancel says nothing: {:?}",
        toasts(h)
    );
}

#[gpui::test]
fn esc_while_fetching_cancels_closes_and_deletes_the_part(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);
    let (id, folder) = half_fetched(&mut h, &root);
    h.keys("escape");
    assert_cancelled(&mut h, &id, &folder);
}

#[gpui::test]
fn enter_again_after_fetch_keeps_the_fetch_running(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = on_remote(cx, &dir);
    h.open_fetch_dialog_from_more();
    type_text(&mut h, "src/a.rs");
    h.keys("tab tab");
    assert_eq!(
        h.root(RootView::fetch_dialog_focus).as_deref(),
        Some("fetch-submit")
    );
    h.keys("enter");
    assert_eq!(fetches_sent(&mut h).len(), 1, "Fetch on the button fetches");
    assert_eq!(
        h.root(RootView::fetch_dialog_focus).as_deref(),
        Some("fetch-path"),
        "the keyboard goes back to the path field"
    );

    h.keys("enter");
    h.keys("space");
    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::CancelFetch { .. })),
        "a second press does not cancel: {sent:?}"
    );
    assert!(is_open(&mut h));
    assert!(progress(&mut h).is_some(), "the fetch is still running");
}

#[gpui::test]
fn cancel_button_does_the_same(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);
    let (id, folder) = half_fetched(&mut h, &root);
    h.click_on("fetch-cancel");
    assert_cancelled(&mut h, &id, &folder);
}

#[gpui::test]
fn backdrop_click_does_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = on_remote(cx, &dir);
    h.open_fetch_dialog_from_more();
    let backdrop = h.bounds("fetch-dialog");
    h.click(
        point(backdrop.origin.x + px(4.0), backdrop.origin.y + px(4.0)),
        Modifiers::none(),
    );
    assert!(is_open(&mut h), "a backdrop click keeps the dialog");
}

#[gpui::test]
fn a_daemon_error_shows_inline_and_leaves_the_dialog_open_with_no_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);
    let (id, folder) = half_fetched(&mut h, &root);
    h.send(DaemonMessage::FileFetchError {
        id,
        error: "reading the file: access denied".to_owned(),
    });

    assert!(is_open(&mut h));
    let shown = error(&mut h).expect("an error under the field");
    assert!(shown.contains("access denied"), "{shown}");
    assert_eq!(progress(&mut h), None, "the fetch is over");
    assert!(names_in(&folder).is_empty(), "no part file is left");
    no_failure_toast(&mut h);

    type_text(&mut h, "x");
    h.keys("backspace");
    assert_eq!(error(&mut h), None, "an edit clears the error");
    h.keys("enter");
    assert_eq!(fetches_sent(&mut h).len(), 1, "Enter fetches again");
}

#[gpui::test]
fn a_missing_file_shows_the_daemons_text_inline(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = on_remote(cx, &dir);
    let id = fetch(&mut h, "nope.rs");
    let text = "path \"nope.rs\" does not exist under C:/wt/x";
    h.send(DaemonMessage::FileFetchError {
        id,
        error: text.to_owned(),
    });
    assert_eq!(error(&mut h).as_deref(), Some(text));
    assert!(is_open(&mut h));
    no_failure_toast(&mut h);
}

#[gpui::test]
fn already_fetching_shows_inline(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = on_remote(cx, &dir);
    let request = FetchRequest {
        repo_id: "r1".to_owned(),
        repo_name: "r1".to_owned(),
        worktree_path: Some("C:/wt/x".to_owned()),
        rel_path: "src/a.rs".to_owned(),
        line: None,
        column: None,
        open_when_done: true,
    };
    h.root_update(move |view, _, cx| view.fetch_remote_file(request, cx))
        .expect("the link fetch starts");
    h.sent();

    h.open_fetch_dialog_from_more();
    type_text(&mut h, "src/a.rs");
    h.keys("enter");
    assert_eq!(error(&mut h).as_deref(), Some("Already fetching a.rs"));
    assert!(fetches_sent(&mut h).is_empty(), "nothing is fetched");
    assert!(
        !toasts(&mut h)
            .iter()
            .any(|(title, _)| title.starts_with("Already fetching")),
        "no toast for the dialog's refusal"
    );
}

#[gpui::test]
fn a_lost_connection_closes_the_dialog_and_deletes_the_part(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);
    let (_, folder) = half_fetched(&mut h, &root);
    h.lose_connection();

    assert!(!is_open(&mut h), "the overlay closes the dialog");
    assert!(
        names_in(&folder).is_empty(),
        "the part file is deleted: {:?}",
        names_in(&folder)
    );
    no_failure_toast(&mut h);
    assert!(h.opened().is_empty(), "nothing opens");
}

#[gpui::test]
fn the_dialog_blocks_a_run_confirm_and_other_dialogs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = on_remote(cx, &dir);
    h.open_fetch_dialog_from_more();
    h.click_on("sidebar-more");
    assert!(
        !h.root(|root, _| root.shell_dialog_open()),
        "the dialog's backdrop takes the click"
    );
    h.keys("escape");
    assert!(!is_open(&mut h));

    h.open_shell_dialog_from_more();
    h.click_on("sidebar-more");
    assert!(!is_open(&mut h), "nor does Fetch file… open over Shell…");
}
