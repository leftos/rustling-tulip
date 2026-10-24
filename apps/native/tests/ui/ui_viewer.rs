//! Text viewer specs: a path link to a text file opens a viewer tab instead
//! of the default app, scrolled to its `:line`; binary and large files, and
//! every file while the setting is on, still go out; the header's buttons
//! take the route the tab's file came by; a file fetched from a remote host
//! opens in the viewer too.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use gpui::{Modifiers, ScrollStrategy, TestAppContext, px};
use protocol::{ClientMessage, DaemonMessage, SessionSnapshot, TabEntry};
use rustling_tulip_native::file_fetch::FetchRequest;
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{MarkdownMode, ViewerLoad, ViewerOrigin, ViewerTab};
use serde_json::json;
use support::{Fixture, Harness, Opened, TestDir, session};

const HOST: &str = "box.tail1234.ts.net";

/// `s` alone in pane `p1`, its scrollback answered, the pane focused and
/// everything sent so far drained.
fn attached<'a>(cx: &'a mut TestAppContext, dir: &TestDir, s: SessionSnapshot) -> Harness<'a> {
    let id = s.id.clone();
    let mut h = Harness::with(cx, dir, &Fixture::single(s));
    h.answer_scrollback(&id, b"");
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h
}

/// A shell session whose folder is `cwd`.
fn shell_in(cwd: &Path) -> SessionSnapshot {
    session("s1").shell(&cwd.to_string_lossy()).build()
}

/// Writes `bytes` to `rel` under `root`; returns the path a link to it
/// resolves to.
fn write_file(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(rel);
    let parent = path.parent().expect("a file has a parent folder");
    fs::create_dir_all(parent).expect("create the file's folder");
    fs::write(&path, bytes).expect("write the file");
    let resolved = fs::canonicalize(&path).expect("canonicalize the file");
    let text = resolved.to_string_lossy();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
}

/// `count` numbered lines of Rust.
fn numbered_lines(count: usize) -> String {
    (1..=count)
        .map(|n| format!("let line_{n} = {n};\n"))
        .collect::<Vec<_>>()
        .concat()
}

/// Seeds `native-ui.json` with the setting that sends text files out.
fn seed_open_externally(dir: &TestDir) {
    fs::write(
        dir.path().join("native-ui.json"),
        json!({ "general": { "open_text_externally": true } }).to_string(),
    )
    .expect("seed native-ui.json");
}

fn ctrl_click_cell(h: &mut Harness<'_>, col: usize, row: usize) {
    let on = h.cell_center("p1", col, row);
    h.click(on, Modifiers::secondary_key());
}

/// The `OpenViewerTab`s sent since the last look, as `(id, path)`.
fn opens_sent(h: &mut Harness<'_>) -> Vec<(String, String)> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::OpenViewerTab { id, path } => Some((id, path)),
            _ => None,
        })
        .collect()
}

/// The one `OpenViewerTab` sent since the last look, checked to name
/// `path`; returns its id.
fn one_open(h: &mut Harness<'_>, path: &Path) -> String {
    let opens = opens_sent(h);
    let [(id, sent_path)] = opens.as_slice() else {
        panic!("one OpenViewerTab is sent, got {opens:?}");
    };
    assert_eq!(Path::new(sent_path), path);
    id.clone()
}

/// A viewer tab `id` of `path`.
fn viewer_tab(id: &str, path: &Path) -> TabEntry {
    serde_json::from_value(json!({
        "id": id,
        "name": path.file_name().map(|name| name.to_string_lossy().into_owned()),
        "content": { "kind": "viewer", "path": path.to_string_lossy() },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("viewer tab fixture")
}

/// The daemon answers the open `id` with the new tab `v1` of `path`.
fn answer_open(h: &mut Harness<'_>, id: &str, path: &Path) {
    h.send(DaemonMessage::TabUpdated {
        tab: viewer_tab("v1", path),
    });
    h.send(DaemonMessage::ViewerTabOpened {
        id: id.to_owned(),
        tab_id: "v1".to_owned(),
    });
}

/// Whether an element tagged `selector` was painted.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.cx.run_until_parked();
    let selector: &'static str = Box::leak(selector.to_owned().into_boxed_str());
    h.cx.debug_bounds(selector).is_some()
}

/// Viewer tab `v1`'s origin, load state and line target.
fn shown(h: &mut Harness<'_>) -> Option<(ViewerOrigin, ViewerLoad, Option<u32>)> {
    h.root(|root, _| {
        root.viewer_tab("v1")
            .map(|tab| (tab.origin(), tab.load(), tab.line_target()))
    })
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

/// A local harness whose tab `v1` views `path`, shown.
fn showing<'a>(cx: &'a mut TestAppContext, dir: &TestDir, path: &Path) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s1").build());
    fixture.tabs.push(viewer_tab("v1", path));
    let mut h = Harness::with(cx, dir, &fixture);
    h.click_on("tab-v1");
    h.sent();
    h
}

#[gpui::test]
fn a_plain_path_link_opens_a_viewer_tab_not_the_default_app(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.md", b"# Notes\n");
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", b"wrote docs/notes.md");
    ctrl_click_cell(&mut h, 9, 0);

    assert_eq!(h.opened(), [], "no default app");
    let id = one_open(&mut h, &file);
    answer_open(&mut h, &id, &file);
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("v1")
    );
    assert_eq!(
        shown(&mut h),
        Some((ViewerOrigin::Link, ViewerLoad::Loaded, None))
    );
    assert!(drawn(&mut h, "viewer-v1"));
    assert!(
        drawn(&mut h, "viewer-md-block-v1-0"),
        "the file shows, rendered"
    );
    assert!(!drawn(&mut h, "viewer-banner-v1"));
}

