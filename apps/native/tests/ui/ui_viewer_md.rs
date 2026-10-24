//! Markdown viewer specs: a Markdown file opens rendered, built a row at a
//! time; the header's Rendered | Source toggle, shown once the text loads,
//! switches the clicked tab and saves the default new tabs open in; a file
//! over the render cap opens as source with the toggle disabled; a link with
//! a line opens as source at the line; other files have no toggle; a link
//! opens a URL in the browser, a file through the viewer's routing, scrolls
//! to a heading, or is refused with a toast.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{Modifiers, TestAppContext};
use protocol::{ClientMessage, DaemonMessage, TabEntry};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{MarkdownMode, ViewerLoad, ViewerOrigin, ViewerTab};
use serde_json::{Value, json};
use support::{Fixture, Harness, Opened, TestDir, session};

/// A Markdown file with one block of each kind the view draws.
const SAMPLE: &str = "# Title\n\nSome *text* with `code` and a [link](https://example.com).\n\n\
- one\n- [x] done\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> quoted\n\n---\n";

/// Writes `text` to `rel` under `root`; returns its path.
fn write_file(root: &Path, rel: &str, text: &str) -> PathBuf {
    let path = root.join(rel);
    let parent = path.parent().expect("a file has a parent folder");
    fs::create_dir_all(parent).expect("create the file's folder");
    fs::write(&path, text).expect("write the file");
    path
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

/// A local harness whose tabs view `paths` as `v1`, `v2`, …, with `v1`
/// shown.
fn showing<'a>(cx: &'a mut TestAppContext, dir: &TestDir, paths: &[&Path]) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s1").build());
    for (index, path) in paths.iter().enumerate() {
        fixture
            .tabs
            .push(viewer_tab(&format!("v{}", index + 1), path));
    }
    let mut h = Harness::with(cx, dir, &fixture);
    h.click_on("tab-v1");
    h.sent();
    h
}

/// Whether an element tagged `selector` was painted.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.cx.run_until_parked();
    let selector: &'static str = Box::leak(selector.to_owned().into_boxed_str());
    h.cx.debug_bounds(selector).is_some()
}

/// Viewer tab `tab_id`'s Markdown mode.
fn mode(h: &mut Harness<'_>, tab_id: &str) -> Option<MarkdownMode> {
    h.root(|root, _| root.viewer_tab(tab_id).and_then(ViewerTab::markdown))
}

fn saved_ui(dir: &TestDir) -> Value {
    serde_json::from_str(
        &fs::read_to_string(dir.path().join("native-ui.json")).expect("native-ui.json"),
    )
    .expect("native-ui.json is JSON")
}

#[gpui::test]
fn a_markdown_file_opens_rendered(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.md", SAMPLE);
    let mut h = showing(cx, &dir, &[&file]);

    assert_eq!(
        h.root(|root, _| root.viewer_tab("v1").map(ViewerTab::load)),
        Some(ViewerLoad::Loaded)
    );
    assert_eq!(mode(&mut h, "v1"), Some(MarkdownMode::Rendered));
    assert!(drawn(&mut h, "viewer-md-v1"), "the rendered body shows");
    for block in 0..3 {
        assert!(
            drawn(&mut h, &format!("viewer-md-block-v1-{block}")),
            "block {block} shows"
        );
    }
    assert!(!drawn(&mut h, "viewer-line-v1-1"), "no source lines");
    assert!(drawn(&mut h, "viewer-md-rendered-v1"));
    assert!(drawn(&mut h, "viewer-md-source-v1"));
}

#[gpui::test]
fn the_source_toggle_shows_the_text_and_saves(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.md", SAMPLE);
    let mut h = showing(cx, &dir, &[&file]);
    assert!(drawn(&mut h, "viewer-md-v1"));

    h.click_on("viewer-md-source-v1");
    assert_eq!(mode(&mut h, "v1"), Some(MarkdownMode::Source));
    assert!(drawn(&mut h, "viewer-line-v1-1"), "the source shows");
    assert_eq!(saved_ui(&dir)["viewer_markdown_rendered"], json!(false));

    // A painted element's bounds outlive the frame that last drew it, so the
    // way back is checked by the mode and the save.
    h.click_on("viewer-md-rendered-v1");
    assert_eq!(mode(&mut h, "v1"), Some(MarkdownMode::Rendered));
    assert_eq!(saved_ui(&dir)["viewer_markdown_rendered"], json!(true));
}

