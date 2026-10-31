//! Themes modal specs: the Theme row of Settings › Appearance opens it on the
//! theme in use, the keyboard and the pointer move its preview, and the
//! window takes a theme only on Use.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{Modifiers, TestAppContext};
use protocol::{DaemonMessage, WorktreeCleanupFailure};
use rustling_tulip_native::builtin_themes::{DRACULA, NORD};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::theme_store::{
    MAX_THEME_BYTES, THEMES_DIR, ThemeStore, export_json, parse_theme_json,
};
use rustling_tulip_native::ui_theme::{self, UiTheme};
use rustling_tulip_native::{Connection, FileFilter};
use support::{Fixture, Harness, SaveAsk, TestDir, repo, session};

const HOST: &str = "box.tail1234.ts.net";

fn fixture() -> Fixture {
    Fixture {
        repos: vec![repo("r1", "C:/repos/r1")],
        ..Fixture::single(session("s1").in_repo("r1").build())
    }
}

/// The theme `native-ui.json` names, when it names one.
fn saved_theme(dir: &TestDir) -> Option<String> {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).ok()?;
    let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    saved["theme"].as_str().map(str::to_owned)
}

fn settings_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).settings_focus(window, cx))
}

fn themes_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    h.root(|root, _| root.themes_modal_focus())
}

/// Whether the last frame drew what `selector` tags.
fn shown(h: &mut Harness<'_>, selector: &'static str) -> bool {
    h.cx.run_until_parked();
    h.cx.debug_bounds(selector).is_some()
}

fn open_dialog(h: &mut Harness<'_>) {
    h.keys("ctrl-,");
    h.click_on("settings-tab-appearance");
    h.click_on("appearance-theme");
    assert!(
        h.root(|root, _| root.themes_modal_open()),
        "the dialog opens"
    );
}

/// The window on the saved theme with the dialog open.
fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with_saved_theme(cx, dir, &fixture());
    open_dialog(&mut h);
    h
}

/// What the dialog's keyboard, pointer and preview stand on, as row indices.
fn cursor(h: &mut Harness<'_>) -> usize {
    h.root(|root, _| root.themes_modal().expect("the dialog is open").cursor)
}

fn preview(h: &mut Harness<'_>) -> usize {
    h.root(|root, _| root.themes_modal().expect("the dialog is open").preview())
}

/// The id of the row at `index`.
fn row_id(h: &mut Harness<'_>, index: usize) -> String {
    h.root(move |root, _| {
        root.themes_modal().expect("the dialog is open").rows[index]
            .entry
            .id
            .clone()
    })
}

/// Presses Down until the first row of `id` that has `built_in` is under the
/// cursor.
fn cursor_to(h: &mut Harness<'_>, id: &str, built_in: bool) {
    let id = id.to_owned();
    let at = h
        .root(move |root, _| {
            root.themes_modal()
                .expect("the dialog is open")
                .rows
                .iter()
                .position(|row| row.entry.id == id && row.entry.built_in == built_in)
        })
        .expect("the row is listed");
    let from = cursor(h);
    let key = if at > from { "down" } else { "up" };
    for _ in 0..at.abs_diff(from) {
        h.keys(key);
    }
    assert_eq!(cursor(h), at);
}

fn drawn(h: &mut Harness<'_>) -> UiTheme {
    h.root(|_, cx| ui_theme::theme(cx))
}

fn assert_returned_to_settings(h: &mut Harness<'_>) {
    assert!(
        !h.root(|root, _| root.themes_modal_open()),
        "the dialog closed"
    );
    assert!(
        h.root(|root, _| root.settings_open()),
        "Settings stays open"
    );
    assert_eq!(
        settings_focus(h),
        Some("appearance-theme"),
        "the keyboard is back on the Theme stop"
    );
}