#[gpui::test]
fn a_line_ref_opens_the_viewer_scrolled_to_the_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "src/main.rs", numbered_lines(300).as_bytes());
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", b"error at src/main.rs:250:5 here");
    ctrl_click_cell(&mut h, 12, 0);

    assert_eq!(h.opened(), [], "no VS Code");
    let id = one_open(&mut h, &file);
    answer_open(&mut h, &id, &file);
    assert_eq!(
        shown(&mut h),
        Some((ViewerOrigin::Link, ViewerLoad::Loaded, Some(250)))
    );
    assert!(drawn(&mut h, "viewer-line-v1-250"), "the line is in view");
    assert!(!drawn(&mut h, "viewer-line-v1-1"), "scrolled past the top");
}

#[gpui::test]
fn a_binary_file_still_goes_to_the_default_app(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "out/blob.dat", b"PK\0\x03binary");
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", b"wrote out/blob.dat");
    ctrl_click_cell(&mut h, 9, 0);

    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
    assert!(opens_sent(&mut h).is_empty(), "no viewer tab");
}

#[gpui::test]
fn a_big_file_still_goes_to_the_default_app(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let big = vec![b'a'; 2 * 1024 * 1024 + 1];
    let file = write_file(dir.path(), "logs/big.log", &big);
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", b"wrote logs/big.log");
    ctrl_click_cell(&mut h, 9, 0);

    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
    assert!(opens_sent(&mut h).is_empty(), "no viewer tab");
    assert!(
        toasts(&mut h).contains(&(
            "Too large for the viewer, opened externally".to_owned(),
            None
        )),
        "a toast says why: {:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn a_file_with_bad_bytes_past_the_probe_head_opens_externally(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut bytes = vec![b'a'; 50 * 1024];
    bytes.push(0x93);
    let file = write_file(dir.path(), "logs/odd.log", &bytes);
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", b"wrote logs/odd.log");
    ctrl_click_cell(&mut h, 9, 0);

    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
    assert!(opens_sent(&mut h).is_empty(), "no viewer tab");
}

#[gpui::test]
fn reopening_a_loaded_viewer_tab_reads_the_file_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.txt", b"first\n");
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", b"wrote docs/notes.txt");
    ctrl_click_cell(&mut h, 9, 0);
    let id = one_open(&mut h, &file);
    answer_open(&mut h, &id, &file);
    assert!(drawn(&mut h, "viewer-line-v1-1"));
    assert!(!drawn(&mut h, "viewer-line-v1-2"), "one line read");

    fs::write(&file, b"first\nsecond\n").expect("change the file");
    h.click_on("tab-t1");
    ctrl_click_cell(&mut h, 9, 0);
    let id = one_open(&mut h, &file);
    h.send(DaemonMessage::ViewerTabOpened {
        id,
        tab_id: "v1".to_owned(),
    });

    assert_eq!(
        shown(&mut h),
        Some((ViewerOrigin::Link, ViewerLoad::Loaded, None))
    );
    assert!(drawn(&mut h, "viewer-line-v1-2"), "the new line shows");
    h.click_on("viewer-copy-all-v1");
    assert_eq!(h.clipboard().as_deref(), Some("first\nsecond\n"));
}

#[gpui::test]
fn with_the_setting_on_a_link_opens_as_before(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_open_externally(&dir);
    let notes = write_file(dir.path(), "docs/notes.md", b"# Notes\n");
    let main = write_file(dir.path(), "src/main.rs", b"fn main() {}\n");
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", b"wrote docs/notes.md\r\nat src/main.rs:1:4");
    ctrl_click_cell(&mut h, 9, 0);
    ctrl_click_cell(&mut h, 5, 1);

    assert_eq!(
        h.opened(),
        [
            Opened::DefaultApp(notes),
            Opened::VsCode {
                path: main,
                line: 1,
                column: 4
            }
        ]
    );
    assert!(opens_sent(&mut h).is_empty(), "no viewer tab");
}

