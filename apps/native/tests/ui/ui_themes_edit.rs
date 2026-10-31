//! Theme editor specs: Duplicate and edit opens the Themes modal's editor on a
//! copy that nothing writes until Save, the keyboard walks the tokens and edits
//! them through one input, Save, Revert and Delete write through the store, and
//! the window keeps its theme until a save of the theme in use.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::TestAppContext;
use rustling_tulip_native::Connection;
use rustling_tulip_native::builtin_themes::NORD;
use rustling_tulip_native::target::Target;
use rustling_tulip_native::theme_store::{THEMES_DIR, ThemeStore};
use rustling_tulip_native::theme_tokens::{self, TokenValue};
use rustling_tulip_native::themes::ThemeEditor;
use rustling_tulip_native::ui_theme::{self, UiTheme};
use support::{Fixture, Harness, TestDir, repo, session};

fn fixture() -> Fixture {
    Fixture {
        repos: vec![repo("r1", "C:/repos/r1")],
        ..Fixture::single(session("s1").in_repo("r1").build())
    }
}

fn store(dir: &TestDir) -> ThemeStore {
    ThemeStore::new(dir.path().join(THEMES_DIR))
}

/// The theme `native-ui.json` names, when it names one.
fn saved_theme(dir: &TestDir) -> Option<String> {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).ok()?;
    let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    saved["theme"].as_str().map(str::to_owned)
}

fn select_saved(dir: &TestDir, id: &str) {
    std::fs::write(
        dir.path().join("native-ui.json"),
        serde_json::json!({ "theme": id }).to_string(),
    )
    .expect("write native-ui.json");
}

/// Whether the last frame drew what `selector` tags.
fn shown(h: &mut Harness<'_>, selector: &'static str) -> bool {
    h.cx.run_until_parked();
    h.cx.debug_bounds(selector).is_some()
}

fn drawn(h: &mut Harness<'_>) -> UiTheme {
    h.root(|_, cx| ui_theme::theme(cx))
}

fn focus(h: &mut Harness<'_>) -> Option<&'static str> {
    h.root(|root, _| root.themes_modal_focus())
}

/// The window on the saved theme with the Themes modal open.
fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with_saved_theme(cx, dir, &fixture());
    h.keys("ctrl-,");
    h.click_on("settings-tab-appearance");
    h.click_on("appearance-theme");
    assert!(
        h.root(|root, _| root.themes_modal_open()),
        "the modal opens"
    );
    h
}

/// The editor on a copy of Nord, the theme in use.
fn duplicate<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = opened(cx, dir);
    h.click_on("themes-duplicate");
    assert!(editing(&mut h), "the editor opens");
    h
}

fn editing(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.themes_editor().is_some())
}

fn editor(h: &mut Harness<'_>) -> ThemeEditor {
    h.root(|root, _| root.themes_editor().expect("the editor is open").clone())
}

fn hex_text(h: &mut Harness<'_>) -> String {
    h.root(|root, cx| root.themes_editor_hex(cx).expect("the editor is open"))
}

fn hex_note(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.themes_editor_hex_note())
}

fn name_note(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.themes_editor_name_note())
}

fn ask(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.themes_ask())
}

/// Types `text` into the field that has the keyboard, one keystroke per
/// character.
fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
}

/// Presses Up or Down until the token `name` is under the cursor.
fn cursor_to_token(h: &mut Harness<'_>, name: &str) {
    let at = theme_tokens::ROWS
        .iter()
        .position(|row| row.name == name)
        .expect("the token is listed");
    let from = editor(h).cursor;
    let key = if at > from { "down" } else { "up" };
    for _ in 0..at.abs_diff(from) {
        h.keys(key);
    }
    assert_eq!(editor(h).cursor, at);
}

/// Types `text` into the hex input for the token `name` and submits it.
fn enter_value(h: &mut Harness<'_>, name: &str, text: &str) {
    cursor_to_token(h, name);
    h.keys("enter");
    type_text(h, text);
    h.keys("enter");
}

/// Presses Down until the row of `id` that is a user's or a built-in is under
/// the modal's cursor.
fn row_cursor_to(h: &mut Harness<'_>, id: &str, built_in: bool) {
    let id = id.to_owned();
    let (from, at) = h.root(move |root, _| {
        let dialog = root.themes_modal().expect("the modal is open");
        let at = dialog
            .rows
            .iter()
            .position(|row| row.entry.id == id && row.entry.built_in == built_in)
            .expect("the row is listed");
        (dialog.cursor, at)
    });
    let key = if at > from { "down" } else { "up" };
    for _ in 0..at.abs_diff(from) {
        h.keys(key);
    }
}

