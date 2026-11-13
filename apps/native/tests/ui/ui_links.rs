//! Terminal link specs: Ctrl+hover underlines the link under the mouse and
//! names its gestures; Ctrl+click opens a URL in the browser and a path in
//! VS Code at its line or its default app, through a recording opener;
//! Ctrl+Shift+click on a path opens the link menu, whose entries open it in
//! its app, in the viewer, or in its folder.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::path::{Path, PathBuf};

use gpui::{AppContext as _, Modifiers, MouseButton, TestAppContext, point, px};
use protocol::{ClientMessage, SessionSnapshot, SplitDirection};
use rustling_tulip_native::{OpenOutcome, PopoutKey, ToastKind};
use serde_json::{Value, json};
use support::{Fixture, Harness, Opened, TestDir, pane, session, split, tab};

const URL: &str = "https://example.com/docs";

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

/// Writes `rel` under `root`; returns the path a link to it resolves to.
fn write_file(root: &Path, rel: &str) -> PathBuf {
    let path = root.join(rel);
    let parent = path.parent().expect("a file has a parent folder");
    std::fs::create_dir_all(parent).expect("create the file's folder");
    std::fs::write(&path, "x\n").expect("write the file");
    let resolved = std::fs::canonicalize(&path).expect("canonicalize the file");
    let text = resolved.to_string_lossy();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
}

fn ctrl() -> Modifiers {
    support::secondary_key()
}

fn ctrl_shift() -> Modifiers {
    Modifiers {
        shift: true,
        ..support::secondary_key()
    }
}

/// The paths of the `OpenViewerTab`s sent since the last look.
fn viewer_opens(h: &mut Harness<'_>) -> Vec<PathBuf> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::OpenViewerTab { path, .. } => Some(PathBuf::from(path)),
            _ => None,
        })
        .collect()
}

/// Whether an element tagged `selector` was painted.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.cx.run_until_parked();
    let selector: &'static str = Box::leak(selector.to_owned().into_boxed_str());
    h.cx.debug_bounds(selector).is_some()
}

const BOTH_GESTURES: &str = "pane-link-tip-p1=Ctrl+click to open · Ctrl+Shift+click for options";
const OPEN_ONLY: &str = "pane-link-tip-p1=Ctrl+click to open";

#[gpui::test]
fn plain_click_on_a_url_opens_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.pty("s1", format!("see {URL} here").as_bytes());
    let on = h.cell_center("p1", 6, 0);
    h.hover(on, Modifiers::none());
    assert_eq!(h.hovered_link("p1"), None, "no underline without Ctrl");
    h.click(on, Modifiers::none());
    assert_eq!(h.opened(), []);
}

#[gpui::test]
fn ctrl_click_on_a_url_opens_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.pty("s1", format!("see {URL} here").as_bytes());
    let on = h.cell_center("p1", 10, 0);
    h.click(on, ctrl());
    assert!(!h.in_model("link-menu"), "a URL opens no menu");
    assert_eq!(h.opened(), [Opened::Url(URL.to_owned())]);
    let off = h.cell_center("p1", 1, 0);
    h.click(off, ctrl());
    assert_eq!(
        h.opened().len(),
        1,
        "a Ctrl+click off the link opens nothing"
    );
}

#[gpui::test]
fn ctrl_click_on_a_path_with_line_opens_vscode_at_the_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "src/main.rs");
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty(
        "s1",
        b"error at src/main.rs:12:5 here\r\nnote src/main.rs:7",
    );
    let with_column = h.cell_center("p1", 12, 0);
    h.click(with_column, ctrl());
    let line_only = h.cell_center("p1", 8, 1);
    h.click(line_only, ctrl());
    assert_eq!(
        h.opened(),
        [
            Opened::VsCode {
                path: file.clone(),
                line: 12,
                column: 5,
            },
            Opened::VsCode {
                path: file,
                line: 7,
                column: 1,
            },
        ]
    );
}

#[gpui::test]
fn ctrl_click_on_a_text_path_opens_the_default_app(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.md");
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", b"wrote docs/notes.md");
    let on = h.cell_center("p1", 9, 0);
    h.click(on, ctrl());
    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
    assert!(!h.in_model("link-menu"), "no menu");
    assert_eq!(viewer_opens(&mut h), Vec::<PathBuf>::new(), "no viewer tab");
}