#[gpui::test]
fn the_open_in_default_app_button_dispatches_a_default_app_job(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.txt", b"hello\n");
    let mut h = showing(cx, &dir, &file);
    h.click_on("viewer-open-default-v1");
    assert_eq!(h.opened(), [Opened::DefaultApp(file.clone())]);
    h.click_on("viewer-reveal-v1");
    assert_eq!(
        h.opened(),
        [Opened::DefaultApp(file.clone()), Opened::Reveal(file)]
    );
}

#[gpui::test]
fn an_executable_script_is_viewed_and_the_button_still_asks_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "tools/run.bat", b"@echo off\r\n");
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", b"run tools/run.bat");
    ctrl_click_cell(&mut h, 8, 0);

    assert_eq!(
        h.run_confirm(),
        None,
        "viewing runs nothing, so nothing asks"
    );
    assert_eq!(h.opened(), []);
    let id = one_open(&mut h, &file);
    answer_open(&mut h, &id, &file);
    assert!(drawn(&mut h, "viewer-line-v1-1"));

    h.click_on("viewer-open-default-v1");
    assert_eq!(h.opened(), [], "nothing runs before the answer");
    assert_eq!(
        h.run_confirm(),
        Some(("Run run.bat?".to_owned(), file.display().to_string()))
    );
}

/// A harness on the remote host [`HOST`], its downloads under `dir`; returns
/// the download root.
fn on_remote<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> (Harness<'a>, PathBuf) {
    let fixture = Fixture::single(session("s1").worktree("r1").build());
    let mut h = Harness::on_target(cx, dir, Target::tailnet(HOST, None), &fixture);
    let root = dir.path().join("remote-files");
    let set = root.clone();
    h.root_update(move |view, _, _| view.set_remote_files_root(set));
    h.sent();
    (h, root)
}

/// Fetches `rel` from the repo `r1` with `line`, the daemon streaming
/// `bytes`; returns the saved copy.
fn fetch(h: &mut Harness<'_>, root: &Path, rel: &str, line: Option<u32>, bytes: &[u8]) -> PathBuf {
    let request = FetchRequest {
        repo_id: "r1".to_owned(),
        repo_name: "repo".to_owned(),
        worktree_path: Some("C:/wt/r1".to_owned()),
        rel_path: rel.to_owned(),
        line,
        column: None,
        open_when_done: true,
    };
    let id = h
        .root_update(move |view, _, cx| view.fetch_remote_file(request, cx))
        .expect("a fetch on a remote host starts");
    h.sent();
    let size = u64::try_from(bytes.len()).expect("a small file");
    h.send(DaemonMessage::FileFetchStarted {
        id: id.clone(),
        resolved_path: r"\\?\C:\repos\repo\file".to_owned(),
        size,
    });
    h.send(DaemonMessage::FileChunk {
        id: id.clone(),
        seq: 0,
        data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
    });
    h.send(DaemonMessage::FileFetchDone { id });
    rel.split('/')
        .fold(root.join(HOST).join("repo"), |path, part| path.join(part))
}

#[gpui::test]
fn on_a_remote_host_a_fetched_file_opens_in_the_viewer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);
    let saved = fetch(&mut h, &root, "src/main.rs", Some(2), b"fn main() {\n}\n");

    assert_eq!(h.opened(), [], "no VS Code");
    let id = one_open(&mut h, &saved);
    answer_open(&mut h, &id, &saved);
    assert_eq!(
        shown(&mut h),
        Some((ViewerOrigin::Fetched, ViewerLoad::Loaded, Some(2)))
    );
    assert!(drawn(&mut h, "viewer-line-v1-2"));
}

#[gpui::test]
fn on_a_remote_host_the_button_takes_the_fetched_route_and_refuses_a_path_outside_the_download_root(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let (mut h, root) = on_remote(cx, &dir);
    let saved = fetch(&mut h, &root, "docs/notes.txt", None, b"first\nsecond\n");
    let id = one_open(&mut h, &saved);
    answer_open(&mut h, &id, &saved);

    h.click_on("viewer-open-default-v1");
    assert_eq!(h.opened(), [Opened::DefaultApp(saved.clone())]);

    let elsewhere = dir.path().join("elsewhere");
    h.root_update(move |view, _, _| view.set_remote_files_root(elsewhere));
    h.click_on("viewer-open-default-v1");
    assert_eq!(
        h.opened(),
        [Opened::DefaultApp(saved)],
        "a path outside the download root does not open"
    );
    assert!(
        toasts(&mut h).contains(&(
            "Couldn't open".to_owned(),
            Some("Only files in the folder for fetched files open here.".to_owned())
        )),
        "the refusal says why: {:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn on_a_remote_host_a_local_path_link_is_still_refused(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").worktree("r1").build());
    let mut h = Harness::on_target(cx, &dir, Target::tailnet(HOST, None), &fixture);
    h.answer_scrollback("s1", b"");
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h.pty("s1", br"wrote C:\wt\x\docs\notes.md");
    ctrl_click_cell(&mut h, 9, 0);

    assert_eq!(h.opened(), [], "a host path never opens on this PC");
    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::OpenViewerTab { .. })),
        "no viewer tab: {sent:?}"
    );
    let fetches: Vec<String> = sent
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::FetchFile { path, .. } => Some(path),
            _ => None,
        })
        .collect();
    assert_eq!(
        fetches,
        ["docs/notes.md".to_owned()],
        "the host's file is fetched instead"
    );
}

