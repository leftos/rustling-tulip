//! Fetch file… specs: with a remote host's pane focused, the sessions
//! toolbar's ⋯ menu offers a dialog that fetches one typed path from the
//! focused session's repo or worktree, scoped to the one holding its cwd. A
//! pasted absolute path under the root is made relative, one outside it or
//! with `..` is refused in the dialog; progress shows in the dialog, the
//! file opens when it is saved, Esc or Cancel stop the fetch and delete the
//! part, and errors show in the dialog, never as a toast. A pane of This PC,
//! whose files are already here, offers no Fetch file… row.

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
use rustling_tulip_native::file_fetch::FetchRequest;
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, NetEvent, RootView};
use support::{
    Fixture, Harness, HostHandle, TestDir, remote_in_pane, saved_remote, saved_remote_over, session,
};

const HOST: &str = "box.tail1234.ts.net";
const ROW: &str = "sidebar-more-fetch-file";

/// The saved host [`HOST`], as a spec drives its fake daemon.
fn remote<'h, 'a>(h: &'h mut Harness<'a>) -> HostHandle<'h, 'a> {
    h.host(&saved_remote())
}

/// `s` in pane `p2` beside This PC's `a1`, on the saved host [`HOST`]
/// listing the repos `r1` and `r2`, `p2` focused, its downloads under
/// `dir`; returns the download root. Text files open externally, as these
/// specs pin the external route; `ui_viewer` covers the viewer's.
fn on_remote_with<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    s: SessionSnapshot,
) -> (Harness<'a>, PathBuf) {
    on_remote_saving(cx, dir, &[], s)
}

/// [`on_remote_with`], with the tailnet hosts `also` saved after [`HOST`]
/// on port 8787, so the window dials them too.
fn on_remote_saving<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    also: &[&str],
    s: SessionSnapshot,
) -> (Harness<'a>, PathBuf) {
    let saved: Vec<serde_json::Value> = also
        .iter()
        .map(|name| serde_json::json!({ "name": name, "port": 8787 }))
        .collect();
    let ui = serde_json::json!({ "saved_hosts": saved });
    let mut h = remote_in_pane(cx, dir, &ui, &["r1", "r2"], s);
    let root = dir.path().join("remote-files");
    let set = root.clone();
    h.root_update(move |view, _, _| view.set_remote_files_root(set));
    h.sent();
    (h, root)
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

/// The `FetchFile` messages sent to [`HOST`] since the last look, as `(id,
/// repo id, worktree, path)`. None may go to This PC.
fn fetches_sent(h: &mut Harness<'_>) -> Vec<(String, String, Option<String>, String)> {
    let local = h.sent();
    assert!(
        !local
            .iter()
            .any(|msg| matches!(msg, ClientMessage::FetchFile { .. })),
        "nothing is fetched from This PC: {local:?}"
    );
    remote(h)
        .sent()
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
fn the_more_menu_offers_fetch_file_only_with_a_focused_remote_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = on_remote(cx, &dir);
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    assert_eq!(h.root(|root, _| root.focused_pane()).as_deref(), Some("p1"));
    h.click_on("sidebar-more");
    assert!(
        !drawn(&mut h, ROW),
        "a focused pane of This PC offers no Fetch file…"
    );
    h.keys("escape");
    assert!(!h.root(|root, _| root.sidebar_more_open()));

    let at = h.cell_center("p2", 0, 0);
    h.click(at, Modifiers::none());
    assert_eq!(h.root(|root, _| root.focused_pane()).as_deref(), Some("p2"));
    h.click_on("sidebar-more");
    assert!(drawn(&mut h, ROW), "a focused remote pane offers it");
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
    let mut h = saved_remote_over(cx, &dir, &Fixture::default(), &[]);
    remote(&mut h).send(DaemonMessage::Sessions {
        sessions: vec![session("s1").worktree("r1").build()],
    });
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

    remote(&mut h).send(started(id, 3));
    remote(&mut h).send(chunk(id, 0, b"fn\n"));
    remote(&mut h).send(DaemonMessage::FileFetchDone { id: id.clone() });
    let saved = root.join(HOST).join("r1").join("src").join("main.rs");
    assert_eq!(h.opened(), [], "the dialog's text file goes to the viewer");
    assert_eq!(viewer_opens(&mut h), [saved]);
}

/// The paths of the `OpenViewerTab`s sent on This PC since the last look.
fn viewer_opens(h: &mut Harness<'_>) -> Vec<PathBuf> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::OpenViewerTab { path, .. } => Some(PathBuf::from(path)),
            _ => None,
        })
        .collect()
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

    remote(&mut h).send(started(&id, 10));
    remote(&mut h).send(chunk(&id, 0, b"abcd"));
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
    remote(&mut h).send(started(&id, 2));
    remote(&mut h).send(chunk(&id, 0, b"ok"));
    remote(&mut h).send(DaemonMessage::FileFetchDone { id: id.clone() });

    let saved = root.join(HOST).join("r1").join("src").join("a.rs");
    assert_eq!(h.opened(), [], "a text file goes to the viewer");
    assert_eq!(viewer_opens(&mut h), std::slice::from_ref(&saved));
    assert_eq!(fs::read(&saved).expect("the file is saved"), b"ok");
    assert!(!is_open(&mut h), "the save closes the dialog");
    no_failure_toast(&mut h);
}