/// A shell in a fresh folder holding `docs/notes.md`, with a Ctrl+Shift+click
/// on the link to it made; returns the harness, the file and the click point.
fn notes_menu<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
) -> (Harness<'a>, PathBuf, gpui::Point<gpui::Pixels>) {
    let file = write_file(dir.path(), "docs/notes.md");
    let mut h = attached(cx, dir, shell_in(dir.path()));
    h.pty("s1", b"wrote docs/notes.md");
    let on = h.cell_center("p1", 9, 0);
    h.click(on, ctrl_shift());
    (h, file, on)
}

/// The text of the open link menu's row `id`.
fn row_label(h: &mut Harness<'_>, id: &'static str) -> Option<&'static str> {
    h.root(move |root, _| root.link_menu_row_label(id))
}

#[gpui::test]
fn ctrl_shift_click_on_a_path_shows_three_entries(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _, _) = notes_menu(cx, &dir);
    for id in ["link-menu-default", "link-menu-viewer", "link-menu-folder"] {
        assert!(h.in_model(id), "{id} is in the menu");
    }
    assert_eq!(
        row_label(&mut h, "link-menu-default"),
        Some("Open in default app")
    );
    assert_eq!(
        row_label(&mut h, "link-menu-viewer"),
        Some("Open in viewer")
    );
    assert_eq!(
        row_label(&mut h, "link-menu-folder"),
        Some("Open containing folder")
    );
}

#[gpui::test]
fn ctrl_shift_click_on_a_path_with_a_line_names_vscode(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    write_file(dir.path(), "src/main.rs");
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", b"error at src/main.rs:12:5 here");
    let on = h.cell_center("p1", 12, 0);
    h.click(on, ctrl_shift());
    assert_eq!(
        row_label(&mut h, "link-menu-default"),
        Some("Open in VS Code")
    );
}

#[gpui::test]
fn the_viewer_entry_opens_the_viewer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, file, _) = notes_menu(cx, &dir);
    h.click_on("link-menu-viewer");
    assert_eq!(viewer_opens(&mut h), [file]);
    assert_eq!(h.opened(), [], "no default app");
}

#[gpui::test]
fn the_folder_entry_reveals_the_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, file, _) = notes_menu(cx, &dir);
    h.click_on("link-menu-folder");
    assert_eq!(h.opened(), [Opened::Reveal(file)]);
}

#[gpui::test]
fn down_then_enter_picks_the_viewer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, file, _) = notes_menu(cx, &dir);
    h.keys("down");
    h.keys("enter");
    assert_eq!(viewer_opens(&mut h), [file]);
    assert_eq!(h.opened(), [], "no default app");
}

#[gpui::test]
fn escape_and_an_outside_click_close_the_link_menu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _, on) = notes_menu(cx, &dir);
    assert!(h.in_model("link-menu"));
    h.keys("escape");
    assert!(!h.in_model("link-menu"), "Esc closes the menu");
    assert_eq!(h.opened(), [], "and opens nothing");

    h.click(on, ctrl_shift());
    assert!(h.in_model("link-menu"));
    let grid = h.bounds("pane-grid-p1");
    let far = point(
        grid.origin.x + grid.size.width - px(10.0),
        grid.origin.y + grid.size.height - px(10.0),
    );
    assert!(
        !h.bounds("link-menu").contains(&far),
        "the click is over the pane, not the menu"
    );
    h.click(far, Modifiers::none());
    assert!(!h.in_model("link-menu"), "and a click outside it");
    assert_eq!(h.opened(), [], "which picked no entry");
    assert_eq!(viewer_opens(&mut h), Vec::<PathBuf>::new());
}

#[gpui::test]
fn the_menu_viewer_row_on_a_text_path_opens_the_viewer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.md");
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", b"wrote docs/notes.md");
    let on = h.cell_center("p1", 9, 0);
    h.click(on, ctrl_shift());
    h.click_on("link-menu-viewer");
    assert_eq!(h.opened(), [], "no default app");
    assert_eq!(viewer_opens(&mut h), [file]);
}

#[gpui::test]
fn the_menu_viewer_row_on_a_bare_name_after_a_full_path_views_its_sibling(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    write_file(dir.path(), "a/b/c.html");
    let sibling = write_file(dir.path(), "a/b/d.html");
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    let full = dir
        .path()
        .join("a/b/c.html")
        .to_string_lossy()
        .replace('\\', "/");
    let line = format!("{full} d.html");
    h.pty("s1", line.as_bytes());
    let on = h.cell_center("p1", full.chars().count() + 2, 0);
    h.click(on, ctrl_shift());
    h.click_on("link-menu-viewer");
    assert_eq!(h.opened(), [], "no default app");
    assert_eq!(viewer_opens(&mut h), [sibling], "{line}");
}