#[gpui::test]
fn on_a_remote_host_a_local_viewer_tabs_buttons_open_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.txt", b"local\n");
    let mut fixture = Fixture::single(session("s1").worktree("r1").build());
    fixture.tabs.push(viewer_tab("v1", &file));
    let mut h = Harness::on_target(cx, &dir, Target::tailnet(HOST, None), &fixture);
    h.click_on("tab-v1");
    h.sent();

    assert_eq!(
        shown(&mut h),
        Some((ViewerOrigin::NotAvailable, ViewerLoad::NotAvailable, None))
    );
    assert!(
        drawn(&mut h, "viewer-banner-v1"),
        "the remote-host note shows"
    );
    assert!(!drawn(&mut h, "viewer-line-v1-1"), "nothing is read");
    h.click_on("viewer-open-default-v1");
    h.click_on("viewer-reveal-v1");
    assert_eq!(h.opened(), [], "the disabled buttons open nothing");
    assert!(h.sent().is_empty(), "and ask nothing of the host");
}

#[gpui::test]
fn the_gutter_numbers_every_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "src/lib.rs", b"one\r\ntwo\nthree");
    let mut h = showing(cx, &dir, &file);
    for line in 1..=3 {
        assert!(
            drawn(&mut h, &format!("viewer-line-v1-{line}")),
            "line {line}"
        );
    }
    assert!(!drawn(&mut h, "viewer-line-v1-4"), "three lines, no fourth");
}

#[gpui::test]
fn copy_path_copies_the_full_path(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.txt", b"alpha\nbeta\n");
    let mut h = showing(cx, &dir, &file);
    h.click_on("viewer-copy-path-v1");
    assert_eq!(h.clipboard(), Some(file.to_string_lossy().into_owned()));
    h.click_on("viewer-copy-all-v1");
    assert_eq!(h.clipboard().as_deref(), Some("alpha\nbeta\n"));
}

/// Five lines, the second ending in CRLF.
const FIVE_LINES: &[u8] = b"one\ntwo\r\nthree\nfour\nfive\n";

/// A local harness whose tab `v1` views a file of [`FIVE_LINES`], shown.
fn five_lines<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let file = write_file(dir.path(), "src/five.txt", FIVE_LINES);
    let mut h = showing(cx, dir, &file);
    assert!(drawn(&mut h, "viewer-line-v1-5"), "the file is shown");
    h
}

/// Viewer tab `v1`'s selected lines, counted from zero.
fn selected(h: &mut Harness<'_>) -> Option<std::ops::Range<usize>> {
    h.root(|root, _| root.viewer_tab("v1").and_then(ViewerTab::selected_lines))
}

#[gpui::test]
fn a_click_on_a_gutter_number_selects_the_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = five_lines(cx, &dir);
    assert_eq!(selected(&mut h), None, "nothing is selected at first");
    h.click_on("viewer-line-v1-2");
    assert_eq!(selected(&mut h), Some(1..2));
    h.click_on("viewer-line-v1-4");
    assert_eq!(selected(&mut h), Some(3..4), "a plain click starts over");
}

#[gpui::test]
fn a_click_on_a_lines_text_selects_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = five_lines(cx, &dir);
    h.click_on("viewer-text-v1-3");
    assert_eq!(selected(&mut h), Some(2..3));
}

#[gpui::test]
fn a_shift_click_selects_the_line_range(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = five_lines(cx, &dir);
    h.click_on("viewer-line-v1-2");
    let fourth = h.center("viewer-text-v1-4");
    h.click(fourth, Modifiers::shift());
    assert_eq!(selected(&mut h), Some(1..4), "down from the anchor");
    let first = h.center("viewer-line-v1-1");
    h.click(first, Modifiers::shift());
    assert_eq!(selected(&mut h), Some(0..2), "up from the same anchor");
}

#[gpui::test]
fn a_drag_over_rows_selects_them(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = five_lines(cx, &dir);
    let from = h.center("viewer-line-v1-2");
    let to = h.center("viewer-text-v1-4");
    h.drag(from, to, [Modifiers::none(), Modifiers::none()]);
    assert_eq!(selected(&mut h), Some(1..4));
}