/// Starts a fetch of `src/a.rs` with half of it written; returns its id and
/// the folder its part file is in.
fn half_fetched(h: &mut Harness<'_>, root: &Path) -> (String, PathBuf) {
    let id = fetch(h, "src/a.rs");
    remote(h).send(started(&id, 10));
    remote(h).send(chunk(&id, 0, b"half"));
    let folder = root.join(HOST).join("r1").join("src");
    assert_eq!(names_in(&folder).len(), 1, "the part file is being written");
    (id, folder)
}

/// The fetch `id` was cancelled: `CancelFetch` went out to [`HOST`], the
/// part is gone, nothing opened and no toast said anything.
fn assert_cancelled(h: &mut Harness<'_>, id: &str, folder: &Path) {
    assert!(!is_open(h));
    assert!(
        remote(h).sent().iter().any(|msg| matches!(
            msg,
            ClientMessage::CancelFetch { id: cancelled } if cancelled == id
        )),
        "the daemon is told to stop"
    );
    remote(h).send(chunk(id, 1, b"late"));
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
    let sent = remote(&mut h).sent();
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
    remote(&mut h).send(DaemonMessage::FileFetchError {
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
    remote(&mut h).send(DaemonMessage::FileFetchError {
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
        host: HostKey::of(&Target::tailnet(HOST, None)),
        repo_id: "r1".to_owned(),
        repo_name: "r1".to_owned(),
        worktree_path: Some("C:/wt/x".to_owned()),
        rel_path: "src/a.rs".to_owned(),
        line: None,
        column: None,
        open_when_done: true,
        prefer_viewer: true,
    };
    h.root_update(move |view, _, cx| view.fetch_remote_file(request, cx))
        .expect("the link fetch starts");
    remote(&mut h).sent();

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
    remote(&mut h).event(NetEvent::State(Connection::for_target(saved_remote())));

    assert!(!is_open(&mut h), "the host's loss closes the dialog");
    assert!(
        names_in(&folder).is_empty(),
        "the part file is deleted: {:?}",
        names_in(&folder)
    );
    no_failure_toast(&mut h);
    assert!(h.opened().is_empty(), "nothing opens");
}

#[gpui::test]
fn another_remote_hosts_lost_connection_leaves_the_dialog_open(cx: &mut TestAppContext) {
    const DEN: &str = "den.tail1234.ts.net";
    let den = Target::tailnet(DEN, Some(8787));
    let dir = TestDir::new();
    let (mut h, root) = on_remote_saving(cx, &dir, &[DEN], session("s1").worktree("r1").build());
    let mut other = h.host(&den);
    other.connect();
    other.send(DaemonMessage::Sessions {
        sessions: Vec::new(),
    });
    let (_, folder) = half_fetched(&mut h, &root);
    h.host(&den)
        .event(NetEvent::State(Connection::for_target(den.clone())));

    assert!(
        is_open(&mut h),
        "another remote host's loss leaves the dialog open"
    );
    assert_eq!(
        names_in(&folder).len(),
        1,
        "the part file is kept: {:?}",
        names_in(&folder)
    );
    assert!(progress(&mut h).is_some(), "the fetch still runs");
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