#[gpui::test]
fn the_menu_viewer_row_on_a_binary_opens_the_default_app(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "out/blob.dat");
    std::fs::write(&file, b"PK\0\x03binary").expect("make the file binary");
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", b"wrote out/blob.dat");
    let on = h.cell_center("p1", 9, 0);
    h.click(on, ctrl_shift());
    h.click_on("link-menu-viewer");
    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
    assert_eq!(
        viewer_opens(&mut h),
        Vec::<PathBuf>::new(),
        "the viewer can't show it"
    );
}

#[gpui::test]
fn the_link_tooltip_names_both_gestures(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    write_file(dir.path(), "docs/notes.md");
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", format!("wrote docs/notes.md\r\nsee {URL}").as_bytes());
    let path = h.cell_center("p1", 9, 0);
    h.hover(path, Modifiers::none());
    assert!(!drawn(&mut h, BOTH_GESTURES), "no tip without Ctrl");
    h.hover(path, ctrl());
    assert!(drawn(&mut h, BOTH_GESTURES), "a path names both gestures");

    let url = h.cell_center("p1", 10, 1);
    h.hover(url, ctrl());
    assert!(drawn(&mut h, OPEN_ONLY), "a URL only opens");
    assert!(!drawn(&mut h, BOTH_GESTURES), "a URL has no options");

    h.set_modifiers(Modifiers::none());
    assert!(!drawn(&mut h, OPEN_ONLY), "releasing Ctrl hides the tip");
}

#[gpui::test]
fn a_local_path_link_still_opens_locally(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.md");
    let cwd = dir.path().to_string_lossy().into_owned();
    let mut h = attached(
        cx,
        &dir,
        session("s1")
            .shell(&cwd)
            .members(&[("r1", "main", &cwd)])
            .build(),
    );
    h.pty("s1", b"wrote docs/notes.md");
    let on = h.cell_center("p1", 9, 0);
    h.pick_link(on);
    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
    assert!(
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::FetchFile { .. })),
        "the local daemon's files open in place, never fetched"
    );
}

#[gpui::test]
fn soft_wrapped_path_opens_whole(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    let grid = h.bounds("pane-grid-p1");
    let cell = h.cell_center("p1", 1, 0).x - h.cell_center("p1", 0, 0).x;
    let cols = (grid.size.width / cell).floor();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a small positive column count"
    )]
    let cols = cols as usize;
    let name = format!("{}.txt", "w".repeat(cols + 8));
    let rel = format!("docs/{name}");
    let file = write_file(dir.path(), &rel);
    h.pty("s1", rel.as_bytes());
    let text = h.grid_text("p1");
    assert!(
        text.get(1).is_some_and(|row| !row.is_empty()),
        "the path wraps onto a second row: {text:?}"
    );
    let on_second_row = h.cell_center("p1", 3, 1);
    h.pick_link(on_second_row);
    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
}

#[gpui::test]
fn a_path_wrapped_inside_a_capped_reply_box_opens_whole(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    let grid = h.bounds("pane-grid-p1");
    let cell = h.cell_center("p1", 1, 0).x - h.cell_center("p1", 0, 0).x;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a small positive column count"
    )]
    let cols = (grid.size.width / cell).floor() as usize;
    // ui-themes' reply box: `│` (U+2502) in the first and last columns, 2 pad
    // columns, the text held to a cap 20 columns short of the right border.
    let cap = cols - 20;
    let rel = format!("docs/{}.txt", "w".repeat(cap));
    let file = write_file(dir.path(), &rel);
    let (upper, lower) = rel.split_at(cap);
    h.pty(
        "s1",
        format!("\u{2502}  {upper}\x1b[{cols}G\u{2502}\r\n\u{2502}  {lower}\x1b[{cols}G\u{2502}")
            .as_bytes(),
    );
    let on_lower = h.cell_center("p1", 4, 1);
    h.hover(on_lower, ctrl());
    assert_eq!(h.hovered_link("p1"), Some(rel));
    h.pick_link(on_lower);
    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
}

/// The grid row and column where `needle` first shows in pane `p1`.
fn find_on_grid(h: &mut Harness<'_>, needle: &str) -> (usize, usize) {
    h.grid_text("p1")
        .iter()
        .enumerate()
        .find_map(|(r, text)| text.find(needle).map(|c| (r, text[..c].chars().count())))
        .expect("the text is on screen")
}