#[gpui::test]
fn a_drag_released_off_the_rows_ends_the_selection_drag(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = five_lines(cx, &dir);
    let third = h.center("viewer-line-v1-3");
    let header = h.center("viewer-copy-path-v1");
    h.drag(third, header, [Modifiers::none(), Modifiers::none()]);
    let before = selected(&mut h);
    assert!(before.is_some(), "the press on line 3 selected it");
    let fifth = h.center("viewer-text-v1-5");
    h.drag(header, fifth, [Modifiers::none(), Modifiers::none()]);
    assert_eq!(
        selected(&mut h),
        before,
        "a drag begun on the header does not extend the released selection"
    );
}

#[gpui::test]
fn a_drag_past_the_last_visible_row_selects_up_to_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = five_lines(cx, &dir);
    let second = h.center("viewer-line-v1-2");
    let fifth = h.bounds("viewer-text-v1-5");
    let below = gpui::point(fifth.center().x, fifth.bottom() + fifth.size.height * 3.0);
    h.drag(second, below, [Modifiers::none(), Modifiers::none()]);
    assert_eq!(
        selected(&mut h),
        Some(1..5),
        "the drag below the last row stops at it"
    );
}

#[gpui::test]
fn ctrl_c_copies_the_selected_lines_joined_with_newlines(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = five_lines(cx, &dir);
    h.set_clipboard("before");
    h.click_on("viewer-line-v1-2");
    let third = h.center("viewer-line-v1-3");
    h.click(third, Modifiers::shift());
    h.keys("ctrl-c");
    assert_eq!(
        h.clipboard().as_deref(),
        Some("two\nthree"),
        "no gutter numbers, no CR, no trailing newline"
    );
}

#[gpui::test]
fn ctrl_c_with_no_selection_copies_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = five_lines(cx, &dir);
    h.set_clipboard("before");
    h.keys("ctrl-c");
    assert_eq!(h.clipboard().as_deref(), Some("before"));
}

#[gpui::test]
fn ctrl_a_then_ctrl_c_copies_the_whole_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = five_lines(cx, &dir);
    h.keys("ctrl-a");
    assert_eq!(selected(&mut h), Some(0..5));
    h.keys("ctrl-c");
    assert_eq!(
        h.clipboard().as_deref(),
        Some("one\ntwo\nthree\nfour\nfive")
    );
}

/// 300 lines of Rust, `needle` on lines 3 and 250 and `Needle` on line 120.
fn needles() -> String {
    (1..=300)
        .map(|n| match n {
            3 | 250 => format!("let needle_{n} = {n};\n"),
            120 => "// Needle\n".to_owned(),
            _ => format!("let line_{n} = {n};\n"),
        })
        .collect::<Vec<_>>()
        .concat()
}

/// A local harness whose tab `v1` views a file of [`needles`], shown.
fn needles_shown<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let file = write_file(dir.path(), "src/needles.rs", needles().as_bytes());
    let mut h = showing(cx, dir, &file);
    assert!(drawn(&mut h, "viewer-line-v1-1"), "the file is shown");
    h
}

/// Viewer tab `v1`'s find count; `None` while its find bar does not show.
fn find_label(h: &mut Harness<'_>) -> Option<String> {
    h.cx.run_until_parked();
    h.root(|root, _| root.viewer_tab("v1").and_then(ViewerTab::find_label))
}

/// Whether viewer tab `v1`'s find field has the keyboard.
fn find_focused(h: &mut Harness<'_>) -> bool {
    h.cx.run_until_parked();
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).viewer_find_focused("v1", window, cx))
}

/// Whether viewer tab `v1`'s body has the keyboard.
fn tab_focused(h: &mut Harness<'_>) -> bool {
    h.cx.run_until_parked();
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).viewer_tab_focused("v1", window))
}

/// Opens tab `v1`'s find bar and types `query` into it.
fn find(h: &mut Harness<'_>, query: &str) {
    h.keys("ctrl-f");
    h.cx.simulate_input(query);
}

#[gpui::test]
fn ctrl_f_opens_the_find_bar_and_focuses_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = five_lines(cx, &dir);
    assert_eq!(find_label(&mut h), None, "no find bar at first");
    h.keys("ctrl-f");
    assert!(drawn(&mut h, "viewer-find-v1"));
    assert_eq!(
        find_label(&mut h).as_deref(),
        Some(""),
        "no query, no count"
    );
    assert!(find_focused(&mut h), "the query field has the keyboard");
    h.click_on("viewer-line-v1-2");
    assert!(!find_focused(&mut h), "a press on a row takes it back");
    h.keys("ctrl-f");
    assert!(find_focused(&mut h), "Ctrl+F gives it the keyboard again");
}

#[gpui::test]
fn typing_in_the_find_bar_counts_matches(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = needles_shown(cx, &dir);
    find(&mut h, "needle");
    assert_eq!(find_label(&mut h).as_deref(), Some("1 of 3"));
    assert!(drawn(&mut h, "viewer-find-count-v1"));
    h.cx.simulate_input("_2");
    assert_eq!(find_label(&mut h).as_deref(), Some("1 of 1"));
    h.cx.simulate_input("x");
    assert_eq!(find_label(&mut h).as_deref(), Some("0 of 0"));
    h.keys("ctrl-a backspace");
    assert_eq!(find_label(&mut h).as_deref(), Some(""), "an empty query");
}

