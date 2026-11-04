//! Auto-update specs: the check at every launch and every hour, the
//! footer's notice of a ready update and its Restart to update button, the
//! Settings → General update rows and their Restart to update button, and
//! the toast after an update. The updater is the harness's recording fake:
//! no feed, no installer.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{Modifiers, TestAppContext, VisualTestContext, px};
use rustling_tulip_native::NetCommand;
use rustling_tulip_native::update::LastResult;
use rustling_tulip_native::{
    PopoutKey, Toast, ToastAction, ToastKind, ToastSpec, toast_action_selector,
};
use serde_json::json;
use support::{Fixture, Harness, TestDir, UpdaterRecorder, pane, seed_ui, session, tab};

const CHECK_EVERY: Duration = Duration::from_hours(1);
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
    h.click_on("settings-apply");
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
    h.cx.run_until_parked();
    assert_eq!(footer(&mut h).as_deref(), Some("Update 0.99.0 ready"));
    h
}

#[gpui::test]
fn a_check_is_requested_right_after_launch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    updater.offer("0.99.0", &installer(&dir));
    let mut h = opened(cx, &dir, &updater);
    h.cx.run_until_parked();

    assert_eq!(updater.checks(), 1, "the check at launch");
    assert_eq!(footer(&mut h).as_deref(), Some("Update 0.99.0 ready"));
}

/// Nothing a launch keeps says when the last check ran: a relaunch over the
/// same layout checks at once, moments after the last window checked.
#[gpui::test]
fn the_launch_check_runs_even_right_after_a_check(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    {
        let mut first = opened(cx, &dir, &updater);
        first.cx.run_until_parked();
        press_in_settings(&mut first, "settings-general-check-now");
        assert_eq!(updater.checks(), 2, "the launch check, then Check now");
    }

    updater.offer("0.99.0", &installer(&dir));
    let mut h = opened(cx, &dir, &updater);
    h.cx.run_until_parked();

    assert_eq!(updater.checks(), 3, "the relaunch checks at once");
    assert_eq!(footer(&mut h).as_deref(), Some("Update 0.99.0 ready"));
}

#[gpui::test]
fn no_check_runs_at_launch_with_checks_off(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    turned_off(&dir);
    let updater = Arc::new(UpdaterRecorder::default());
    updater.offer("0.99.0", &installer(&dir));
    let mut h = opened(cx, &dir, &updater);
    h.cx.run_until_parked();

    assert_eq!(updater.checks(), 0, "no check while turned off");
    assert_eq!(footer(&mut h), None);
}

#[gpui::test]
fn a_failed_launch_check_is_retried_after_five_minutes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    updater.fail_checks("the network is not up yet");
    let mut h = opened(cx, &dir, &updater);
    h.cx.run_until_parked();
    assert_eq!(updater.checks(), 1, "the check at launch");
    assert_eq!(
        settings_lines(&mut h)[1],
        "Last check failed: the network is not up yet"
    );

    updater.offer("0.99.0", &installer(&dir));
    h.advance(Duration::from_secs(299));
    assert_eq!(updater.checks(), 1, "no retry before 5 minutes");

    h.advance(Duration::from_secs(1));
    assert_eq!(updater.checks(), 2, "the retry 5 minutes after the failure");
    assert_eq!(footer(&mut h).as_deref(), Some("Update 0.99.0 ready"));
}

#[gpui::test]
fn a_ready_update_shows_in_the_footer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    let mut h = ready_update(cx, &dir, &updater);

    assert_eq!(updater.checks(), 1, "the check at launch");
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
fn settings_offers_restart_to_update_once_an_update_is_ready(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    let mut h = opened(cx, &dir, &updater);
    h.cx.run_until_parked();
    assert_eq!(footer(&mut h), None, "the launch check found nothing");
    h.commands();

    updater.offer("0.99.0", &installer(&dir));
    press_in_settings(&mut h, "settings-general-check-now");
    assert!(
        h.root(|root, _| root.settings_open()),
        "Settings stays open"
    );
    h.scroll_into_view("settings-content", "settings-restart-to-update");
    assert!(
        drawn(&mut h, "settings-restart-to-update"),
        "Settings draws Restart to update"
    );

    h.click_on("settings-restart-to-update");

    assert_eq!(updater.installs(), [installer(&dir)]);
    assert_eq!(h.quit_requests(), 1, "the client quits");
    assert_eq!(shutdowns(&mut h), 0, "the daemon keeps running");
}