#[gpui::test]
fn a_path_with_spaces_underlines_and_opens_whole(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(
        dir.path(),
        "Program Files/Microsoft Visual Studio/notes.txt",
    );
    let shown = file.to_string_lossy().into_owned();
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", format!("at {shown} now").as_bytes());
    let (row, col) = h
        .grid_text("p1")
        .iter()
        .enumerate()
        .find_map(|(r, text)| text.find("Visual").map(|c| (r, text[..c].chars().count())))
        .expect("the path's Visual folder is on screen");
    let on = h.cell_center("p1", col, row);
    h.hover(on, ctrl());
    h.cx.run_until_parked(); // the existence check
    assert_eq!(
        h.hovered_link("p1"),
        Some(shown),
        "the whole spaced path is underlined"
    );
    h.pick_link(on);
    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
}

#[gpui::test]
fn prose_after_a_path_stays_split_when_no_spaced_file_exists(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let first = write_file(dir.path(), "foo.txt");
    let second = write_file(dir.path(), "src/bar.rs");
    let shown = first.to_string_lossy().into_owned();
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", format!("see {shown} and then src/bar.rs").as_bytes());

    let (row, col) = find_on_grid(&mut h, " and then");
    let on_prose = h.cell_center("p1", col + 2, row);
    h.hover(on_prose, ctrl());
    h.cx.run_until_parked(); // the existence check
    assert_eq!(h.hovered_link("p1"), None, "the prose is not underlined");

    let (row, col) = find_on_grid(&mut h, "foo.txt");
    let on_first = h.cell_center("p1", col, row);
    h.hover(on_first, ctrl());
    h.cx.run_until_parked();
    assert_eq!(h.hovered_link("p1"), Some(shown), "the path alone");
    h.pick_link(on_first);

    let (row, col) = find_on_grid(&mut h, "bar.rs");
    let on_second = h.cell_center("p1", col, row);
    h.hover(on_second, ctrl());
    assert_eq!(h.hovered_link("p1").as_deref(), Some("src/bar.rs"));
    h.pick_link(on_second);
    assert_eq!(
        h.opened(),
        [Opened::DefaultApp(first), Opened::DefaultApp(second)]
    );
}

#[gpui::test]
fn quoted_path_with_spaces_widens_when_it_exists(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "my-docs/a b.txt");
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty(
        "s1",
        b"open \"my-docs/a b.txt\" now\r\nopen \"none/a b.txt\" now",
    );

    let (row, col) = find_on_grid(&mut h, "none");
    let on = h.cell_center("p1", col, row);
    h.hover(on, ctrl());
    assert_eq!(
        h.hovered_link("p1").as_deref(),
        Some("none/a"),
        "a quoted run is not taken until its file is found"
    );

    let (row, col) = find_on_grid(&mut h, "b.txt");
    let on = h.cell_center("p1", col, row);
    h.hover(on, ctrl());
    assert_eq!(
        h.hovered_link("p1").as_deref(),
        Some("my-docs/a b.txt"),
        "the whole quoted path is underlined"
    );
    h.pick_link(on);
    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
}

#[gpui::test]
fn hover_on_a_later_path_checks_its_own_readings(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "Program Files/App/a.txt");
    let shown = file.to_string_lossy().into_owned();
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", format!(r"copied C:\temp\a.txt to {shown}").as_bytes());

    let (row, col) = find_on_grid(&mut h, "Files");
    let on = h.cell_center("p1", col, row);
    h.hover(on, ctrl());
    assert_eq!(
        h.hovered_link("p1"),
        Some(shown),
        "the later path's own spaced reading is checked"
    );
    h.pick_link(on);
    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
}

#[gpui::test]
fn widen_survives_new_output_elsewhere_on_screen(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "Program Files/App/notes.txt");
    let shown = file.to_string_lossy().into_owned();
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", format!("at {shown} now").as_bytes());
    let (row, col) = find_on_grid(&mut h, "Files");
    let on = h.cell_center("p1", col, row);
    h.hover(on, ctrl());
    assert_eq!(h.hovered_link("p1"), Some(shown.clone()));

    h.pty("s1", b"\r\n\r\nmore output");
    h.cx.run_until_parked();
    assert_eq!(
        h.hovered_link("p1"),
        Some(shown),
        "the widen holds once other rows change"
    );
    h.pick_link(on);
    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
}