#[gpui::test]
fn the_toggle_leaves_other_open_tabs_alone(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let first = write_file(dir.path(), "docs/a.md", SAMPLE);
    let second = write_file(dir.path(), "docs/b.md", SAMPLE);
    let mut h = showing(cx, &dir, &[&first, &second]);
    assert_eq!(mode(&mut h, "v2"), Some(MarkdownMode::Rendered));

    h.click_on("viewer-md-source-v1");
    assert_eq!(mode(&mut h, "v1"), Some(MarkdownMode::Source));
    assert_eq!(
        mode(&mut h, "v2"),
        Some(MarkdownMode::Rendered),
        "an open tab keeps its mode"
    );

    let third = write_file(dir.path(), "docs/c.md", SAMPLE);
    h.send(DaemonMessage::TabUpdated {
        tab: viewer_tab("v3", &third),
    });
    h.cx.run_until_parked();
    assert_eq!(
        mode(&mut h, "v3"),
        Some(MarkdownMode::Source),
        "a new tab opens in the saved default"
    );
}

#[gpui::test]
fn a_large_markdown_file_opens_as_source_with_the_toggle_disabled(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let big = "para\n\n".repeat(100_000);
    let file = write_file(dir.path(), "docs/big.md", &big);
    let mut h = showing(cx, &dir, &[&file]);

    assert_eq!(mode(&mut h, "v1"), Some(MarkdownMode::Source));
    assert_eq!(
        h.root(|root, _| root.viewer_tab("v1").map(ViewerTab::markdown_too_large)),
        Some(true)
    );
    assert!(drawn(&mut h, "viewer-line-v1-1"), "the source shows");
    assert!(!drawn(&mut h, "viewer-md-v1"));
    assert!(drawn(&mut h, "viewer-md-rendered-v1"), "the toggle shows");

    let at = h.center("viewer-md-rendered-v1");
    h.hover(at, Modifiers::none());
    h.advance(Duration::from_millis(600));
    assert!(
        drawn(&mut h, "tooltip=Too large to render as Markdown"),
        "the disabled toggle says why"
    );

    h.click_on("viewer-md-rendered-v1");
    assert_eq!(
        mode(&mut h, "v1"),
        Some(MarkdownMode::Source),
        "the toggle is disabled"
    );
    assert!(!drawn(&mut h, "viewer-md-v1"));
}

#[gpui::test]
fn a_huge_code_block_builds_only_visible_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let lines: String = (0..5_000)
        .map(|n| ["let x", &n.to_string(), " = 1;\n"].concat())
        .collect();
    let file = write_file(
        dir.path(),
        "docs/code.md",
        &["```\n", &lines, "```\n"].concat(),
    );
    let mut h = showing(cx, &dir, &[&file]);

    assert!(
        drawn(&mut h, "viewer-md-block-v1-0"),
        "the first line shows"
    );
    assert!(drawn(&mut h, "viewer-md-row-v1-1"), "a line is a row");
    assert!(
        !drawn(&mut h, "viewer-md-row-v1-4990"),
        "a line far below the view is never built"
    );
}

#[gpui::test]
fn the_toggle_is_hidden_until_the_text_loads(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let missing = dir.path().join("docs").join("gone.md");
    let binary = dir.path().join("docs").join("blob.md");
    fs::create_dir_all(dir.path().join("docs")).expect("create docs");
    fs::write(&binary, b"PK\0\x03binary").expect("write the binary file");
    let mut h = showing(cx, &dir, &[&missing, &binary]);

    assert_eq!(
        h.root(|root, _| root.viewer_tab("v1").map(ViewerTab::load)),
        Some(ViewerLoad::Missing)
    );
    assert!(drawn(&mut h, "viewer-banner-v1"));
    assert!(!drawn(&mut h, "viewer-md-rendered-v1"));
    assert!(!drawn(&mut h, "viewer-md-source-v1"));

    h.click_on("tab-v2");
    assert_eq!(
        h.root(|root, _| root.viewer_tab("v2").map(ViewerTab::load)),
        Some(ViewerLoad::Binary)
    );
    assert!(drawn(&mut h, "viewer-banner-v2"));
    assert!(!drawn(&mut h, "viewer-md-rendered-v2"));
    assert!(!drawn(&mut h, "viewer-md-source-v2"));
}