#[gpui::test]
fn settings_shows_no_restart_without_an_update(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    let mut h = opened(cx, &dir, &updater);
    h.cx.run_until_parked();

    press_in_settings(&mut h, "settings-general-check-now");

    assert_eq!(settings_lines(&mut h)[1], "Up to date");
    assert!(drawn(&mut h, "settings-general-check-now"));
    assert!(
        !drawn(&mut h, "settings-restart-to-update"),
        "no Restart to update without a ready update"
    );
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
    let mut h = opened(cx, &dir, &updater);
    h.cx.run_until_parked();
    assert_eq!(updater.checks(), 1, "the check at launch found nothing");

    press_in_settings(&mut h, "settings-general-check-updates");
    assert_eq!(
        saved_ui(&dir)["general"]["check_updates"],
        false,
        "saved at once"
    );
    h.keys("escape");

    updater.offer("0.99.0", &installer(&dir));
    h.advance(CHECK_EVERY);
    assert_eq!(updater.checks(), 1, "no check while turned off");
    assert_eq!(footer(&mut h), None);
}

#[gpui::test]
fn check_now_checks_even_when_turned_off(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    turned_off(&dir);
    let updater = Arc::new(UpdaterRecorder::default());
    let mut h = opened(cx, &dir, &updater);
    h.cx.run_until_parked();
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

/// Whether the What's new window is open, its title and its text.
fn whats_new(h: &mut Harness<'_>) -> (bool, Option<String>, Option<String>) {
    h.root(|root, _| {
        (
            root.whats_new_open(),
            root.whats_new_title(),
            root.whats_new_text(),
        )
    })
}

/// The toast an installed update raises at launch.
fn installed_toast(h: &mut Harness<'_>) -> Toast {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .find(|toast| toast.key.as_deref() == Some("update-installed"))
            .cloned()
    })
    .expect("the installed toast shows")
}

/// Presses the installed toast's What's new.
fn press_toast_whats_new(h: &mut Harness<'_>) {
    let id = installed_toast(h).id;
    h.click_on(&toast_action_selector(id, "whats-new"));
    h.cx.run_until_parked();
}

fn installed_over(from: Option<&str>, to: &str) -> LastResult {
    LastResult {
        from: from.map(str::to_owned),
        to: to.to_owned(),
        ok: true,
        message: format!("updated to {to}"),
    }
}

/// Presses `selector` in the pop-out window.
fn click_in_popout(popout: &mut VisualTestContext, selector: &str) {
    popout.run_until_parked();
    let name: &'static str = Box::leak(selector.to_owned().into_boxed_str());
    let bounds = popout
        .debug_bounds(name)
        .expect("the control is drawn in the pop-out");
    popout.simulate_click(bounds.center(), Modifiers::none());
    popout.run_until_parked();
}

#[gpui::test]
fn whats_new_from_a_toast_in_a_popout_opens_in_the_main_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    updater.set_last_result(installed_over(Some("0.1.0"), "0.99.0"));
    updater.answer_changelog(Ok("## v0.99.0\n\n- two"));
    let mut h = opened(cx, &dir, &updater);
    let launched = installed_toast(&mut h).id;
    h.click_on(&format!("toast-close-{launched}"));
    let key = PopoutKey::Pane {
        tab_id: "t1".to_owned(),
        pane_id: "p1".to_owned(),
    };
    h.root_update(move |root, window, cx| root.open_popout(key, window, cx));
    let mut popout = h.popout_window();

    // A toast raised after a press in the pop-out shows there.
    click_in_popout(&mut popout, "popout-toolbar-p1");
    let id = h.root_update(|root, _, cx| {
        root.push_toast_spec(
            ToastSpec {
                kind: ToastKind::Info,
                title: "Updated to 0.99.0".to_owned(),
                detail: None,
                key: Some("update-installed".to_owned()),
                sticky: true,
                actions: vec![ToastAction {
                    id: "whats-new",
                    label: "What's new",
                    danger: false,
                }],
            },
            cx,
        )
    });
    click_in_popout(&mut popout, &toast_action_selector(id, "whats-new"));
    h.cx.run_until_parked();

    assert!(h.root(|root, _| root.whats_new_open()));
    assert!(
        h.root_update(|root, window, _| root.whats_new_has_keyboard(window)),
        "the main window, which draws the What's new window, holds the keyboard"
    );
}

