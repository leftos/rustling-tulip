//! Terminal link specs: Ctrl+hover underlines the link under the mouse, and
//! Ctrl+click opens it — a URL in the browser, a path with a line in VS
//! Code, a plain path in its default app — through a recording opener.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::path::{Path, PathBuf};

use gpui::{Modifiers, TestAppContext, point, px};
use protocol::SessionSnapshot;
use rustling_tulip_native::ToastKind;
use serde_json::{Value, json};
use support::{Fixture, Harness, Opened, TestDir, session};

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
    Modifiers::secondary_key()
}

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
fn ctrl_click_on_a_plain_path_opens_the_default_app(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let file = write_file(dir.path(), "docs/notes.md");
    let mut h = attached(cx, &dir, shell_in(dir.path()));
    h.pty("s1", b"wrote docs/notes.md");
    let on = h.cell_center("p1", 9, 0);
    h.click(on, ctrl());
    assert_eq!(h.opened(), [Opened::DefaultApp(file)]);
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
    h.click(on_second_row, ctrl());
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
    h.click(on, ctrl());
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
    h.click(on, ctrl());
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
fn run_in_the_confirm_opens_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, file) = bat_link_clicked(cx, &dir);
    press(&mut h, "run-confirm-run");
    assert_eq!(h.opened(), [Opened::DefaultApp(file.clone())]);
    assert_eq!(h.run_confirm(), None);

    let on = h.cell_center("p1", 8, 0);
    h.click(on, ctrl());
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
    h.click(on, ctrl());
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
    h.click(on, ctrl());
    assert!(h.run_confirm().is_some());
    press(&mut h, "run-confirm-cancel");
    assert_eq!(h.run_confirm(), None);
    assert_eq!(h.opened(), []);
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
    h.click(on_tail, ctrl());
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

    h.click(on, ctrl());
    assert_eq!(h.opened(), []);
    assert_eq!(toasts(&mut h), std::slice::from_ref(&refused));

    hand_edit_unc_hosts(&dir, Some(&["NAS"]));
    h.click(on, ctrl());
    let opened = Opened::DefaultApp(PathBuf::from(r"\\nas\share\notes.md"));
    assert_eq!(
        h.opened(),
        std::slice::from_ref(&opened),
        "listed now, so it opens"
    );

    hand_edit_unc_hosts(&dir, None);
    h.click(on, ctrl());
    assert_eq!(h.opened(), [opened], "unlisted again, so it does not");
    assert_eq!(
        toasts(&mut h),
        std::slice::from_ref(&refused),
        "the second refusal of the same host updates the toast that is up"
    );
    assert_eq!(toast_kinds(&mut h), [ToastKind::Warning]);
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
