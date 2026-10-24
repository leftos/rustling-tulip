//! Markdown viewer specs: a Markdown file opens rendered, built a row at a
//! time; the header's Rendered | Source toggle, shown once the text loads,
//! switches the clicked tab and saves the default new tabs open in; a file
//! over the render cap opens as source with the toggle disabled; a link with
//! a line opens as source at the line; other files have no toggle.

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
use rustling_tulip_native::{MarkdownMode, ViewerLoad, ViewerTab};
use serde_json::{Value, json};
use support::{Fixture, Harness, TestDir, session};

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