#[gpui::test]
fn closing_whats_new_over_settings_hands_the_keyboard_back_to_settings(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    updater.set_offer_notes("## v0.99.0\n\n- one");
    let mut h = ready_update(cx, &dir, &updater);

    press_in_settings(&mut h, "settings-whats-new");
    h.click_on("whats-new-close");

    assert!(!whats_new(&mut h).0, "Close closes the window");
    assert_eq!(
        h.root_update(|root, window, cx| root.settings_focus(window, cx)),
        Some("list"),
        "Settings' tab list holds the keyboard again"
    );
}

#[gpui::test]
fn a_ready_update_offers_whats_new_in_the_footer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    updater.set_offer_notes("## v0.99.0\n\n- one");
    let mut h = ready_update(cx, &dir, &updater);

    assert!(
        drawn(&mut h, "footer-whats-new"),
        "the footer offers What's new"
    );
    h.click_on("footer-whats-new");

    assert_eq!(
        whats_new(&mut h),
        (
            true,
            Some("What's new in 0.99.0".to_owned()),
            Some("## v0.99.0\n\n- one".to_owned())
        )
    );
    h.click_on("whats-new-close");
    assert!(!whats_new(&mut h).0, "Close closes the window");
}

#[gpui::test]
fn settings_offers_whats_new_beside_restart(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    updater.set_offer_notes("## v0.99.0\n\n- one");
    let mut h = ready_update(cx, &dir, &updater);

    press_in_settings(&mut h, "settings-whats-new");

    assert_eq!(
        whats_new(&mut h),
        (
            true,
            Some("What's new in 0.99.0".to_owned()),
            Some("## v0.99.0\n\n- one".to_owned())
        )
    );
    assert!(
        updater.changelog_calls().is_empty(),
        "the notes were in hand"
    );
}

#[gpui::test]
fn a_successful_update_toast_offers_whats_new(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    updater.set_last_result(installed_over(Some("0.1.0"), "0.99.0"));
    updater.answer_changelog(Ok("## v0.99.0\n\n- two"));
    let mut h = opened(cx, &dir, &updater);

    let toast = installed_toast(&mut h);
    assert!(toast.sticky, "the installed toast waits for its reader");
    let actions: Vec<(&str, &str)> = toast
        .actions
        .iter()
        .map(|action| (action.id, action.label))
        .collect();
    assert_eq!(actions, [("whats-new", "What's new")]);

    press_toast_whats_new(&mut h);

    assert_eq!(
        updater.changelog_calls(),
        [(Some("0.1.0".to_owned()), "0.99.0".to_owned())]
    );
    assert_eq!(
        whats_new(&mut h),
        (
            true,
            Some("What's new in 0.99.0".to_owned()),
            Some("## v0.99.0\n\n- two".to_owned())
        )
    );
}

#[gpui::test]
fn the_saved_version_starts_the_changelog_range(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, &json!({ "last_installed_version": "0.5.0" }));
    let updater = Arc::new(UpdaterRecorder::default());
    updater.set_last_result(installed_over(Some("0.1.0"), "0.99.0"));
    let mut h = opened(cx, &dir, &updater);

    press_toast_whats_new(&mut h);

    assert_eq!(
        updater.changelog_calls(),
        [(Some("0.5.0".to_owned()), "0.99.0".to_owned())]
    );
}

#[gpui::test]
fn a_changelog_that_cannot_load_says_so(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    updater.set_last_result(installed_over(Some("0.1.0"), "0.99.0"));
    updater.answer_changelog(Err("rate limited"));
    let mut h = opened(cx, &dir, &updater);

    press_toast_whats_new(&mut h);

    assert_eq!(
        whats_new(&mut h).2.as_deref(),
        Some("Couldn't load the changelog: rate limited")
    );
    assert!(
        drawn(&mut h, "whats-new-releases"),
        "the window offers the releases page"
    );
}