/// Whether the modal lists a row of `id` that is a user's or a built-in.
fn listed(h: &mut Harness<'_>, id: &str, built_in: bool) -> bool {
    let id = id.to_owned();
    h.root(move |root, _| {
        root.themes_modal()
            .expect("the modal is open")
            .rows
            .iter()
            .any(|row| row.entry.id == id && row.entry.built_in == built_in)
    })
}

fn file_exists(dir: &TestDir, id: &str) -> bool {
    dir.path()
        .join(THEMES_DIR)
        .join(format!("{id}.json"))
        .exists()
}

#[gpui::test]
fn duplicate_opens_the_editor_and_writes_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);

    let opened = editor(&mut h);
    assert_eq!(opened.id, None);
    assert_eq!(opened.name, "Nord copy");
    assert_eq!(opened.base.as_deref(), Some("nord"));
    assert_eq!(opened.working, NORD);
    assert!(!opened.is_dirty());
    assert!(!dir.path().join(THEMES_DIR).exists(), "nothing is written");
    assert_eq!(hex_text(&mut h), "#2e3440", "the first row's value");
    assert!(shown(&mut h, "themes-editor-preview"));
}

#[gpui::test]
fn up_and_down_move_the_token_cursor(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);

    h.keys("down");
    assert_eq!(editor(&mut h).cursor, 1);
    let row = &theme_tokens::ROWS[1];
    let value = theme_tokens::get(&NORD, row.name).expect("the row reads");
    assert_eq!(
        hex_text(&mut h),
        theme_tokens::format_value(row, value),
        "the input follows the cursor"
    );

    h.keys("up up");
    assert_eq!(editor(&mut h).cursor, 0, "it stops at the top");
}

#[gpui::test]
fn enter_focuses_the_hex_input(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);
    assert_eq!(focus(&mut h), Some("themes-list"));

    h.keys("enter");
    assert_eq!(focus(&mut h), Some("themes-hex"));

    h.keys("escape");
    assert_eq!(
        focus(&mut h),
        Some("themes-list"),
        "Esc returns to the list"
    );
    assert!(editing(&mut h), "and stays in the editor");
}

#[gpui::test]
fn typing_reaches_the_hex_input(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);

    h.keys("enter");
    type_text(&mut h, "#abc");

    assert_eq!(hex_text(&mut h), "#abc", "the typing replaces the value");
    assert_eq!(editor(&mut h).working, NORD, "nothing applies before Enter");
}

#[gpui::test]
fn a_valid_hex_updates_the_preview_theme(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);

    enter_value(&mut h, "ground", "#112233");

    let edited = editor(&mut h);
    assert_eq!(edited.working.ground, 0x0011_2233);
    assert!(edited.is_dirty());
    assert_eq!(hex_note(&mut h), None);
    assert_eq!(
        focus(&mut h),
        Some("themes-list"),
        "focus returns to the list"
    );
}

#[gpui::test]
fn an_invalid_hex_shows_its_note_and_changes_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);

    enter_value(&mut h, "ground", "#12");

    assert_eq!(hex_note(&mut h).as_deref(), Some("Use #RRGGBB"));
    assert!(shown(&mut h, "themes-hex-note"));
    assert_eq!(editor(&mut h).working, NORD);
    assert_eq!(focus(&mut h), Some("themes-hex"));
}

#[gpui::test]
fn an_alpha_row_takes_eight_digits(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);

    enter_value(&mut h, "row_card", "#112233");
    assert_eq!(hex_note(&mut h).as_deref(), Some("Use #RRGGBBAA"));
    h.keys("escape");

    enter_value(&mut h, "row_card", "#11223344");
    assert_eq!(editor(&mut h).working.row_card, 0x1122_3344);
    assert_eq!(hex_note(&mut h), None);
}