#[gpui::test]
fn the_settings_row_opens_on_the_theme_in_use(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        serde_json::json!({ "theme": "dracula" }).to_string(),
    )
    .expect("write native-ui.json");
    let mut h = Harness::with_saved_theme(cx, &dir, &fixture());
    h.keys("ctrl-,");
    h.click_on("settings-tab-appearance");
    assert_eq!(
        h.root(|root, _| root.appearance_theme_name()).as_deref(),
        Some("Dracula"),
        "the row names the theme in use"
    );

    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("appearance-theme"),
        "the Theme stop is first in the ring"
    );
    h.keys("enter");

    assert_eq!(themes_focus(&mut h), Some("themes-list"));
    let at = h.root(|root, _| root.themes_modal().expect("open").in_use);
    assert_eq!(row_id(&mut h, at), "dracula");
    assert_eq!(cursor(&mut h), at, "the keyboard starts on it");
    assert!(shown(&mut h, "themes-in-use"), "its row carries the mark");
    assert!(shown(&mut h, "themes-miniature"), "the preview draws");
}

#[gpui::test]
fn colours_a_released_build_saved_open_as_my_colours_in_use(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        serde_json::json!({
            "sidebar_width": 300.0,
            "app_appearance": {
                "accent_color": "#38bdf8",
                "terminal_background_color": "#f6f4ef",
                "terminal_frame_color": null
            },
            "recent_colors": ["#38bdf8"]
        })
        .to_string(),
    )
    .expect("write native-ui.json");
    let mut h = Harness::with_saved_theme(cx, &dir, &fixture());
    assert_eq!(drawn(&mut h).accent, 0x38_bdf8, "the window's accent");
    assert_eq!(
        h.root(|root, _| root.theme_base()).background,
        0xf6_f4ef,
        "the terminal's background"
    );
    assert_eq!(
        h.root(|root, _| root.session_accent("s1")),
        Some(0x38_bdf8),
        "a session with no colour of its own takes the theme's"
    );

    open_dialog(&mut h);
    let (name, id) = h.root(|root, _| {
        let dialog = root.themes_modal().expect("open");
        let entry = &dialog.rows[dialog.in_use].entry;
        (entry.name.clone(), entry.id.clone())
    });
    assert_eq!(name, "My colours", "the migrated theme is the one in use");
    assert_eq!(saved_theme(&dir).as_deref(), Some(id.as_str()));
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("read");
    let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert!(saved.get("app_appearance").is_none(), "{saved}");
    assert_eq!(saved["recent_colors"][0], "#38bdf8", "the rest is kept");
}

#[gpui::test]
fn arrow_keys_move_the_preview(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let start = cursor(&mut h);

    h.keys("down");
    assert_eq!(preview(&mut h), start + 1);
    assert_eq!(drawn(&mut h), NORD, "the window keeps its theme");
    h.keys("up");
    assert_eq!(preview(&mut h), start);
}

#[gpui::test]
fn hovering_a_row_previews_it_and_leaving_returns_to_the_cursor(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let start = cursor(&mut h);

    let row = h.center("themes-row-builtin-contrast");
    h.hover(row, Modifiers::none());
    assert_eq!(preview(&mut h), 0, "the row under the pointer");
    assert_eq!(cursor(&mut h), start, "the keyboard stays");
    assert_eq!(drawn(&mut h), NORD, "the window keeps its theme");

    let off = h.center("themes-details");
    h.hover(off, Modifiers::none());
    assert_eq!(preview(&mut h), start, "back to the keyboard's row");
}

#[gpui::test]
fn a_key_press_overrides_the_hover(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let start = cursor(&mut h);

    let row = h.center("themes-row-builtin-contrast");
    h.hover(row, Modifiers::none());
    h.keys("down");

    assert_eq!(preview(&mut h), start + 1);
    assert_eq!(
        h.root(|root, _| root.themes_modal().expect("open").hover),
        None
    );
}

#[gpui::test]
fn use_applies_saves_and_closes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    cursor_to(&mut h, "dracula", true);

    h.click_on("themes-use");

    assert_eq!(drawn(&mut h), DRACULA);
    assert_eq!(saved_theme(&dir).as_deref(), Some("dracula"));
    assert_returned_to_settings(&mut h);
    assert_eq!(
        h.root(|root, _| root.appearance_theme_name()).as_deref(),
        Some("Dracula"),
        "the row follows"
    );
}

