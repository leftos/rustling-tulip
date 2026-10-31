//! Auto-update specs: the check 30 s after launch and every 24 h, the
//! footer's notice of a ready update and its Restart to update button, the
//! Settings → General update rows, and the toast after an update. The
//! updater is the harness's recording fake: no feed, no installer.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{TestAppContext, px};
use rustling_tulip_native::NetCommand;
use rustling_tulip_native::update::LastResult;
use support::{Fixture, Harness, TestDir, UpdaterRecorder, pane, session, tab};

const FIRST_CHECK: Duration = Duration::from_secs(30);
const CHECK_EVERY: Duration = Duration::from_hours(24);
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// A stand-in installer in `dir`, which a ready update holds open.
fn installer(dir: &TestDir) -> PathBuf {
    let path = dir.path().join("rustling-tulip-0.99.0-setup.exe");
    std::fs::write(&path, b"stand-in installer").expect("write the stand-in installer");
    path
}

/// The window over `updater`, with one working session loaded, so a plain
/// close would ask what to do with it.
fn opened<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    updater: &Arc<UpdaterRecorder>,
) -> Harness<'a> {
    let mut h = Harness::open_with_updater(cx, dir, None, Arc::clone(updater));
    h.load(&Fixture {
        sessions: vec![session("s1").status("working").build()],
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
        ..Fixture::default()
    });
    h.answer_scrollback("s1", b"");
    h
}

/// Whether the footer draws Restart to update. gpui keeps an element's
/// last bounds after it goes, so this only proves it was drawn at some
/// point; [`footer`] says whether it still is.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.title.clone(), toast.detail.clone()))
            .collect()
    })
}

fn footer(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.update_footer_text())
}

fn settings_lines(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.update_settings_text())
}

fn saved_ui(dir: &TestDir) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("a saved layout");
    serde_json::from_str(&text).expect("native-ui.json is JSON")
}

fn turned_off(dir: &TestDir) {
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "general": { "check_updates": false } }"#,
    )
    .expect("write the layout");
}

/// Opens Settings → General scrolled to `selector` and presses it.
fn press_in_settings(h: &mut Harness<'_>, selector: &str) {
    if !h.root(|root, _| root.settings_open()) {
        h.keys("ctrl-,");
    }
    h.scroll_into_view("settings-content", selector);
    h.click_on(selector);
}

fn shutdowns(h: &mut Harness<'_>) -> usize {
    h.commands()
        .iter()
        .filter(|command| matches!(command, NetCommand::Shutdown { .. }))
        .count()
}

fn ready_update<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    updater: &Arc<UpdaterRecorder>,
) -> Harness<'a> {
    updater.offer("0.99.0", &installer(dir));
    let mut h = opened(cx, dir, updater);
    h.advance(FIRST_CHECK);
    assert_eq!(footer(&mut h).as_deref(), Some("Update 0.99.0 ready"));
    h
}

#[gpui::test]
fn a_ready_update_shows_in_the_footer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    updater.offer("0.99.0", &installer(&dir));
    let mut h = opened(cx, &dir, &updater);

    h.advance(Duration::from_secs(29));
    assert_eq!(updater.checks(), 0, "no check before 30 s");
    assert_eq!(footer(&mut h), None);
    assert!(!drawn(&mut h, "footer-update"));

    h.advance(Duration::from_secs(1));
    assert_eq!(updater.checks(), 1, "the check 30 s after launch");
    assert_eq!(footer(&mut h).as_deref(), Some("Update 0.99.0 ready"));
    assert!(drawn(&mut h, "footer-update"), "Restart to update is drawn");

    h.advance(CHECK_EVERY);
    assert_eq!(
        updater.checks(),
        1,
        "no scheduled check while an update is ready"
    );
}

#[gpui::test]
fn check_now_still_checks_while_an_update_is_ready(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    let mut h = ready_update(cx, &dir, &updater);
    let before = updater.checks();

    press_in_settings(&mut h, "settings-general-check-now");

    assert_eq!(updater.checks(), before + 1, "Check now ran a check");
}

#[gpui::test]
fn restart_to_update_starts_the_installer_and_quits_keeping_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    let mut h = ready_update(cx, &dir, &updater);
    h.commands();

    h.click_on("footer-update");

    assert_eq!(updater.installs(), [installer(&dir)]);
    assert_eq!(h.quit_requests(), 1, "the client quits");
    assert!(!h.root(|root, _| root.exit_dialog_open()), "no exit dialog");
    assert_eq!(shutdowns(&mut h), 0, "the daemon keeps running");
}