#[gpui::test]
fn a_markdown_link_with_a_line_opens_source_at_the_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let text: String = (1..=80)
        .map(|n| ["line ", &n.to_string(), "\n"].concat())
        .collect();
    write_file(dir.path(), "docs/notes.md", &text);
    let shell = session("s1").shell(&dir.path().to_string_lossy()).build();
    let mut h = Harness::with(cx, &dir, &Fixture::single(shell));
    h.answer_scrollback("s1", b"");
    let pane = h.cell_center("p1", 0, 0);
    h.click(pane, Modifiers::none());
    h.sent();
    h.pty("s1", b"see docs/notes.md:60 here");
    let link = h.cell_center("p1", 6, 0);
    h.click(link, Modifiers::secondary_key());

    let opens: Vec<(String, String)> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::OpenViewerTab { id, path } => Some((id, path)),
            _ => None,
        })
        .collect();
    let [(id, path)] = opens.as_slice() else {
        panic!("one OpenViewerTab is sent, got {opens:?}");
    };
    h.send(DaemonMessage::TabUpdated {
        tab: viewer_tab("v1", Path::new(path)),
    });
    h.send(DaemonMessage::ViewerTabOpened {
        id: id.clone(),
        tab_id: "v1".to_owned(),
    });
    h.cx.run_until_parked();

    assert_eq!(mode(&mut h, "v1"), Some(MarkdownMode::Source));
    assert!(drawn(&mut h, "viewer-line-v1-60"), "the line is in view");

    let other = write_file(dir.path(), "docs/other.md", "# Other\n");
    h.send(DaemonMessage::TabUpdated {
        tab: viewer_tab("v2", &other),
    });
    h.cx.run_until_parked();
    assert_eq!(
        mode(&mut h, "v2"),
        Some(MarkdownMode::Rendered),
        "the saved default is unchanged"
    );
}

#[gpui::test]
fn a_non_markdown_file_has_no_toggle(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.txt", "# not markdown\n");
    let mut h = showing(cx, &dir, &[&file]);

    assert_eq!(mode(&mut h, "v1"), None);
    assert!(drawn(&mut h, "viewer-line-v1-1"));
    assert!(!drawn(&mut h, "viewer-md-rendered-v1"));
    assert!(!drawn(&mut h, "viewer-md-source-v1"));
    assert!(!drawn(&mut h, "viewer-md-v1"));
}

const HOST: &str = "box.tail1234.ts.net";

/// Every toast's title and detail, oldest first.
fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.title.clone(), toast.detail.clone()))
            .collect()
    })
}

/// Clicks a link to `href` in viewer tab `tab_id`'s rendered Markdown.
fn follow(h: &mut Harness<'_>, tab_id: &str, href: &str) {
    let tab_id = tab_id.to_owned();
    let href = href.to_owned();
    h.root_update(move |root, window, cx| root.follow_md_href(&tab_id, &href, window, cx));
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

/// `path` as a resolved link names it: canonical, without the verbatim
/// prefix.
fn canonical(path: &Path) -> PathBuf {
    let resolved = fs::canonicalize(path).expect("canonicalize the file");
    let text = resolved.to_string_lossy();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
}

/// The row at the top of viewer tab `tab_id`'s rendered Markdown.
fn top_row(h: &mut Harness<'_>, tab_id: &str) -> Option<usize> {
    h.cx.run_until_parked();
    h.root(|root, _| root.viewer_markdown_top_row(tab_id))
}

#[gpui::test]
fn an_https_link_click_opens_the_browser_job(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.md", SAMPLE);
    let mut h = showing(cx, &dir, &[&file]);
    assert!(drawn(&mut h, "viewer-md-v1"));

    follow(&mut h, "v1", "https://example.com/page");
    assert_eq!(
        h.opened(),
        [Opened::Url("https://example.com/page".to_owned())]
    );
    assert_eq!(toasts(&mut h), []);
}

#[gpui::test]
fn a_relative_md_link_opens_another_viewer_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.md", "[other](other.md#part)\n");
    let other = write_file(dir.path(), "docs/other.md", "# Other\n");
    let mut h = showing(cx, &dir, &[&file]);
    assert!(drawn(&mut h, "viewer-md-v1"));

    follow(&mut h, "v1", "other.md#part");
    let opens = opens_sent(&mut h);
    let [(id, path)] = opens.as_slice() else {
        panic!("one OpenViewerTab is sent, got {opens:?}");
    };
    assert_eq!(Path::new(path), canonical(&other), "the anchor is dropped");
    assert_eq!(h.opened(), [], "nothing goes out");

    h.send(DaemonMessage::TabUpdated {
        tab: viewer_tab("v2", Path::new(path)),
    });
    h.send(DaemonMessage::ViewerTabOpened {
        id: id.clone(),
        tab_id: "v2".to_owned(),
    });
    h.cx.run_until_parked();
    assert_eq!(mode(&mut h, "v2"), Some(MarkdownMode::Rendered));
    assert!(drawn(&mut h, "viewer-md-v2"), "the second tab shows");
}

#[gpui::test]
fn a_file_scheme_link_toasts_and_opens_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.md", SAMPLE);
    let mut h = showing(cx, &dir, &[&file]);
    assert!(drawn(&mut h, "viewer-md-v1"));

    follow(&mut h, "v1", "file:///C:/notes/other.md");
    assert_eq!(
        toasts(&mut h),
        [(
            "Couldn't open".to_owned(),
            Some("Only http and https links open from a Markdown file.".to_owned())
        )]
    );
    assert_eq!(h.opened(), []);
    assert_eq!(opens_sent(&mut h), []);
}