#[gpui::test]
fn enter_uses_the_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    cursor_to(&mut h, "dracula", true);

    h.keys("enter");

    assert_eq!(drawn(&mut h), DRACULA);
    assert_eq!(saved_theme(&dir).as_deref(), Some("dracula"));
    assert_returned_to_settings(&mut h);
}

#[gpui::test]
fn esc_changes_nothing_and_returns_to_the_settings_stop(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    cursor_to(&mut h, "dracula", true);

    h.keys("escape");

    assert_eq!(drawn(&mut h), NORD);
    assert_eq!(saved_theme(&dir), None);
    assert_returned_to_settings(&mut h);
}

#[gpui::test]
fn cancel_changes_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    cursor_to(&mut h, "dracula", true);

    h.click_on("themes-cancel");

    assert_eq!(drawn(&mut h), NORD);
    assert_eq!(saved_theme(&dir), None);
    assert_returned_to_settings(&mut h);
}

#[gpui::test]
fn a_broken_row_shows_its_error_and_disables_use(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let themes = dir.path().join(THEMES_DIR);
    std::fs::create_dir_all(&themes).expect("make the themes folder");
    std::fs::write(themes.join("broken.json"), "{").expect("write the broken file");
    let mut h = opened(cx, &dir);

    cursor_to(&mut h, "broken", false);

    assert!(shown(&mut h, "themes-row-error"), "the row shows the error");
    assert!(shown(&mut h, "themes-preview-error"), "so does the preview");
    assert!(
        h.root(|root, _| {
            let dialog = root.themes_modal().expect("open");
            dialog.rows[dialog.preview()].theme.is_none()
        }),
        "in place of the miniature"
    );
    assert!(
        !h.root(|root, _| root.themes_modal().expect("open").can_use()),
        "Use is disabled"
    );

    h.keys("enter");
    h.click_on("themes-use");
    assert!(
        h.root(|root, _| root.themes_modal_open()),
        "nothing is used"
    );
    assert_eq!(drawn(&mut h), NORD);

    h.keys("tab");
    assert_eq!(themes_focus(&mut h), Some("themes-cancel"));
    h.keys("tab");
    assert_eq!(
        themes_focus(&mut h),
        Some("themes-list"),
        "a disabled Use is out of the ring"
    );
}

#[gpui::test]
fn a_shadow_file_is_listed_beside_the_built_in(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let store = ThemeStore::new(dir.path().join(THEMES_DIR));
    let id = store
        .save("Mine", &UiTheme::CONTRAST, None, None)
        .expect("save");
    let themes = dir.path().join(THEMES_DIR);
    std::fs::copy(themes.join(format!("{id}.json")), themes.join("nord.json")).expect("copy");
    let mut h = opened(cx, &dir);

    assert!(shown(&mut h, "themes-row-builtin-nord"));
    assert!(
        shown(&mut h, "themes-row-user-nord"),
        "its element id differs"
    );
    assert!(shown(&mut h, "themes-yours"), "under the Yours heading");
    let in_use = h.root(|root, _| {
        let dialog = root.themes_modal().expect("open");
        dialog.rows[dialog.in_use].entry.built_in
    });
    assert!(in_use, "the built-in is the row in use");

    cursor_to(&mut h, "nord", false);
    assert!(shown(&mut h, "themes-preview-error"));
}

