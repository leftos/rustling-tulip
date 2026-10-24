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
use gpui::{Modifiers, TestAppContext};
use protocol::{ClientMessage, DaemonMessage, SessionSnapshot, TabEntry};
use rustling_tulip_native::file_fetch::FetchRequest;
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{ViewerLoad, ViewerOrigin};
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
    assert!(drawn(&mut h, "viewer-line-v1-1"), "the file's text shows");
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