#[gpui::test]
fn enter_goes_to_the_next_match_and_scrolls_to_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = needles_shown(cx, &dir);
    find(&mut h, "needle");
    assert_eq!(find_label(&mut h).as_deref(), Some("1 of 3"));
    h.keys("enter");
    assert_eq!(find_label(&mut h).as_deref(), Some("2 of 3"));
    assert!(drawn(&mut h, "viewer-line-v1-120"), "scrolled to the match");
    h.keys("enter");
    assert_eq!(find_label(&mut h).as_deref(), Some("3 of 3"));
    assert!(drawn(&mut h, "viewer-line-v1-250"), "scrolled to the match");
    h.keys("enter");
    assert_eq!(find_label(&mut h).as_deref(), Some("1 of 3"), "it wraps");
}

#[gpui::test]
fn shift_enter_goes_to_the_previous_match(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = needles_shown(cx, &dir);
    find(&mut h, "needle");
    h.keys("shift-enter");
    assert_eq!(
        find_label(&mut h).as_deref(),
        Some("3 of 3"),
        "before the first is the last"
    );
    assert!(drawn(&mut h, "viewer-line-v1-250"), "scrolled to the match");
    h.keys("shift-enter");
    assert_eq!(find_label(&mut h).as_deref(), Some("2 of 3"));
    assert!(drawn(&mut h, "viewer-line-v1-120"), "scrolled to the match");
}

#[gpui::test]
fn f3_and_shift_f3_step_through_matches(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = needles_shown(cx, &dir);
    find(&mut h, "needle");
    h.keys("f3");
    assert_eq!(find_label(&mut h).as_deref(), Some("2 of 3"));
    h.keys("shift-f3");
    assert_eq!(find_label(&mut h).as_deref(), Some("1 of 3"));

    h.click_on("viewer-line-v1-3");
    assert!(!find_focused(&mut h), "the body has the keyboard");
    h.keys("f3");
    assert_eq!(
        find_label(&mut h).as_deref(),
        Some("2 of 3"),
        "F3 from the body"
    );
    h.keys("enter");
    assert_eq!(
        find_label(&mut h).as_deref(),
        Some("3 of 3"),
        "Enter from the body"
    );
    h.set_clipboard("before");
    h.keys("ctrl-c");
    assert_eq!(
        h.clipboard().as_deref(),
        Some("let needle_3 = 3;"),
        "Ctrl+C from the body still copies the selection"
    );
}

#[gpui::test]
fn escape_closes_the_find_bar_and_returns_the_keyboard_to_the_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = needles_shown(cx, &dir);
    find(&mut h, "needle");
    assert!(find_focused(&mut h));
    h.keys("escape");
    assert_eq!(find_label(&mut h), None, "the bar is closed");
    assert!(tab_focused(&mut h), "the tab has the keyboard");
    h.keys("ctrl-f");
    assert_eq!(
        find_label(&mut h).as_deref(),
        Some("1 of 3"),
        "the query is kept"
    );
}

#[gpui::test]
fn the_case_toggle_makes_find_case_sensitive(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = needles_shown(cx, &dir);
    find(&mut h, "needle");
    assert_eq!(find_label(&mut h).as_deref(), Some("1 of 3"), "Needle too");
    h.click_on("viewer-find-case-v1");
    assert_eq!(find_label(&mut h).as_deref(), Some("1 of 2"));
    h.click_on("viewer-find-case-v1");
    assert_eq!(find_label(&mut h).as_deref(), Some("1 of 3"));
}

#[gpui::test]
fn ctrl_f_on_a_rendered_markdown_tab_switches_to_source_and_opens_find(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = dir.path().join("native-ui.json");
    fs::write(&ui, json!({ "viewer_markdown_rendered": true }).to_string())
        .expect("seed native-ui.json");
    let file = write_file(dir.path(), "docs/notes.md", b"# Notes\n\nfind me\n");
    let mut h = showing(cx, &dir, &file);
    let mode =
        |h: &mut Harness<'_>| h.root(|root, _| root.viewer_tab("v1").and_then(ViewerTab::markdown));
    assert_eq!(mode(&mut h), Some(MarkdownMode::Rendered));
    assert!(drawn(&mut h, "viewer-md-block-v1-0"), "rendered");

    h.keys("ctrl-f");
    assert_eq!(mode(&mut h), Some(MarkdownMode::Source));
    assert!(drawn(&mut h, "viewer-line-v1-3"), "the source shows");
    assert!(find_focused(&mut h));
    h.cx.simulate_input("find");
    assert_eq!(find_label(&mut h).as_deref(), Some("1 of 1"));
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&ui).expect("native-ui.json"))
            .expect("native-ui.json is JSON");
    assert_eq!(
        saved["viewer_markdown_rendered"],
        json!(true),
        "the saved default is untouched"
    );
}