#[gpui::test]
fn the_contrast_summary_names_failing_pairs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut low = UiTheme::CONTRAST;
    low.text = low.ground;
    let id = ThemeStore::new(dir.path().join(THEMES_DIR))
        .save("Low", &low, None, Some("dracula"))
        .expect("save");
    let mut h = opened(cx, &dir);

    assert_eq!(
        h.root(|root, _| root.themes_modal_contrast()),
        ["All pairs pass AA"],
        "a built-in passes"
    );
    assert_eq!(
        h.root(|root, _| root.themes_modal_terminal_source())
            .as_deref(),
        Some("Terminal: Own palette")
    );

    cursor_to(&mut h, &id, false);
    let lines = h.root(|root, _| root.themes_modal_contrast());
    let failing = h.root(|root, _| {
        let dialog = root.themes_modal().expect("open");
        dialog.rows[dialog.cursor].failing.len()
    });
    assert_eq!(lines.len(), failing + 1, "{lines:?}");
    assert_eq!(lines[0], format!("{failing} pairs below AA"));
    assert!(
        lines
            .iter()
            .any(|line| line == "text on ground: 1.0:1, needs 4.5:1"),
        "{lines:?}"
    );
    assert_eq!(
        h.root(|root, _| root.themes_modal_terminal_source())
            .as_deref(),
        Some("Terminal: Dracula")
    );
}

#[gpui::test]
fn the_theme_row_works_on_a_remote_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::on_target(cx, &dir, Target::tailnet(HOST, None), &fixture());
    open_dialog(&mut h);
    cursor_to(&mut h, "dracula", true);

    h.keys("enter");

    assert_eq!(drawn(&mut h), DRACULA, "the theme is this PC's to choose");
    assert_eq!(saved_theme(&dir).as_deref(), Some("dracula"));
}

#[gpui::test]
fn a_daemon_switch_closes_the_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    h.set_connection(Connection::for_target(Target::tailnet(HOST, None)));

    assert!(!h.root(|root, _| root.themes_modal_open()));
    assert!(!h.root(|root, _| root.settings_open()), "Settings closed");
    assert_eq!(drawn(&mut h), NORD);

    h.load(&fixture());
    h.answer_scrollback("s1", b"");
    h.sent();
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the pane holds the keyboard");
}

#[gpui::test]
fn disabled_controls_are_out_of_the_ring(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    for control in ["themes-duplicate", "themes-export", "themes-import"] {
        assert!(shown(&mut h, control), "{control} is drawn");
    }
    for expected in ["themes-cancel", "themes-use", "themes-list"] {
        h.keys("tab");
        assert_eq!(themes_focus(&mut h), Some(expected));
    }
    h.keys("shift-tab");
    assert_eq!(themes_focus(&mut h), Some("themes-use"), "and back");

    for control in ["themes-export", "themes-import"] {
        h.click_on(control);
        assert!(
            h.root(|root, _| root.themes_modal_open()),
            "{control} is inert"
        );
    }
}

#[gpui::test]
fn a_window_close_with_the_modal_open_leaves_no_orphan(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.answer_scrollback("s1", b"");

    assert!(!h.cx.simulate_close(), "the exit dialog asks first");
    h.cx.run_until_parked();
    assert!(h.root(|root, _| root.exit_dialog_open()));
    h.keys("escape");

    assert!(!h.root(|root, _| root.exit_dialog_open()));
    assert!(!h.root(|root, _| root.themes_modal_open()), "no orphan");
    assert!(!h.root(|root, _| root.settings_open()));
    h.sent();
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the pane has the keyboard");
}

#[gpui::test]
fn a_notice_dismissed_over_the_modal_returns_the_keyboard_to_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(DaemonMessage::WorktreeCleanupFailed {
        session_id: "s1".to_owned(),
        session_label: "feat/x".to_owned(),
        failures: vec![WorktreeCleanupFailure {
            member_path: "C:/wts/wt.feat-x/a".to_owned(),
            repo_path: "C:/src/a".to_owned(),
            reason: "fs::remove_dir_all: access denied".to_owned(),
            locking_processes: Vec::new(),
        }],
    });
    assert!(h.root(|root, _| root.cleanup_failed_open()));

    h.keys("escape");

    assert!(!h.root(|root, _| root.cleanup_failed_open()));
    assert!(h.root(|root, _| root.themes_modal_open()));
    let root = h.root.clone();
    assert!(
        h.cx.update(|window, cx| root.read(cx).themes_modal_has_keyboard(window)),
        "the keyboard is back on the modal"
    );
}