#[gpui::test]
fn a_share_row_takes_a_number(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);

    enter_value(&mut h, "diff_filler_alpha", "2");
    assert_eq!(
        hex_note(&mut h).as_deref(),
        Some("Use a number from 0 to 1")
    );
    h.keys("escape");

    enter_value(&mut h, "diff_filler_alpha", "0.5");
    assert_eq!(
        theme_tokens::get(&editor(&mut h).working, "diff_filler_alpha"),
        Some(TokenValue::Share(0.5))
    );
}

#[gpui::test]
fn the_badge_follows_the_token(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);
    assert!(shown(&mut h, "themes-badge-text"), "text is in a pair");
    assert!(!shown(&mut h, "themes-badge-chip"), "a chip is in none");
    let before = h.root(|root, _| root.themes_badge("text"));
    assert_ne!(before.as_deref(), Some("1.0:1"));

    enter_value(&mut h, "text", "#2e3440");

    assert_eq!(
        h.root(|root, _| root.themes_badge("text")).as_deref(),
        Some("1.0:1"),
        "text on its own ground"
    );
    assert!(editor(&mut h).badge("text").expect("a pair").below);
    assert_eq!(
        h.root(|root, _| root.themes_editor_summary()).as_deref(),
        Some(&*format!(
            "{} pairs below AA",
            editor(&mut h).failing().len()
        ))
    );
}

#[gpui::test]
fn save_writes_a_new_theme_and_keeps_editing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);
    enter_value(&mut h, "ground", "#112233");

    h.click_on("themes-save");

    let saved = editor(&mut h);
    assert_eq!(saved.id.as_deref(), Some("nord-copy"));
    assert!(!saved.is_dirty(), "the saved tokens are the new original");
    assert_eq!(
        store(&dir).load("nord-copy").expect("the file"),
        saved.working
    );
    assert_eq!(saved_theme(&dir), None, "a save selects nothing");
    assert_eq!(drawn(&mut h), NORD);

    enter_value(&mut h, "ground", "#223344");
    h.click_on("themes-save");
    let entries = store(&dir).list();
    assert_eq!(
        entries.iter().filter(|entry| !entry.built_in).count(),
        1,
        "the second save updates the same file"
    );

    h.keys("escape");
    assert!(!editing(&mut h));
    let on = h.root(|root, _| {
        let dialog = root.themes_modal().expect("the modal is open");
        dialog.rows[dialog.cursor].entry.id.clone()
    });
    assert_eq!(on, "nord-copy", "the list comes back on the saved row");
}

#[gpui::test]
fn save_of_the_theme_in_use_redraws_the_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let id = store(&dir).save("Mine", &NORD, None, None).expect("save");
    select_saved(&dir, &id);
    let mut h = opened(cx, &dir);

    h.click_on("themes-edit");
    let opened = editor(&mut h);
    assert_eq!(opened.id.as_deref(), Some(id.as_str()));
    assert_eq!(opened.name, "Mine");
    enter_value(&mut h, "ground", "#112233");
    assert_eq!(drawn(&mut h).ground, NORD.ground, "not before the save");

    h.click_on("themes-save");

    assert_eq!(drawn(&mut h).ground, 0x0011_2233);
    assert_eq!(saved_theme(&dir).as_deref(), Some(id.as_str()));
}

#[gpui::test]
fn save_refuses_a_taken_name(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);

    h.click_on("themes-name");
    h.keys("ctrl-a");
    type_text(&mut h, "Nord");
    assert_eq!(editor(&mut h).name, "Nord");
    h.click_on("themes-save");

    assert_eq!(
        name_note(&mut h).as_deref(),
        Some("A theme is already called Nord")
    );
    assert_eq!(editor(&mut h).id, None);
    assert!(!dir.path().join(THEMES_DIR).exists(), "nothing is written");
}

#[gpui::test]
fn save_refuses_a_blank_name(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);

    h.click_on("themes-name");
    h.keys("ctrl-a backspace");
    h.click_on("themes-save");

    assert_eq!(name_note(&mut h).as_deref(), Some("Give the theme a name"));
    assert!(!dir.path().join(THEMES_DIR).exists(), "nothing is written");
}

#[gpui::test]
fn revert_is_disabled_until_a_change(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);

    h.keys("tab");
    assert_eq!(
        focus(&mut h),
        Some("themes-save"),
        "a clean Revert is skipped"
    );
    h.keys("shift-tab");
    enter_value(&mut h, "ground", "#112233");
    h.click_on("themes-revert");

    assert_eq!(editor(&mut h).working, NORD);
    assert!(!editor(&mut h).is_dirty());
    assert_eq!(hex_text(&mut h), "#2e3440", "the input follows");
}

