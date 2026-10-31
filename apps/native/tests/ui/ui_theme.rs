//! Theme specs: the theme `native-ui.json` names is the one the window
//! draws in from boot, and the sidebar's appearance cascade sits on the same
//! theme; no name, or a name that loads nothing, boots the default, Nord.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]
#![expect(clippy::unreadable_literal, reason = "hex colors read as #rrggbb")]

use crate::support;

use gpui::TestAppContext;
use rustling_tulip_native::appearance::ThemeBase;
use rustling_tulip_native::builtin_themes::{DRACULA, NORD};
use rustling_tulip_native::theme_store::{THEMES_DIR, ThemeError, ThemeStore};
use rustling_tulip_native::ui_theme::{self, UiTheme};
use support::{Fixture, Harness, TestDir, repo, session};

/// Writes a `native-ui.json` whose theme selection is `id`.
fn select_theme(dir: &TestDir, id: &str) {
    std::fs::write(
        dir.path().join("native-ui.json"),
        serde_json::json!({ "theme": id }).to_string(),
    )
    .expect("write native-ui.json");
}

/// The theme `native-ui.json` names, when it names one.
fn saved_theme(dir: &TestDir) -> Option<String> {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).ok()?;
    let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    saved["theme"].as_str().map(str::to_owned)
}

/// One repo with one session that sets no appearance of its own.
fn fixture() -> Fixture {
    Fixture {
        repos: vec![repo("r1", "C:/repos/r1")],
        ..Fixture::single(session("s1").in_repo("r1").build())
    }
}

#[gpui::test]
fn a_saved_user_theme_is_drawn_at_boot(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut mine = UiTheme::CONTRAST;
    mine.ground = 0x102030;
    mine.accent = 0x40c080;
    let id = ThemeStore::new(dir.path().join(THEMES_DIR))
        .save("Mine", &mine, Some("contrast"), None)
        .expect("save the user theme");
    select_theme(&dir, &id);
    let mut h = Harness::with_saved_theme(cx, &dir, &fixture());

    let drawn = h.root(|_, cx| ui_theme::theme(cx));
    assert_eq!(drawn.ground, 0x102030, "the window draws the user theme");
    assert_eq!(drawn, mine);
    assert_eq!(
        h.root(|root, _| root.theme_base()),
        ThemeBase::of(&mine),
        "the cascade sits on the same theme"
    );
    assert_eq!(h.root(|root, _| root.theme_base().accent), 0x40c080);
    assert_eq!(
        h.root(|root, _| root.session_accent("s1")),
        Some(0x40c080),
        "a session with no accent of its own takes the theme's"
    );
}

#[gpui::test]
fn a_fresh_start_draws_nord(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with_saved_theme(cx, &dir, &fixture());

    assert_eq!(h.root(|_, cx| ui_theme::theme(cx)), NORD);
    assert_eq!(h.root(|_, cx| ui_theme::theme(cx)).ground, 0x2e3440);
    assert_eq!(h.root(|root, _| root.theme_base()), ThemeBase::of(&NORD));
    assert_eq!(h.root(|root, _| root.theme_base().accent), 0xb48ead);
}

#[gpui::test]
fn an_unknown_saved_theme_boots_the_default(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    select_theme(&dir, "no-such-theme");
    let mut h = Harness::with_saved_theme(cx, &dir, &fixture());

    assert_eq!(h.root(|_, cx| ui_theme::theme(cx)), NORD);
    assert_eq!(h.root(|root, _| root.theme_base()), ThemeBase::of(&NORD));
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("the layout");
    let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(saved["theme"], "no-such-theme", "the selection is kept");
}

#[gpui::test]
fn selecting_a_theme_draws_it_and_saves_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with_saved_theme(cx, &dir, &fixture());

    h.root_update(|root, _, cx| root.select_theme("dracula", cx))
        .expect("the built-in loads");
    assert_eq!(
        h.root(|_, cx| ui_theme::theme(cx)),
        DRACULA,
        "the window draws it"
    );
    assert_eq!(
        saved_theme(&dir).as_deref(),
        Some("dracula"),
        "the selection is saved at once"
    );
    drop(h);

    let mut h = Harness::with_saved_theme(cx, &dir, &fixture());
    assert_eq!(
        h.root(|_, cx| ui_theme::theme(cx)),
        DRACULA,
        "the next boot draws it"
    );
}

#[gpui::test]
fn selecting_a_theme_that_does_not_load_changes_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    select_theme(&dir, "dracula");
    let mut h = Harness::with_saved_theme(cx, &dir, &fixture());
    let before = h.root(|_, cx| ui_theme::theme(cx));
    assert_eq!(before, DRACULA);

    let err = h
        .root_update(|root, _, cx| root.select_theme("no-such-theme", cx))
        .expect_err("no theme has that id");
    assert!(matches!(err, ThemeError::NotFound { .. }), "{err}");
    assert_eq!(
        h.root(|_, cx| ui_theme::theme(cx)),
        before,
        "the window keeps its theme"
    );
    assert_eq!(
        saved_theme(&dir).as_deref(),
        Some("dracula"),
        "the saved selection is unchanged"
    );
}

#[gpui::test]
fn selecting_a_theme_sets_the_theme_base(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with_saved_theme(cx, &dir, &fixture());

    h.root_update(|root, _, cx| root.select_theme("dracula", cx))
        .expect("the built-in loads");
    assert_eq!(h.root(|root, _| root.theme_base()), ThemeBase::of(&DRACULA));
    assert_eq!(h.root(|root, _| root.theme_base().accent), DRACULA.accent);
}

#[gpui::test]
fn selecting_a_saved_user_theme_draws_it_and_saves_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut mine = UiTheme::CONTRAST;
    mine.ground = 0x102030;
    mine.accent = 0x40c080;
    let id = ThemeStore::new(dir.path().join(THEMES_DIR))
        .save("Mine", &mine, Some("contrast"), None)
        .expect("save the user theme");
    let mut h = Harness::with_saved_theme(cx, &dir, &fixture());

    h.root_update(|root, _, cx| root.select_theme(&id, cx))
        .expect("the user theme loads");
    assert_eq!(
        h.root(|_, cx| ui_theme::theme(cx)),
        mine,
        "the window draws the user theme"
    );
    assert_eq!(
        saved_theme(&dir).as_deref(),
        Some(id.as_str()),
        "the selection is saved at once"
    );
}

#[gpui::test]
fn selecting_a_corrupt_user_theme_changes_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let themes = dir.path().join(THEMES_DIR);
    std::fs::create_dir_all(&themes).expect("make the themes folder");
    std::fs::write(themes.join("broken.json"), "{").expect("write the broken file");
    select_theme(&dir, "dracula");
    let mut h = Harness::with_saved_theme(cx, &dir, &fixture());
    let before = h.root(|_, cx| ui_theme::theme(cx));
    assert_eq!(before, DRACULA);

    let err = h
        .root_update(|root, _, cx| root.select_theme("broken", cx))
        .expect_err("the file is not a theme");
    assert!(matches!(err, ThemeError::Parse { .. }), "{err}");
    assert_eq!(
        h.root(|_, cx| ui_theme::theme(cx)),
        before,
        "the window keeps its theme"
    );
    assert_eq!(
        saved_theme(&dir).as_deref(),
        Some("dracula"),
        "the saved selection is unchanged"
    );
}