#[gpui::test]
fn space_on_the_list_changes_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    cursor_to(&mut h, "dracula", true);

    h.keys("space");

    assert!(h.root(|root, _| root.themes_modal_open()));
    assert_eq!(drawn(&mut h), NORD);
    assert_eq!(saved_theme(&dir), None);

    h.keys("tab space");
    assert!(
        !h.root(|root, _| root.themes_modal_open()),
        "Space still presses Cancel"
    );
}

#[gpui::test]
fn ctrl_enter_on_the_list_changes_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    cursor_to(&mut h, "dracula", true);

    h.keys("ctrl-enter");

    assert!(h.root(|root, _| root.themes_modal_open()));
    assert_eq!(drawn(&mut h), NORD);
}

#[gpui::test]
fn a_held_enter_does_not_use_the_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    cursor_to(&mut h, "dracula", true);

    h.held("enter");

    assert!(h.root(|root, _| root.themes_modal_open()));
    assert_eq!(drawn(&mut h), NORD);
}

#[gpui::test]
fn use_on_the_row_in_use_saves_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(dir.path().join("native-ui.json"), "{}").expect("write native-ui.json");
    let mut h = opened(cx, &dir);

    h.keys("enter");

    assert_returned_to_settings(&mut h);
    assert_eq!(saved_theme(&dir), None, "no selection stays no selection");
}

#[gpui::test]
fn the_close_button_returns_to_the_settings_stop(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    h.click_on("themes-close");

    assert_returned_to_settings(&mut h);
}

/// The theme the clipboard holds: its name, theme and base.
fn copied_theme(h: &mut Harness<'_>) -> (String, UiTheme, Option<String>) {
    let text = h.clipboard().expect("the clipboard holds the theme");
    let (name, theme, base, _) = parse_theme_json(&text).expect("a whole theme");
    (name, theme, base)
}

/// The names of the files in `dir`, but `native-ui.json`, which the window
/// saves on its own.
fn files_in(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read the folder")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name != "native-ui.json")
        .collect();
    names.sort();
    names
}

fn theme_filter() -> FileFilter {
    FileFilter::new("Theme", &["json"])
}

#[gpui::test]
fn export_copies_the_theme_then_saves_it_where_asked(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let out = dir.path().join("exported.json");
    let mut h = opened(cx, &dir);
    cursor_to(&mut h, "dracula", true);
    h.set_saved_file(Some(&out));

    h.click_on("themes-export");
    h.cx.run_until_parked();

    let (name, theme, _) = copied_theme(&mut h);
    assert_eq!((name.as_str(), theme), ("Dracula", DRACULA));
    assert!(
        h.root(|root, _| root.copied_chip()).is_some(),
        "the Copied chip shows"
    );
    assert_eq!(
        h.save_asks(),
        [SaveAsk {
            dir: None,
            suggested: "dracula.json".to_owned(),
            filter: theme_filter(),
        }]
    );
    let written = std::fs::read_to_string(&out).expect("the file is written");
    assert_eq!(
        Some(written),
        h.clipboard(),
        "the file holds the copied JSON"
    );
    assert_eq!(h.root(|root, _| root.themes_modal_error()), None);
    assert_eq!(drawn(&mut h), NORD, "nothing is applied");
}

#[gpui::test]
fn cancelling_the_save_keeps_the_clipboard_and_writes_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let before = files_in(dir.path());
    h.set_saved_file(None);

    h.click_on("themes-export");
    h.cx.run_until_parked();

    assert_eq!(h.save_asks().len(), 1, "the save picker was asked");
    assert_eq!(
        copied_theme(&mut h).0,
        "Nord",
        "the clipboard keeps the theme"
    );
    assert_eq!(files_in(dir.path()), before, "nothing is written");
    assert_eq!(h.root(|root, _| root.themes_modal_error()), None);
}