#[gpui::test]
fn esc_with_changes_asks_before_leaving(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);
    enter_value(&mut h, "ground", "#112233");

    h.keys("escape");
    assert_eq!(ask(&mut h).as_deref(), Some("Discard your changes?"));
    assert!(editing(&mut h));

    h.keys("enter");
    assert_eq!(ask(&mut h), None, "Enter keeps editing");
    assert_eq!(editor(&mut h).working.ground, 0x0011_2233);

    h.click_on("themes-close");
    assert_eq!(ask(&mut h).as_deref(), Some("Discard your changes?"));
    h.click_on("themes-ask-yes");
    assert!(!editing(&mut h), "back on the list");
    assert!(h.root(|root, _| root.themes_modal_open()));
    assert!(!dir.path().join(THEMES_DIR).exists(), "nothing is written");
}

#[gpui::test]
fn esc_with_no_changes_leaves_at_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);

    h.keys("escape");

    assert_eq!(ask(&mut h), None);
    assert!(!editing(&mut h));
    assert!(h.root(|root, _| root.themes_modal_open()));
    assert_eq!(focus(&mut h), Some("themes-list"));
}

#[gpui::test]
fn closing_settings_drops_the_editor(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);
    enter_value(&mut h, "ground", "#112233");

    h.set_connection(Connection::for_target(Target::tailnet(
        "box.tail1234.ts.net",
        None,
    )));

    assert!(!h.root(|root, _| root.settings_open()));
    assert!(!h.root(|root, _| root.themes_modal_open()));
    assert_eq!(ask(&mut h), None, "no prompt");
    assert!(!dir.path().join(THEMES_DIR).exists(), "nothing is written");
    assert_eq!(drawn(&mut h), NORD);
}

#[gpui::test]
fn the_window_does_not_recolour_while_editing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);

    enter_value(&mut h, "accent", "#ff0000");

    assert_eq!(editor(&mut h).working.accent, 0x00ff_0000);
    assert_eq!(drawn(&mut h), NORD);
}

#[gpui::test]
fn delete_of_the_theme_in_use_falls_to_nord(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let id = store(&dir)
        .save("Mine", &UiTheme::CONTRAST, None, None)
        .expect("save");
    select_saved(&dir, &id);
    let mut h = opened(cx, &dir);
    assert_eq!(drawn(&mut h), UiTheme::CONTRAST);

    h.click_on("themes-delete");
    assert_eq!(ask(&mut h).as_deref(), Some("Delete Mine?"));
    assert!(file_exists(&dir, &id), "not before the answer");
    h.click_on("themes-ask-yes");

    assert!(!file_exists(&dir, &id));
    assert_eq!(drawn(&mut h), NORD);
    assert_eq!(saved_theme(&dir).as_deref(), Some("nord"));
    assert_eq!(
        h.root(|root, _| root.appearance_theme_name()).as_deref(),
        Some("Nord"),
        "the Settings row follows"
    );
    let (cursor, rows) = h.root(|root, _| {
        let dialog = root.themes_modal().expect("the modal is open");
        (dialog.cursor, dialog.rows.len())
    });
    assert!(cursor < rows, "the cursor is clamped");
}

#[gpui::test]
fn delete_removes_a_broken_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let themes = dir.path().join(THEMES_DIR);
    std::fs::create_dir_all(&themes).expect("make the themes folder");
    std::fs::write(themes.join("broken.json"), "{").expect("write the broken file");
    let mut h = opened(cx, &dir);
    row_cursor_to(&mut h, "broken", false);

    h.click_on("themes-delete");
    h.click_on("themes-ask-yes");

    assert!(!file_exists(&dir, "broken"));
    assert!(!listed(&mut h, "broken", false));
}

#[gpui::test]
fn delete_removes_a_shadow_file_and_keeps_the_built_in(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let id = store(&dir)
        .save("Mine", &UiTheme::CONTRAST, None, None)
        .expect("save");
    let themes = dir.path().join(THEMES_DIR);
    std::fs::copy(themes.join(format!("{id}.json")), themes.join("nord.json")).expect("copy");
    let mut h = opened(cx, &dir);
    row_cursor_to(&mut h, "nord", false);

    h.click_on("themes-delete");
    h.click_on("themes-ask-yes");

    assert!(!file_exists(&dir, "nord"));
    assert!(!listed(&mut h, "nord", false));
    assert!(listed(&mut h, "nord", true));
    assert_eq!(drawn(&mut h), NORD, "the built-in stays in use");
}