#[gpui::test]
fn a_failed_installer_start_keeps_the_client_and_says_so(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    let mut h = ready_update(cx, &dir, &updater);
    updater.fail_installs("access is denied");

    h.click_on("footer-update");

    assert_eq!(h.quit_requests(), 0, "the client stays");
    assert_eq!(
        toasts(&mut h),
        [(
            "Couldn't start the update".to_owned(),
            Some("access is denied".to_owned())
        )]
    );
    assert_eq!(
        footer(&mut h).as_deref(),
        Some("Update 0.99.0 ready"),
        "the update is still offered"
    );

    updater.refuse_installs_as_running();
    h.click_on("footer-update");
    assert_eq!(h.quit_requests(), 0);
    let titles: Vec<String> = toasts(&mut h).into_iter().map(|(title, _)| title).collect();
    assert!(
        titles.contains(&"An update is already running".to_owned()),
        "{titles:?}"
    );
    assert_eq!(shutdowns(&mut h), 0);
}

#[gpui::test]
fn checks_are_skipped_when_turned_off(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    updater.offer("0.99.0", &installer(&dir));
    let mut h = opened(cx, &dir, &updater);

    press_in_settings(&mut h, "settings-general-check-updates");
    assert_eq!(
        saved_ui(&dir)["general"]["check_updates"],
        false,
        "saved at once"
    );
    h.keys("escape");

    h.advance(FIRST_CHECK);
    h.advance(CHECK_EVERY);
    assert_eq!(updater.checks(), 0, "no check while turned off");
    assert_eq!(footer(&mut h), None);
}

#[gpui::test]
fn check_now_checks_even_when_turned_off(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    turned_off(&dir);
    let updater = Arc::new(UpdaterRecorder::default());
    let mut h = opened(cx, &dir, &updater);
    h.advance(FIRST_CHECK);
    assert_eq!(updater.checks(), 0);

    press_in_settings(&mut h, "settings-general-check-now");

    assert_eq!(updater.checks(), 1, "Check now ran a check");
    assert_eq!(settings_lines(&mut h)[1], "Up to date");
}

#[gpui::test]
fn a_successful_update_says_updated_to_at_launch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    updater.set_last_result(LastResult {
        from: Some("0.1.0".to_owned()),
        to: "0.99.0".to_owned(),
        ok: true,
        message: "updated to 0.99.0".to_owned(),
    });

    let mut h = opened(cx, &dir, &updater);

    assert_eq!(toasts(&mut h), [("Updated to 0.99.0".to_owned(), None)]);
    assert_eq!(updater.last_result_reads(), 1, "read once at launch");
}

#[gpui::test]
fn a_failed_update_says_it_did_not_install(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    let message = "could not stop the daemon: timed out; nothing was changed";
    updater.set_last_result(LastResult {
        from: Some("0.1.0".to_owned()),
        to: "0.99.0".to_owned(),
        ok: false,
        message: message.to_owned(),
    });

    let mut h = opened(cx, &dir, &updater);

    assert_eq!(
        toasts(&mut h),
        [(
            "Update to 0.99.0 didn't install".to_owned(),
            Some(message.to_owned())
        )]
    );
}

#[gpui::test]
fn settings_shows_the_version_and_last_check(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    let mut h = opened(cx, &dir, &updater);
    h.keys("ctrl-,");
    h.scroll_into_view("settings-content", "settings-update-version");
    let version = format!("Version {CURRENT}");
    assert_eq!(
        settings_lines(&mut h),
        [version.clone(), "Not checked yet".to_owned()]
    );

    updater.fail_checks("the feed is down");
    press_in_settings(&mut h, "settings-general-check-now");
    assert_eq!(
        settings_lines(&mut h),
        [
            version.clone(),
            "Last check failed: the feed is down".to_owned()
        ]
    );

    updater.offer("0.99.0", &installer(&dir));
    press_in_settings(&mut h, "settings-general-check-now");
    assert_eq!(
        settings_lines(&mut h),
        [version.clone(), "Update 0.99.0 ready".to_owned()]
    );

    updater.offer_nothing();
    press_in_settings(&mut h, "settings-general-check-now");
    assert_eq!(settings_lines(&mut h), [version, "Up to date".to_owned()]);
}