#[gpui::test]
fn widen_holds_on_the_second_row_of_a_wrapped_path(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    let grid = h.bounds("pane-grid-p1");
    let cell = h.cell_center("p1", 1, 0).x - h.cell_center("p1", 0, 0).x;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a small positive column count"
    )]
    let cols = (grid.size.width / cell).floor() as usize;
    let file = write_file(
        dir.path(),
        &format!("Program Files/{}/notes.txt", "w".repeat(cols)),
    );
    let shown = file.to_string_lossy().into_owned();
    h.pty("s1", format!("at {shown} now").as_bytes());
    let (row, col) = find_on_grid(&mut h, "Files");
    let on = h.cell_center("p1", col, row);
    h.hover(on, ctrl());
    assert_eq!(h.hovered_link("p1"), Some(shown));

    // The next row is the wrapped run of the long folder name; the click
    // reads that row's links afresh.
    let below = h.cell_center("p1", 0, row + 1);
    h.pick_link(below);
    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
}

#[gpui::test]
fn ctrl_hover_underlines_only_the_hovered_link(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.pty("s1", b"https://a.example/one and https://b.example/two");
    let second = h.cell_center("p1", 30, 0);
    h.hover(second, ctrl());
    assert_eq!(
        h.hovered_link("p1").as_deref(),
        Some("https://b.example/two")
    );
    let first = h.cell_center("p1", 5, 0);
    h.hover(first, ctrl());
    assert_eq!(
        h.hovered_link("p1").as_deref(),
        Some("https://a.example/one")
    );
    let between = h.cell_center("p1", 23, 0);
    h.hover(between, ctrl());
    assert_eq!(h.hovered_link("p1"), None, "no link under the mouse");
}

#[gpui::test]
fn ctrl_click_on_a_link_wins_over_mouse_reporting(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.pty("s1", format!("\x1b[?1000hsee {URL}").as_bytes());
    let on = h.cell_center("p1", 10, 0);
    h.click(on, Modifiers::none());
    assert!(
        !h.sent_input("s1").is_empty(),
        "a plain click is reported to the child"
    );
    h.click(on, ctrl());
    assert_eq!(h.sent_input("s1"), b"", "no report for the link's click");
    assert_eq!(h.opened(), [Opened::Url(URL.to_owned())]);
}

#[gpui::test]
fn unresolvable_path_shows_a_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", b"open docs/missing.md");
    let on = h.cell_center("p1", 8, 0);
    h.pick_link(on);
    assert_eq!(h.opened(), []);
    let toasts = h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.title.clone(), toast.detail.clone()))
            .collect::<Vec<_>>()
    });
    assert_eq!(
        toasts,
        [(
            "Couldn't open".to_owned(),
            Some("No file found for docs/missing.md".to_owned())
        )]
    );
}

/// A shell in a fresh folder holding `tools/run.bat`, with a Ctrl+click on
/// the link to it made; returns the harness and the file.
fn bat_link_clicked<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> (Harness<'a>, PathBuf) {
    let file = write_file(dir.path(), "tools/run.bat");
    let mut h = attached(cx, dir, shell_in(dir.path()));
    h.pty("s1", b"run tools/run.bat");
    let on = h.cell_center("p1", 8, 0);
    h.pick_link(on);
    (h, file)
}

/// Clicks the run confirm's button `selector`.
fn press(h: &mut Harness<'_>, selector: &str) {
    let at = h.bounds(selector).center();
    h.click(at, Modifiers::none());
}

#[gpui::test]
fn ctrl_click_on_a_bat_asks_before_running(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, file) = bat_link_clicked(cx, &dir);
    assert_eq!(h.opened(), [], "nothing runs before the answer");
    assert_eq!(
        h.run_confirm(),
        Some(("Run run.bat?".to_owned(), file.display().to_string()))
    );
    assert_eq!(
        h.root(|root, _| root.run_confirm_focus()).as_deref(),
        Some("run-confirm-cancel"),
        "Cancel has the focus first"
    );
}

#[gpui::test]
fn the_folder_entry_on_a_bat_does_not_ask(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "tools/run.bat");
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", b"run tools/run.bat");
    let on = h.cell_center("p1", 8, 0);
    h.click(on, ctrl_shift());
    h.click_on("link-menu-folder");
    assert_eq!(h.opened(), [Opened::Reveal(file)]);
    assert_eq!(
        h.run_confirm(),
        None,
        "revealing runs nothing, so nothing asks"
    );
}