#[gpui::test]
fn pick_mode_jumps_to_the_clicked_token(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);
    let name_of = |editor: &ThemeEditor| theme_tokens::ROWS[editor.cursor].name;

    h.click_on("pick-pane-header");
    assert_eq!(name_of(&editor(&mut h)), "ground", "Pick is off");

    h.click_on("themes-pick");
    h.click_on("pick-pane-header");
    assert_eq!(name_of(&editor(&mut h)), "raised");
    let row = &theme_tokens::ROWS[editor(&mut h).cursor];
    let value = theme_tokens::get(&NORD, row.name).expect("the row reads");
    assert_eq!(hex_text(&mut h), theme_tokens::format_value(row, value));

    h.click_on("pick-chip-asking");
    assert_eq!(name_of(&editor(&mut h)), "asking");
}

#[gpui::test]
fn export_stays_disabled(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);
    assert!(shown(&mut h, "themes-export"));

    h.click_on("themes-export");

    assert!(editing(&mut h));
    assert!(!dir.path().join(THEMES_DIR).exists(), "nothing is written");
    h.keys("tab tab tab");
    assert_ne!(focus(&mut h), Some("themes-export"), "out of the ring");
}

/// Lifts `key`, the way the platform reports it.
fn key_up(h: &mut Harness<'_>, key: &str) {
    h.cx.simulate_event(gpui::KeyUpEvent {
        keystroke: gpui::Keystroke::parse(key).expect("a key"),
    });
    h.cx.run_until_parked();
}

#[gpui::test]
fn clicking_save_from_the_hex_input_leaves_the_ring_on_the_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);
    h.keys("enter");
    assert_eq!(focus(&mut h), Some("themes-hex"));

    h.click_on("themes-save");

    assert_eq!(focus(&mut h), Some("themes-list"));
}

#[gpui::test]
fn a_click_in_the_name_field_keeps_the_caret(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);

    h.click_on("themes-name");
    type_text(&mut h, "x");

    let name = editor(&mut h).name;
    assert_eq!(name.len(), "Nord copyx".len(), "{name}");
}

#[gpui::test]
fn tab_then_enter_discards(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);
    enter_value(&mut h, "ground", "#112233");
    h.keys("escape");

    h.keys("tab enter");

    assert!(!editing(&mut h), "the changes are gone");
}

#[gpui::test]
fn tab_then_enter_deletes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let id = store(&dir)
        .save("Mine", &UiTheme::CONTRAST, None, None)
        .expect("save");
    select_saved(&dir, &id);
    let mut h = opened(cx, &dir);
    h.click_on("themes-delete");

    h.keys("tab enter");

    assert!(!file_exists(&dir, &id));
}

#[gpui::test]
fn a_click_on_a_row_does_nothing_while_asking(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);
    enter_value(&mut h, "ground", "#112233");
    h.keys("escape");

    h.click_on("themes-token-sunken");

    assert_eq!(editor(&mut h).cursor, 0);
    assert_eq!(ask(&mut h).as_deref(), Some("Discard your changes?"));
}

#[gpui::test]
fn an_unchanged_hex_leaves_the_editor_clean(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    row_cursor_to(&mut h, "contrast", true);
    h.click_on("themes-duplicate");
    cursor_to_token(&mut h, "danger");

    h.keys("enter");
    h.held("enter");
    assert_eq!(
        focus(&mut h),
        Some("themes-hex"),
        "a held Enter submits nothing"
    );

    key_up(&mut h, "enter");
    h.keys("enter");

    assert_eq!(focus(&mut h), Some("themes-list"));
    assert!(!editor(&mut h).is_dirty(), "the followers stay as stored");
}

#[gpui::test]
fn a_held_esc_does_not_strobe_the_question(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = duplicate(cx, &dir);
    enter_value(&mut h, "ground", "#112233");
    h.held("escape");
    assert_eq!(ask(&mut h), None, "a held Esc does not ask");

    h.keys("escape");
    h.held("escape");

    assert_eq!(ask(&mut h).as_deref(), Some("Discard your changes?"));
}
