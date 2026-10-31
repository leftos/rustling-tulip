//! Theme specs: the theme `native-ui.json` names is the one the window
//! draws in from boot, and the sidebar's appearance cascade sits on the same
//! theme; a name that loads nothing boots the built-in theme.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]
#![expect(clippy::unreadable_literal, reason = "hex colors read as #rrggbb")]

use crate::support;

use gpui::TestAppContext;
use rustling_tulip_native::appearance::ThemeBase;
use rustling_tulip_native::theme_store::{THEMES_DIR, ThemeStore};
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
    let mut h = Harness::with(cx, &dir, &fixture());

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
fn an_unknown_saved_theme_boots_the_built_in(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    select_theme(&dir, "no-such-theme");
    let mut h = Harness::with(cx, &dir, &fixture());

    assert_eq!(h.root(|_, cx| ui_theme::theme(cx)), UiTheme::CONTRAST);
    assert_eq!(
        h.root(|root, _| root.theme_base()),
        ThemeBase::of(&UiTheme::CONTRAST)
    );
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("the layout");
    let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(saved["theme"], "no-such-theme", "the selection is kept");
}