#[gpui::test]
fn run_in_the_confirm_opens_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, file) = bat_link_clicked(cx, &dir);
    press(&mut h, "run-confirm-run");
    assert_eq!(h.opened(), [Opened::DefaultApp(file.clone())]);
    assert_eq!(h.run_confirm(), None);

    let on = h.cell_center("p1", 8, 0);
    h.pick_link(on);
    h.keys("tab tab enter");
    assert_eq!(
        h.opened(),
        [Opened::DefaultApp(file.clone()), Opened::DefaultApp(file)],
        "Tab twice reaches Run, and Enter presses it"
    );
}

#[gpui::test]
fn show_in_explorer_reveals_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, file) = bat_link_clicked(cx, &dir);
    h.keys("tab space");
    assert_eq!(h.opened(), [Opened::Reveal(file.clone())]);
    assert_eq!(h.run_confirm(), None);

    let on = h.cell_center("p1", 8, 0);
    h.pick_link(on);
    press(&mut h, "run-confirm-reveal");
    assert_eq!(
        h.opened(),
        [Opened::Reveal(file.clone()), Opened::Reveal(file)]
    );
}

#[gpui::test]
fn cancel_opens_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = bat_link_clicked(cx, &dir);
    h.keys("escape");
    assert_eq!(h.run_confirm(), None, "Esc closes the confirm");

    let on = h.cell_center("p1", 8, 0);
    h.pick_link(on);
    assert!(h.run_confirm().is_some());
    press(&mut h, "run-confirm-cancel");
    assert_eq!(h.run_confirm(), None);
    assert_eq!(h.opened(), []);
}

#[gpui::test]
fn run_confirm_escape_returns_focus_to_the_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = bat_link_clicked(cx, &dir);
    h.keys("escape");
    assert_eq!(h.run_confirm(), None, "Esc closes the confirm");
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the pane has the keyboard");
}

#[gpui::test]
fn boxed_link_border_is_not_clickable(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), &format!("docs/{}.md", "a".repeat(16)));
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty(
        "s1",
        "│ docs/aaaaaaaaaaaa │\r\n│ aaaa.md           │".as_bytes(),
    );
    for (col, row, what) in [
        (20, 0, "the right border"),
        (19, 0, "the right padding"),
        (0, 1, "the left border below"),
        (1, 1, "the left padding below"),
        (12, 1, "the padding past the tail"),
    ] {
        let at = h.cell_center("p1", col, row);
        h.hover(at, ctrl());
        assert_eq!(h.hovered_link("p1"), None, "{what} is not underlined");
        h.click(at, ctrl());
        assert_eq!(h.opened(), [], "{what} opens nothing");
    }
    let toasts = h.root(|root, _| root.toasts().len());
    assert_eq!(toasts, 0, "no open was tried");

    let on_tail = h.cell_center("p1", 3, 1);
    h.hover(on_tail, ctrl());
    assert_eq!(
        h.hovered_link("p1"),
        Some(format!("docs/{}.md", "a".repeat(16)))
    );
    h.pick_link(on_tail);
    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
}

#[gpui::test]
fn releasing_ctrl_removes_the_underline(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    h.pty("s1", format!("see {URL}").as_bytes());
    let on = h.cell_center("p1", 10, 0);
    h.hover(on, ctrl());
    assert_eq!(h.hovered_link("p1").as_deref(), Some(URL));
    h.set_modifiers(Modifiers::none());
    assert_eq!(h.hovered_link("p1"), None);
    h.set_modifiers(ctrl());
    assert_eq!(
        h.hovered_link("p1").as_deref(),
        Some(URL),
        "pressing Ctrl again over a still mouse brings it back"
    );
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

/// Every toast's kind, oldest first.
fn toast_kinds(h: &mut Harness<'_>) -> Vec<ToastKind> {
    h.root(|root, _| root.toasts().iter().map(|toast| toast.kind).collect())
}

#[gpui::test]
fn a_second_exec_link_while_the_confirm_is_open_is_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, first) = bat_link_clicked(cx, &dir);
    let asked = Some(("Run run.bat?".to_owned(), first.display().to_string()));
    assert_eq!(h.run_confirm(), asked);
    // The backdrop takes every click, so a second request only comes from
    // a background check that finishes while the confirm is up.
    let second = write_file(dir.path(), "tools/two.cmd");
    h.root_update(|root, window, cx| root.ask_to_run(second, window, cx));
    assert_eq!(h.run_confirm(), asked, "the open confirm stays as it was");
    assert_eq!(h.opened(), [], "nothing runs");
    assert_eq!(
        toasts(&mut h),
        [(
            "Couldn't open".to_owned(),
            Some("Another dialog is open.".to_owned())
        )]
    );
    h.keys("escape");
    assert_eq!(h.run_confirm(), None, "no second confirm waits beneath");
    assert_eq!(h.opened(), []);
}