#[gpui::test]
fn export_in_the_editor_takes_the_working_theme_under_the_typed_name(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let out = dir.path().join("mine.json");
    let mut h = opened(cx, &dir);
    h.click_on("themes-duplicate");
    h.click_on("themes-name");
    h.keys("ctrl-a m i n e");
    h.set_saved_file(Some(&out));

    h.click_on("themes-export");
    h.cx.run_until_parked();

    let working = h.root(|root, _| root.themes_editor().expect("the editor is open").working);
    let (name, theme, base) = copied_theme(&mut h);
    assert_eq!(name, "mine");
    assert_eq!(theme, working);
    assert_eq!(
        base.as_deref(),
        Some("nord"),
        "with the theme it was made from"
    );
    assert_eq!(h.save_asks()[0].suggested, "mine.json");
    let written = std::fs::read_to_string(&out).expect("the file is written");
    assert_eq!(Some(written), h.clipboard());
    assert!(
        !dir.path().join(THEMES_DIR).exists(),
        "an export saves no theme"
    );
}

/// Writes `text` to `name` in `dir` and makes the file picker answer it.
fn pick_theme_file(h: &mut Harness<'_>, dir: &TestDir, name: &str, text: &[u8]) {
    let path = dir.path().join(name);
    std::fs::write(&path, text).expect("write the theme file");
    h.set_picked_file(Some(path.to_str().expect("a UTF-8 path")));
}

/// The keyboard's row: its name, and whether it sits under Yours.
fn cursor_row(h: &mut Harness<'_>) -> (String, bool) {
    h.root(|root, _| {
        let dialog = root.themes_modal().expect("the dialog is open");
        let row = &dialog.rows[dialog.cursor];
        let yours = !row.entry.built_in && dialog.cursor >= dialog.yours_start();
        (row.entry.name.clone(), yours)
    })
}

fn modal_error(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.themes_modal_error())
}

#[gpui::test]
fn import_adds_the_theme_under_yours_with_the_cursor_on_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let text = export_json("Night", &DRACULA, Some("dracula"), None).expect("export");
    pick_theme_file(&mut h, &dir, "night-export.json", text.as_bytes());

    h.click_on("themes-import");
    h.cx.run_until_parked();

    assert_eq!(h.file_filters(), [theme_filter()]);
    assert_eq!(cursor_row(&mut h), ("Night".to_owned(), true));
    assert!(
        dir.path().join(THEMES_DIR).join("night.json").exists(),
        "the theme has its own file"
    );
    assert!(shown(&mut h, "themes-row-user-night"));
    assert_eq!(modal_error(&mut h), None);
    assert_eq!(drawn(&mut h), NORD, "nothing is applied");
}

#[gpui::test]
fn an_imported_name_that_is_taken_gets_the_copy_suffix(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let text = export_json("Nord", &NORD, None, None).expect("export");
    pick_theme_file(&mut h, &dir, "nord-export.json", text.as_bytes());

    h.click_on("themes-import");
    h.cx.run_until_parked();

    assert_eq!(cursor_row(&mut h), ("Nord copy".to_owned(), true));
    assert!(dir.path().join(THEMES_DIR).join("nord-copy.json").exists());
}

#[gpui::test]
fn an_oversized_or_malformed_file_is_refused(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    pick_theme_file(&mut h, &dir, "big.json", &vec![b' '; MAX_THEME_BYTES + 1]);

    h.click_on("themes-import");
    h.cx.run_until_parked();

    assert_eq!(
        modal_error(&mut h).as_deref(),
        Some("Couldn't import the theme: the theme file is 1048577 bytes; the limit is 1 MiB")
    );
    assert!(shown(&mut h, "themes-error"));
    assert!(!dir.path().join(THEMES_DIR).exists(), "nothing is written");

    pick_theme_file(&mut h, &dir, "bad.json", b"{");
    h.click_on("themes-import");
    h.cx.run_until_parked();

    let error = modal_error(&mut h).expect("the error row shows");
    assert!(
        error.starts_with("Couldn't import the theme: ")
            && error.contains("bad.json is not a whole theme"),
        "{error}"
    );
    assert!(!dir.path().join(THEMES_DIR).exists(), "nothing is written");
}