#[gpui::test]
fn a_manual_install_over_an_older_version_says_updated_to(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, &json!({ "last_installed_version": "0.0.1" }));
    let updater = Arc::new(UpdaterRecorder::default());
    let mut h = opened(cx, &dir, &updater);

    let toast = installed_toast(&mut h);
    assert_eq!(toast.title, format!("Updated to {CURRENT}"));
    assert_eq!(
        toast
            .actions
            .iter()
            .map(|action| action.id)
            .collect::<Vec<_>>(),
        ["whats-new"]
    );

    press_toast_whats_new(&mut h);

    assert_eq!(
        updater.changelog_calls(),
        [(Some("0.0.1".to_owned()), CURRENT.to_owned())]
    );
}

/// Only a build that checks for updates records the version it runs: a dev
/// build over the installed client's layout leaves the saved version alone,
/// so the installed client's next launch says nothing about an update that
/// never happened.
#[gpui::test]
fn an_inactive_updater_neither_announces_nor_records_the_version(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, &json!({ "last_installed_version": "0.0.1" }));
    let updater = Arc::new(UpdaterRecorder::default());
    updater.set_inactive();
    let mut h = opened(cx, &dir, &updater);

    assert!(toasts(&mut h).is_empty(), "an inactive build says nothing");
    assert_eq!(
        saved_ui(&dir)["last_installed_version"],
        "0.0.1",
        "the saved version is left alone"
    );
}

#[gpui::test]
fn a_first_run_or_relaunch_says_nothing_and_saves_the_version(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    let mut h = opened(cx, &dir, &updater);

    assert!(toasts(&mut h).is_empty(), "a first run says nothing");
    assert_eq!(saved_ui(&dir)["last_installed_version"], CURRENT);
    drop(h);

    let relaunch = TestDir::new();
    seed_ui(&relaunch, &json!({ "last_installed_version": CURRENT }));
    let updater = Arc::new(UpdaterRecorder::default());
    let mut h = opened(cx, &relaunch, &updater);

    assert!(toasts(&mut h).is_empty(), "a relaunch says nothing");
}

/// The selector the footer's version text carries.
fn version_tag() -> String {
    format!("footer-version=v{CURRENT}")
}

#[gpui::test]
fn footer_shows_the_client_version(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    let mut h = opened(cx, &dir, &updater);

    assert!(drawn(&mut h, &version_tag()), "the footer draws v{CURRENT}");
}

/// gpui keeps an element's last bounds after it goes, so the version's
/// absence shows as Restart to update ending at the footer's right padding
/// (12 px) with nothing drawn after it.
#[gpui::test]
fn the_version_gives_way_to_the_update_notice(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let updater = Arc::new(UpdaterRecorder::default());
    let mut h = opened(cx, &dir, &updater);
    h.cx.simulate_resize(gpui::size(px(1200.0), px(700.0)));
    h.cx.run_until_parked();
    assert_eq!(footer(&mut h), None, "no update at launch");
    assert!(
        drawn(&mut h, &version_tag()),
        "the version shows with no update"
    );

    updater.offer("0.99.0", &installer(&dir));
    press_in_settings(&mut h, "settings-general-check-now");
    h.keys("escape");
    assert_eq!(footer(&mut h).as_deref(), Some("Update 0.99.0 ready"));

    let width = h.cx.update(|window, _| window.viewport_size().width);
    let button = h.bounds("footer-update");
    assert!(
        width - button.right() <= px(20.0),
        "Restart to update is the footer's last item: {button:?} in {width:?}"
    );
}

#[gpui::test]
fn the_status_text_yields_before_the_version(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let missing = format!("gone{}", "-and-then-some-more".repeat(30));
    let updater = Arc::new(UpdaterRecorder::default());
    let mut h = Harness::open_with_updater(cx, &dir, Some(&missing), updater);
    h.load(&Fixture::single(session("s1").build()));
    h.cx.simulate_resize(gpui::size(px(640.0), px(480.0)));
    let width = h.cx.update(|window, _| window.viewport_size().width);

    let version = h.bounds(&version_tag());
    let status = h.bounds("footer-status");
    assert!(
        version.origin.x >= px(0.0) && version.right() <= width,
        "the version stays inside the window: {version:?} in {width:?}"
    );
    assert!(
        status.right() <= version.origin.x,
        "the status text ends before the version starts: {status:?} then {version:?}"
    );
}

#[gpui::test]
fn settings_shows_the_version_and_last_check(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    turned_off(&dir);
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