#[gpui::test]
fn find_is_off_on_a_tab_with_no_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let gone = dir.path().join("gone.txt");
    let mut h = showing(cx, &dir, &gone);
    assert_eq!(
        h.root(|root, _| root.viewer_tab("v1").map(ViewerTab::load)),
        Some(ViewerLoad::Missing)
    );
    h.keys("ctrl-f");
    assert_eq!(find_label(&mut h), None);
    assert!(!drawn(&mut h, "viewer-find-v1"));
    assert!(tab_focused(&mut h), "the tab keeps the keyboard");
}

#[gpui::test]
fn switching_to_rendered_with_the_bar_open_returns_the_keyboard_to_the_tab(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.md", b"# Notes\n\nfind me\n");
    let mut h = showing(cx, &dir, &file);
    find(&mut h, "find");
    assert!(find_focused(&mut h));
    h.click_on("viewer-md-rendered-v1");
    assert_eq!(find_label(&mut h), None, "the bar goes with the source");
    assert!(tab_focused(&mut h), "the tab has the keyboard");
    h.click_on("viewer-md-source-v1");
    h.keys("ctrl-a");
    assert_eq!(selected(&mut h), Some(0..3), "Ctrl+A reaches the tab");
}

/// Opens `file` again from this client, at `line`; the daemon names `v1`.
fn reopen(h: &mut Harness<'_>, file: &Path, line: Option<u32>) {
    let path = file.to_string_lossy().into_owned();
    h.root_update(move |view, _, _| view.open_viewer(&path, line));
    let id = one_open(h, file);
    h.send(DaemonMessage::ViewerTabOpened {
        id,
        tab_id: "v1".to_owned(),
    });
}

#[gpui::test]
fn a_reread_with_the_bar_open_returns_the_keyboard_to_the_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "src/needles.rs", needles().as_bytes());
    let mut h = showing(cx, &dir, &file);
    find(&mut h, "needle");
    assert!(find_focused(&mut h));
    reopen(&mut h, &file, None);
    assert!(tab_focused(&mut h), "the tab has the keyboard");
    assert!(!find_focused(&mut h));
    assert_eq!(
        find_label(&mut h).as_deref(),
        Some("1 of 3"),
        "the bar is back with its query once the file is read"
    );
}

#[gpui::test]
fn typing_keeps_the_current_match_when_it_still_matches(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let every_line = (1..=300)
        .map(|n| format!("needle {n}\n"))
        .collect::<Vec<_>>()
        .concat();
    let file = write_file(dir.path(), "src/all.txt", every_line.as_bytes());
    let mut h = showing(cx, &dir, &file);
    reopen(&mut h, &file, Some(200));
    assert!(
        drawn(&mut h, "viewer-line-v1-200"),
        "scrolled down the file"
    );
    h.keys("ctrl-f");
    h.cx.simulate_input("n");
    let first = find_label(&mut h).expect("the bar shows");
    assert_ne!(
        first, "1 of 300",
        "the current match is in view, not at the top"
    );
    for letter in ["e", "e", "d"] {
        h.cx.simulate_input(letter);
        assert_eq!(
            find_label(&mut h).as_deref(),
            Some(first.as_str()),
            "typing {letter} keeps the current match"
        );
    }
}

#[gpui::test]
fn escape_clears_the_selection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = five_lines(cx, &dir);
    h.click_on("viewer-line-v1-2");
    assert_eq!(selected(&mut h), Some(1..2));
    h.keys("escape");
    assert_eq!(selected(&mut h), None);
    h.set_clipboard("before");
    h.keys("ctrl-c");
    assert_eq!(
        h.clipboard().as_deref(),
        Some("before"),
        "nothing left to copy"
    );
}

/// Scrolls viewer tab `v1`'s lines so row `row` is centred.
fn scroll_to_row(h: &mut Harness<'_>, row: usize) {
    h.root_update(move |root, _, cx| {
        let tab = root.viewer_tab("v1").expect("tab v1");
        tab.scroll()
            .scroll_to_item_strict(row, ScrollStrategy::Center);
        cx.notify();
    });
}

/// How far viewer tab `v1`'s lines are scrolled down, in px.
fn scroll_top(h: &mut Harness<'_>) -> f32 {
    h.cx.run_until_parked();
    h.root(|root, _| {
        let tab = root.viewer_tab("v1").expect("tab v1");
        -tab.scroll().0.borrow().base_handle.offset().y / px(1.0)
    })
}

/// Viewer tab `v1`'s line count.
fn line_count(h: &mut Harness<'_>) -> Option<usize> {
    h.cx.run_until_parked();
    h.root(|root, _| root.viewer_tab("v1").map(ViewerTab::line_count))
}