/// A document whose heading `Target` sits at row 201, with as many rows
/// again below it.
fn long_document() -> String {
    [
        "# Top\n\n",
        &"para\n\n".repeat(200),
        "## Target\n\n",
        &"para\n\n".repeat(200),
    ]
    .concat()
}

#[gpui::test]
fn a_heading_anchor_scrolls(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/long.md", &long_document());
    let mut h = showing(cx, &dir, &[&file]);
    assert!(drawn(&mut h, "viewer-md-v1"));
    assert_eq!(top_row(&mut h, "v1"), Some(0));

    follow(&mut h, "v1", "#target");
    assert_eq!(top_row(&mut h, "v1"), Some(201));
    assert!(drawn(&mut h, "viewer-md-block-v1-201"), "the heading shows");
}

#[gpui::test]
fn an_unknown_anchor_does_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/long.md", &long_document());
    let mut h = showing(cx, &dir, &[&file]);
    assert!(drawn(&mut h, "viewer-md-v1"));

    follow(&mut h, "v1", "#nowhere");
    assert_eq!(top_row(&mut h, "v1"), Some(0));
    assert_eq!(toasts(&mut h), []);
    assert_eq!(h.opened(), []);
}

/// A harness on the remote host [`HOST`] whose tab `v1` shows a fetched
/// copy of `notes.md` holding `text`, under the download root `<dir>/remote-files`;
/// returns the copy's path.
fn fetched<'a>(cx: &'a mut TestAppContext, dir: &TestDir, text: &str) -> (Harness<'a>, PathBuf) {
    let fixture = Fixture::single(session("s1").worktree("r1").build());
    let mut h = Harness::on_target(cx, dir, Target::tailnet(HOST, None), &fixture);
    let root = dir.path().join("remote-files");
    let set = root.clone();
    h.root_update(move |view, _, _| view.set_remote_files_root(set));
    let copy = write_file(&root, &[HOST, "/repo/docs/notes.md"].concat(), text);
    h.send(DaemonMessage::TabUpdated {
        tab: viewer_tab("v1", &copy),
    });
    h.click_on("tab-v1");
    h.sent();
    assert_eq!(
        h.root(|root, _| root.viewer_tab("v1").map(ViewerTab::origin)),
        Some(ViewerOrigin::Fetched)
    );
    assert!(drawn(&mut h, "viewer-md-v1"));
    (h, copy)
}

#[gpui::test]
fn a_fetched_tab_opens_a_fetched_sibling_externally(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, copy) = fetched(cx, &dir, "![diagram](diagram.png)\n");
    let folder = copy.parent().expect("the copy has a folder");
    fs::write(folder.join("diagram.png"), b"\x89PNG\r\n\x1a\n\0\0binary").expect("write the image");

    follow(&mut h, "v1", "diagram.png");
    assert_eq!(toasts(&mut h), [], "no refusal");
    let opened = h.opened();
    let [Opened::DefaultApp(path)] = opened.as_slice() else {
        panic!("the image goes to its default app, got {opened:?}");
    };
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some("diagram.png")
    );
    assert_eq!(opens_sent(&mut h), [], "no viewer tab opens");
}

#[gpui::test]
fn a_fetched_tab_refuses_a_link_outside_the_fetch_folder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    write_file(dir.path(), "outside.md", "# Mine\n");
    let (mut h, _) = fetched(cx, &dir, SAMPLE);

    // From remote-files/<host>/repo/docs, four levels up is the test folder.
    follow(&mut h, "v1", "../../../../outside.md");
    assert_eq!(
        toasts(&mut h),
        [(
            "Couldn't open".to_owned(),
            Some("Only files in the folder for fetched files open here.".to_owned())
        )]
    );
    assert_eq!(h.opened(), []);
    assert_eq!(opens_sent(&mut h), [], "no viewer tab opens");
}

#[gpui::test]
fn a_link_on_a_fetched_copy_with_no_sibling_toasts_no_file_found(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, copy) = fetched(cx, &dir, SAMPLE);

    follow(&mut h, "v1", "missing.md");
    let missing = copy
        .parent()
        .expect("the copy has a folder")
        .join("missing.md");
    assert_eq!(
        toasts(&mut h),
        [(
            "Couldn't open".to_owned(),
            Some(format!("No file found for {}", missing.display()))
        )]
    );
    assert_eq!(h.opened(), []);
    assert_eq!(opens_sent(&mut h), [], "no viewer tab opens");
}