/// `native-ui.json` in the spec's ui dir as it stands, or an empty object.
fn ui_file(dir: &TestDir) -> Value {
    std::fs::read_to_string(dir.path().join("native-ui.json")).map_or_else(
        |_| json!({}),
        |text| serde_json::from_str(&text).expect("native-ui.json is JSON"),
    )
}

/// Edits `native-ui.json` by hand: lists `hosts` under `unc_hosts`, or
/// drops the key for `None`.
fn hand_edit_unc_hosts(dir: &TestDir, hosts: Option<&[&str]>) {
    let mut file = ui_file(dir);
    let object = file.as_object_mut().expect("native-ui.json is an object");
    match hosts {
        Some(hosts) => object.insert("unc_hosts".to_owned(), json!(hosts)),
        None => object.remove("unc_hosts"),
    };
    std::fs::write(
        dir.path().join("native-ui.json"),
        serde_json::to_vec_pretty(&file).expect("serialize native-ui.json"),
    )
    .expect("write native-ui.json");
}

#[gpui::test]
fn an_edit_to_unc_hosts_applies_on_the_next_click(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", br"see \\nas\share\notes.md");
    let on = h.cell_center("p1", 8, 0);
    let refused = (
        "Not opening a network path on nas".to_owned(),
        Some("Add it to unc_hosts in native-ui.json to allow it.".to_owned()),
    );

    h.pick_link(on);
    assert_eq!(h.opened(), []);
    assert_eq!(toasts(&mut h), std::slice::from_ref(&refused));

    hand_edit_unc_hosts(&dir, Some(&["NAS"]));
    h.pick_link(on);
    let opened = Opened::DefaultApp(PathBuf::from(r"\\nas\share\notes.md"));
    assert_eq!(
        h.opened(),
        std::slice::from_ref(&opened),
        "listed now, so it opens"
    );

    hand_edit_unc_hosts(&dir, None);
    h.pick_link(on);
    assert_eq!(h.opened(), [opened], "unlisted again, so it does not");
    assert_eq!(
        toasts(&mut h),
        std::slice::from_ref(&refused),
        "the second refusal of the same host updates the toast that is up"
    );
    assert_eq!(toast_kinds(&mut h), [ToastKind::Warning]);
}

/// A tab `t1` with `p1` on `s1` and `p2` on `s2`, both shells in `dir`'s
/// folder, their scrollback answered and `p1` holding the keyboard.
fn two_shell_panes<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let cwd = dir.path().to_string_lossy().into_owned();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s2")),
    );
    let fixture = Fixture {
        sessions: vec![
            session("s1").shell(&cwd).build(),
            session("s2").shell(&cwd).build(),
        ],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.answer_scrollback("s2", b"");
    h.cx.update(|window, _| window.activate_window());
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h
}

/// The pane its tab has focused.
fn focused_pane(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.focused_pane())
}

#[gpui::test]
fn ctrl_shift_click_in_another_pane_moves_the_keyboard_there(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    write_file(dir.path(), "docs/notes.md");
    let mut h = two_shell_panes(cx, &dir);
    assert_eq!(
        focused_pane(&mut h).as_deref(),
        Some("p1"),
        "p1 starts with the keyboard"
    );
    h.pty("s2", b"wrote docs/notes.md");
    let on = h.cell_center("p2", 9, 0);
    h.click(on, ctrl_shift());
    assert!(
        h.in_model("link-menu"),
        "the Ctrl+Shift+click opens the menu"
    );
    h.keys("escape");
    assert!(!h.in_model("link-menu"), "Esc closes the menu");
    assert_eq!(
        focused_pane(&mut h).as_deref(),
        Some("p2"),
        "the keyboard stays on the pane the click was in"
    );
}

/// [`two_shell_panes`] with `p2` popped out into a window of its own.
fn pane_popped<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = two_shell_panes(cx, dir);
    let key = PopoutKey::Pane {
        tab_id: "t1".to_owned(),
        pane_id: "p2".to_owned(),
    };
    assert_eq!(
        h.root_update(move |root, window, cx| root.open_popout(key, window, cx)),
        OpenOutcome::Opened
    );
    h
}