/// Viewer tab `v1`'s text, as Copy all copies it.
fn copied_all(h: &mut Harness<'_>) -> Option<String> {
    h.click_on("viewer-copy-all-v1");
    h.clipboard()
}

#[gpui::test]
fn an_activation_with_a_changed_mtime_reloads_and_keeps_the_scroll(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "src/long.rs", numbered_lines(300).as_bytes());
    let mut h = showing(cx, &dir, &file);
    scroll_to_row(&mut h, 150);
    let top = scroll_top(&mut h);
    assert!(top > 0.0, "scrolled down the file");
    assert!(drawn(&mut h, "viewer-line-v1-151"));

    h.click_on("tab-t1");
    fs::write(&file, numbered_lines(301)).expect("grow the file");
    h.click_on("tab-v1");

    assert_eq!(line_count(&mut h), Some(301), "the file is read again");
    assert!(
        (scroll_top(&mut h) - top).abs() < 0.5,
        "the scroll stays at {top}"
    );
    assert!(drawn(&mut h, "viewer-line-v1-151"));
    scroll_to_row(&mut h, 300);
    assert!(drawn(&mut h, "viewer-line-v1-301"), "the new line draws");
}

#[gpui::test]
fn an_activation_with_an_unchanged_file_reads_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.txt", b"first\n");
    let mut h = showing(cx, &dir, &file);
    let modified = fs::metadata(&file)
        .and_then(|meta| meta.modified())
        .expect("the file's mtime");
    let set_modified = |at| {
        fs::File::options()
            .write(true)
            .open(&file)
            .and_then(|handle| handle.set_modified(at))
            .expect("set the file's mtime");
    };

    h.click_on("tab-t1");
    fs::write(&file, b"other\n").expect("rewrite the file at the same size");
    set_modified(modified);
    h.click_on("tab-v1");
    assert_eq!(
        copied_all(&mut h).as_deref(),
        Some("first\n"),
        "the same mtime and size read nothing"
    );

    h.click_on("tab-t1");
    set_modified(modified + std::time::Duration::from_secs(2));
    h.click_on("tab-v1");
    assert_eq!(
        copied_all(&mut h).as_deref(),
        Some("other\n"),
        "a newer mtime reads it"
    );
}

#[gpui::test]
fn a_file_deleted_before_activation_shows_the_last_read_banner(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.txt", b"first\nsecond\n");
    let mut h = showing(cx, &dir, &file);
    assert!(drawn(&mut h, "viewer-line-v1-2"));

    h.click_on("tab-t1");
    fs::remove_file(&file).expect("delete the file");
    h.click_on("tab-v1");

    assert_eq!(
        shown(&mut h),
        Some((ViewerOrigin::Link, ViewerLoad::Missing, None))
    );
    assert_eq!(
        h.root(|root, _| root.viewer_tab("v1").and_then(ViewerTab::note)),
        Some("The file is gone or cannot be read. Showing the last read.")
    );
    assert!(drawn(&mut h, "viewer-banner-v1"));
    assert!(drawn(&mut h, "viewer-line-v1-2"), "the last read stays");
    assert_eq!(copied_all(&mut h).as_deref(), Some("first\nsecond\n"));
}

#[gpui::test]
fn a_not_available_tab_does_not_reload_on_activation(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.txt", b"local\n");
    let mut fixture = Fixture::single(session("s1").worktree("r1").build());
    fixture.tabs.push(viewer_tab("v1", &file));
    let mut h = Harness::on_target(cx, &dir, Target::tailnet(HOST, None), &fixture);
    h.click_on("tab-v1");
    h.click_on("tab-t1");
    fs::write(&file, b"local\nchanged\n").expect("change the file");
    h.click_on("tab-v1");

    assert_eq!(
        shown(&mut h),
        Some((ViewerOrigin::NotAvailable, ViewerLoad::NotAvailable, None))
    );
    assert_eq!(line_count(&mut h), Some(0));
    assert!(!drawn(&mut h, "viewer-line-v1-1"), "nothing is read");
    assert!(drawn(&mut h, "viewer-banner-v1"));
}

#[gpui::test]
fn a_reopen_still_rereads_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.txt", b"first\n");
    let mut h = showing(cx, &dir, &file);
    assert_eq!(line_count(&mut h), Some(1));

    h.click_on("tab-t1");
    fs::write(&file, b"first\nsecond\n").expect("change the file");
    reopen(&mut h, &file, None);

    assert_eq!(
        shown(&mut h),
        Some((ViewerOrigin::Link, ViewerLoad::Loaded, None))
    );
    assert_eq!(line_count(&mut h), Some(2));
    assert!(drawn(&mut h, "viewer-line-v1-2"), "the new line shows");
    assert_eq!(
        h.root(|root, _| root.viewer_tab("v1").and_then(ViewerTab::note)),
        None
    );
    assert_eq!(copied_all(&mut h).as_deref(), Some("first\nsecond\n"));
}