#[gpui::test]
fn link_menu_keys_act_only_in_its_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    write_file(dir.path(), "docs/notes.md");
    let mut h = pane_popped(cx, &dir);
    h.pty("s1", b"wrote docs/notes.md");
    h.pty("s2", b"wrote docs/notes.md");
    h.cx.run_until_parked();

    // The main window opens a menu of its own, which holds its keyboard.
    let in_main = h.cell_center("p1", 9, 0);
    h.click(in_main, ctrl_shift());
    assert!(h.in_model("link-menu"), "the main window's menu is open");

    // The pop-out then opens its menu, which the overlay owner moves to.
    let root = h.root.clone();
    let mut popout = h.popout_window();
    let at = popout
        .update(|_, cx| root.read(cx).pane_cell_center("p2", 9, 0, cx))
        .expect("p2 is laid out in the pop-out");
    popout.simulate_click(at, ctrl_shift());

    // Up and Enter in the main window, which draws no menu any more, drive none.
    h.keys("down enter");
    assert!(
        h.root(|root, _| root.link_menu_open()),
        "the pop-out's menu is open and stays open"
    );
    assert_eq!(h.opened(), [], "no entry was picked");
    assert_eq!(viewer_opens(&mut h), Vec::<PathBuf>::new(), "no viewer tab");
}

#[gpui::test]
fn ctrl_shift_click_in_the_main_window_opens_the_menu_there_beside_a_popout(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    write_file(dir.path(), "docs/notes.md");
    let mut h = pane_popped(cx, &dir);
    h.pty("s1", b"wrote docs/notes.md");
    // The pop-out draws last, as it does after its own terminal's output.
    h.pty("s2", b"wrote docs/notes.md");
    h.cx.run_until_parked();

    let in_main = h.cell_center("p1", 9, 0);
    h.click(in_main, ctrl_shift());
    assert!(
        h.in_model("link-menu"),
        "the Ctrl+Shift+click opens the menu"
    );
    assert!(
        drawn(&mut h, "link-menu"),
        "the main window, where the click was, draws it"
    );
    let mut popout = h.popout_window();
    popout.run_until_parked();
    assert!(
        popout.debug_bounds("link-menu").is_none(),
        "the pop-out draws no copy of it"
    );
}

#[gpui::test]
fn an_open_from_a_popout_finishes_in_the_main_window_after_the_popout_closes(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "tools/run.bat");
    let mut h = pane_popped(cx, &dir);
    h.pty("s2", b"run tools/run.bat");
    h.cx.run_until_parked();

    let root = h.root.clone();
    let mut popout = h.popout_window();
    let at = popout
        .update(|_, cx| root.read(cx).pane_cell_center("p2", 8, 0, cx))
        .expect("p2 is laid out in the pop-out");

    // The pop-out closes in the same breath as the click it takes, while the
    // open's background work is still out: this task was queued first, and
    // the follow-up's task waits behind it in the same queue.
    let main = h.cx.update(|window, _| window.window_handle());
    let popout_handle =
        h.cx.windows()
            .into_iter()
            .find(|window| *window != main)
            .expect("a pop-out window is open");
    h.cx.spawn(|mut app: gpui::AsyncApp| async move {
        app.update_window(popout_handle, |_, window, _| window.remove_window())
            .ok();
    })
    .detach();

    popout.simulate_mouse_down(at, MouseButton::Left, ctrl());

    assert_eq!(h.window_count(), 1, "the pop-out is gone");
    assert_eq!(
        h.run_confirm(),
        Some(("Run run.bat?".to_owned(), file.display().to_string())),
        "the click's run confirm opens in the main window"
    );
}

#[gpui::test]
fn a_ui_save_keeps_hand_added_unc_hosts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, session("s1").build());
    let panel = h.bounds("sidebar-panel");
    h.click(
        point(panel.center().x, panel.bottom() - px(10.0)),
        Modifiers::none(),
    );
    h.keys("ctrl-b");
    assert!(h.root(|root, _| root.sidebar_collapsed()), "hidden");
    let saved = ui_file(&dir);
    assert_eq!(saved["sidebar_collapsed"], json!(true), "{saved}");
    assert!(
        saved.get("unc_hosts").is_none(),
        "the client writes no unc_hosts of its own: {saved}"
    );

    hand_edit_unc_hosts(&dir, Some(&["nas", "files"]));
    h.click_on("activity-sessions");
    let saved = ui_file(&dir);
    assert_eq!(saved["sidebar_collapsed"], json!(false), "{saved}");
    assert_eq!(saved["unc_hosts"], json!(["nas", "files"]), "{saved}");
}
