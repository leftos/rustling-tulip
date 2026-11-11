//! Safe-start specs: a start after a crashed one holds the tabs This PC sends
//! before any is drawn and asks whether to open them or start without them;
//! starting without them closes them and keeps a dated copy. A normal start
//! does none of it, and bad saved shapes still draw. Each spec runs in a
//! process of its own, so the start guard's statics start fresh.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::TestAppContext;
use protocol::{ClientMessage, DaemonMessage, GridNode, SplitDirection, TabEntry};
use rustling_tulip_native::start_guard::{
    GUARD_FILE, LAYOUT_COPY_PREFIX, PROMPT_TITLE, STABLE_AFTER, STARTED_WITHOUT_TITLE, StartMode,
    UI_RESET_TITLE, begin, is_stable, record_panic, stable_without_tabs,
};
use rustling_tulip_native::target::Target;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use support::{Fixture, Harness, REMOTE, TestDir, UpdaterRecorder, pane, session, tab};

fn kind_tab(id: &str, kind: &str) -> TabEntry {
    serde_json::from_value(json!({
        "id": id,
        "name": id,
        "content": { "kind": kind },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("tab fixture")
}

fn two_tabs() -> Vec<TabEntry> {
    vec![tab("t1", &pane("p1", None)), kind_tab("d1", "dashboard")]
}

fn closed(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::CloseTab { tab_id } => Some(tab_id.clone()),
            _ => None,
        })
        .collect()
}

fn list_requests(sent: &[ClientMessage]) -> usize {
    sent.iter()
        .filter(|msg| matches!(msg, ClientMessage::ListTabs))
        .count()
}

fn asking_start<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::open_in_mode(cx, dir, StartMode::Ask);
    h.sent();
    h
}

fn prompt_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.start_prompt_open())
}

fn tab_ids(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.tab_ids())
}

/// Whether the element `selector` tags has been painted with a size.
fn painted(h: &mut Harness<'_>, selector: &str) -> bool {
    let bounds = h.bounds(selector);
    bounds.size.width > gpui::px(0.0) && bounds.size.height > gpui::px(0.0)
}

/// The title and detail of every toast shown, oldest first.
fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.title.clone(), toast.detail.clone()))
            .collect()
    })
}

/// Closes the window, which quits at once when no session is running.
fn quit(h: &mut Harness<'_>) {
    let _ = h.cx.simulate_close();
    h.cx.run_until_parked();
}

fn layout_copy_paths(dir: &TestDir) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir.path())
        .expect("list the config dir")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(LAYOUT_COPY_PREFIX))
        })
        .collect()
}

fn layout_copies(dir: &TestDir) -> Vec<String> {
    layout_copy_paths(dir)
        .into_iter()
        .map(|path| std::fs::read_to_string(path).expect("read a copy"))
        .collect()
}

/// Asserts the tabs `ids` were closed as a start without tabs closes them,
/// with one copy of the layout naming each, and the prompt is gone.
fn assert_started_without(h: &mut Harness<'_>, dir: &TestDir, ids: &[&str]) {
    assert_eq!(closed(&h.sent()), ids);
    let copies = layout_copies(dir);
    assert_eq!(copies.len(), 1, "{copies:?}");
    for id in ids {
        assert!(copies[0].contains(&format!("\"{id}\"")), "{copies:?}");
    }
    assert!(!prompt_open(h), "the prompt closes");
    assert_eq!(tab_ids(h), Vec::<String>::new());
}

#[gpui::test]
fn a_normal_start_keeps_the_tabs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    h.sent();

    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    assert_eq!(tab_ids(&mut h), ["t1", "d1"]);
    assert_eq!(closed(&h.sent()), Vec::<String>::new());
    assert!(!prompt_open(&mut h));
}

#[gpui::test]
fn a_crashed_start_asks_and_draws_no_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);

    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    assert_eq!(tab_ids(&mut h), Vec::<String>::new());
    assert_eq!(closed(&h.sent()), Vec::<String>::new());
    assert!(prompt_open(&mut h));
    assert!(painted(&mut h, "start-prompt-open"));
    assert!(painted(&mut h, "start-prompt-without"));
    assert_eq!(PROMPT_TITLE, "The last start crashed");
    let lines = h
        .root(|root, _| root.start_prompt_lines())
        .expect("the prompt is up");
    assert_eq!(
        lines,
        [
            "Open your tabs as they were, or start without them? Your sessions keep running \
          either way, and starting without them keeps a copy of the layout."
        ]
    );
    assert_eq!(layout_copies(&dir), Vec::<String>::new());
}

#[gpui::test]
fn a_reset_start_says_the_settings_went_back(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let kept = dir.path().join("native-ui.json.corrupt-x");
    let mut h = Harness::open_in_mode(
        cx,
        &dir,
        StartMode::AskAndResetUi {
            kept_ui: Some(kept.clone()),
        },
    );

    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    let lines = h
        .root(|root, _| root.start_prompt_lines())
        .expect("the prompt is up");
    assert_eq!(
        lines[1..],
        [format!(
            "Your window and sidebar settings went back to their defaults too. A copy is in {}.",
            kept.display()
        )]
    );
}

#[gpui::test]
fn open_tabs_lists_them_again_and_draws_them(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    h.click_on("start-prompt-open");

    let sent = h.sent();
    assert_eq!(list_requests(&sent), 1, "{sent:?}");
    assert!(!prompt_open(&mut h), "the prompt closes");
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    assert_eq!(tab_ids(&mut h), ["t1", "d1"]);
    assert_eq!(closed(&sent), Vec::<String>::new());
    assert_eq!(closed(&h.sent()), Vec::<String>::new());
    assert_eq!(layout_copies(&dir), Vec::<String>::new());
    assert_eq!(toasts(&mut h), Vec::new());
}

#[gpui::test]
fn start_without_tabs_closes_them_and_keeps_a_copy(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    h.click_on("start-prompt-without");

    assert_started_without(&mut h, &dir, &["t1", "d1"]);
    let copy = layout_copy_paths(&dir).remove(0);
    assert_eq!(STARTED_WITHOUT_TITLE, "Started without your tabs");
    assert_eq!(
        toasts(&mut h),
        [(
            "Started without your tabs".to_owned(),
            Some(format!("A copy of the layout is in {}.", copy.display()))
        )]
    );
}

#[gpui::test]
fn enter_starts_without_tabs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    h.keys("enter");

    assert_started_without(&mut h, &dir, &["t1", "d1"]);
}

#[gpui::test]
fn esc_starts_without_tabs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    h.keys("escape");

    assert_started_without(&mut h, &dir, &["t1", "d1"]);
}

#[gpui::test]
fn other_keys_do_nothing_while_the_prompt_is_up(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    h.keys("a");

    assert!(prompt_open(&mut h));
    assert_eq!(closed(&h.sent()), Vec::<String>::new());
}

#[gpui::test]
fn tab_messages_wait_while_the_prompt_is_up(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t2", &pane("p2", None)), tab("t3", &pane("p3", None))],
    });
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t2", &pane("p2", None)),
    });
    h.send(DaemonMessage::TabsReordered {
        ordered_ids: vec!["t3".to_owned(), "t2".to_owned()],
    });

    assert_eq!(tab_ids(&mut h), Vec::<String>::new());
    assert!(prompt_open(&mut h));
    assert_eq!(closed(&h.sent()), Vec::<String>::new());

    h.click_on("start-prompt-without");

    assert_started_without(&mut h, &dir, &["t2", "t3"]);
}

#[gpui::test]
fn a_crash_with_no_tabs_asks_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);

    h.send(DaemonMessage::LayoutInitRequired {
        has_legacy: false,
        active_session_count: 0,
        clonable: Vec::new(),
    });

    assert!(!prompt_open(&mut h));
    assert_eq!(layout_copies(&dir), Vec::<String>::new());
    assert_eq!(toasts(&mut h), Vec::new());
}

#[gpui::test]
fn a_crash_with_an_empty_tab_list_asks_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);

    h.send(DaemonMessage::Tabs { tabs: Vec::new() });

    assert!(!prompt_open(&mut h));
    assert_eq!(layout_copies(&dir), Vec::<String>::new());
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    assert_eq!(tab_ids(&mut h), ["t1", "d1"], "a later list draws");
}

#[gpui::test]
fn a_reset_with_no_tabs_says_the_settings_were_reset(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let kept = dir.path().join("native-ui.json.corrupt-x");
    let mut h = Harness::open_in_mode(
        cx,
        &dir,
        StartMode::AskAndResetUi {
            kept_ui: Some(kept.clone()),
        },
    );

    h.send(DaemonMessage::Tabs { tabs: Vec::new() });

    assert!(!prompt_open(&mut h));
    assert_eq!(UI_RESET_TITLE, "Window and sidebar settings were reset");
    assert_eq!(
        toasts(&mut h),
        [(
            "Window and sidebar settings were reset".to_owned(),
            Some(format!(
                "The last starts crashed. A copy of your settings is in {}.",
                kept.display()
            ))
        )]
    );
}

#[gpui::test]
fn the_removed_tabs_replies_change_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    h.click_on("start-prompt-without");

    for tab_id in ["t1", "d1"] {
        h.send(DaemonMessage::TabRemoved {
            tab_id: tab_id.to_owned(),
        });
    }

    assert_eq!(tab_ids(&mut h), Vec::<String>::new());
    assert!(!prompt_open(&mut h));
}

#[gpui::test]
fn a_later_tab_list_is_used_as_normal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    h.click_on("start-prompt-without");
    h.sent();

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t2", &pane("p2", None))],
    });

    assert_eq!(tab_ids(&mut h), ["t2"]);
    assert_eq!(closed(&h.sent()), Vec::<String>::new());
}

#[gpui::test]
fn a_remote_hosts_tab_list_does_not_ask(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);
    let remote = Target::tailnet(REMOTE, None);
    {
        let mut host = h.host(&remote);
        host.connect();
        host.sent();
        host.send(DaemonMessage::Tabs { tabs: two_tabs() });
        assert_eq!(closed(&host.sent()), Vec::<String>::new());
    }
    assert_eq!(closed(&h.sent()), Vec::<String>::new());
    assert!(!prompt_open(&mut h));

    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    assert!(prompt_open(&mut h));
}

#[gpui::test]
fn a_reconnects_tab_list_draws_nothing_the_start_closed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    h.click_on("start-prompt-without");
    assert_eq!(closed(&h.sent()), ["t1", "d1"]);

    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    assert_eq!(tab_ids(&mut h), Vec::<String>::new());
    assert_eq!(closed(&h.sent()), ["t1", "d1"], "the tabs are closed again");
}

#[gpui::test]
fn a_tab_update_in_flight_draws_nothing_the_start_closed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    h.click_on("start-prompt-without");

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p1", None)),
    });

    assert_eq!(tab_ids(&mut h), Vec::<String>::new());
}

#[gpui::test]
fn a_new_tab_after_starting_without_tabs_draws(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    h.click_on("start-prompt-without");

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t2", &pane("p2", None))],
    });

    assert_eq!(tab_ids(&mut h), ["t2"]);
}

#[gpui::test]
fn a_reconnects_tab_list_keeps_a_stable_run_stable(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    h.advance(STABLE_AFTER);
    assert!(is_stable());

    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    assert!(is_stable(), "a reconnect does not re-arm the start window");
}

#[gpui::test]
fn a_remote_tab_list_does_not_rearm_the_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    stable_without_tabs();
    assert!(is_stable());

    let remote = Target::tailnet(REMOTE, None);
    let mut host = h.host(&remote);
    host.connect();
    host.send(DaemonMessage::Tabs { tabs: two_tabs() });

    assert!(is_stable(), "a remote host's list leaves the window alone");
}

fn seed_guard(dir: &TestDir) -> std::path::PathBuf {
    let guard = dir.path().join(GUARD_FILE);
    std::fs::write(&guard, r#"{"failed_starts":2,"last_panic":"x"}"#).expect("seed the guard");
    guard
}

fn saved_count(guard: &std::path::Path) -> Value {
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(guard).expect("the guard stays"))
            .expect("valid JSON");
    saved["failed_starts"].clone()
}

#[gpui::test]
fn quitting_before_the_first_tab_list_keeps_the_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let guard = seed_guard(&dir);
    let mut h = Harness::open_in_mode(cx, &dir, StartMode::Ask);

    quit(&mut h);

    assert_eq!(h.quit_requests(), 1, "the close quits at once");
    assert_eq!(saved_count(&guard), json!(2), "the count stays");
}

#[gpui::test]
fn quitting_while_the_prompt_is_up_keeps_the_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let guard = seed_guard(&dir);
    let mut h = Harness::open_in_mode(cx, &dir, StartMode::Ask);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    quit(&mut h);

    assert_eq!(h.quit_requests(), 1, "the close quits at once");
    assert_eq!(saved_count(&guard), json!(2), "nothing was chosen yet");
}

#[gpui::test]
fn quitting_after_starting_without_tabs_forgets_the_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let guard = seed_guard(&dir);
    let mut h = Harness::open_in_mode(cx, &dir, StartMode::Ask);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    h.click_on("start-prompt-without");

    quit(&mut h);

    assert_eq!(h.quit_requests(), 1, "the close quits at once");
    assert!(!guard.exists(), "the start was chosen, so the count goes");
}

#[gpui::test]
fn open_tabs_keeps_the_count_until_the_run_is_stable(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let guard = seed_guard(&dir);
    let mut h = Harness::open_in_mode(cx, &dir, StartMode::Ask);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    h.click_on("start-prompt-open");
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    assert_eq!(saved_count(&guard), json!(2), "a crash now still counts");
    assert!(!is_stable());
}

fn seed_one_crash(dir: &TestDir) -> std::path::PathBuf {
    let guard = dir.path().join(GUARD_FILE);
    std::fs::write(&guard, r#"{"failed_starts":1,"last_panic":"x"}"#).expect("seed the guard");
    guard
}

#[gpui::test]
fn a_prompt_left_up_past_the_window_keeps_the_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let guard = seed_one_crash(&dir);
    let mut h = Harness::open_in_mode(cx, &dir, StartMode::Ask);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    h.advance(STABLE_AFTER + Duration::from_secs(1));
    h.click_on("start-prompt-open");

    assert_eq!(
        saved_count(&guard),
        json!(1),
        "the prompt marked nothing stable"
    );
    assert!(!is_stable());
    assert_eq!(
        record_panic(dir.path(), "after a late open"),
        2,
        "a crash after the choice counts"
    );
}

#[gpui::test]
fn the_stable_window_starts_at_the_choice(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let guard = seed_one_crash(&dir);
    let mut h = Harness::open_in_mode(cx, &dir, StartMode::Ask);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    h.advance(STABLE_AFTER + Duration::from_secs(1));
    h.click_on("start-prompt-open");
    assert!(guard.exists(), "the window has only begun");

    h.advance(STABLE_AFTER + Duration::from_secs(1));

    assert_eq!(
        saved_guard(&guard),
        stable_guard(),
        "the start is stable once its window passed"
    );
    assert!(is_stable());
}

#[gpui::test]
fn open_tabs_keeps_the_saved_active_tab_and_views(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        json!({
            "active_tab_id": "d1",
            "dashboard": { "view": "by_status" },
        })
        .to_string(),
    )
    .expect("seed native-ui.json");
    let mut h = asking_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    h.click_on("start-prompt-open");
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    assert_eq!(tab_ids(&mut h), ["t1", "d1"]);
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("d1".to_owned()),
        "the saved active tab comes back"
    );
    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("native-ui.json")).expect("read the file"),
    )
    .expect("valid JSON");
    assert_eq!(
        saved["dashboard"]["view"],
        json!("by_status"),
        "nothing reset it"
    );
}

/// A start after a crash, with a pop-out of `s2` saved and both sessions
/// and one tab loaded behind the prompt.
fn popout_start<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> (Harness<'a>, Fixture) {
    std::fs::write(
        dir.path().join("native-ui.json"),
        json!({ "popouts": [{ "key": { "kind": "session", "session_id": "s2" } }] }).to_string(),
    )
    .expect("seed native-ui.json");
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    let mut h = asking_start(cx, dir);
    h.load(&fixture);
    (h, fixture)
}

#[gpui::test]
fn open_tabs_brings_the_saved_popout_back(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, fixture) = popout_start(cx, &dir);
    assert_eq!(h.window_count(), 1, "no pop-out opens behind the prompt");

    h.click_on("start-prompt-open");
    h.send(DaemonMessage::Tabs {
        tabs: fixture.tabs.clone(),
    });

    assert_eq!(h.window_count(), 2, "the saved pop-out reopened");
}

#[gpui::test]
fn start_without_tabs_opens_no_saved_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, fixture) = popout_start(cx, &dir);

    h.click_on("start-prompt-without");
    h.send(DaemonMessage::Tabs {
        tabs: fixture.tabs.clone(),
    });

    assert_eq!(h.window_count(), 1, "the saved pop-out stays closed");
    assert_eq!(h.root(|root, _| root.popout_count()), 0);
}

#[gpui::test]
fn start_without_tabs_relists_the_tabs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    h.click_on("start-prompt-without");

    let sent = h.sent();
    let last_close = sent
        .iter()
        .rposition(|msg| matches!(msg, ClientMessage::CloseTab { .. }))
        .expect("the tabs were closed");
    let relist = sent
        .iter()
        .position(|msg| matches!(msg, ClientMessage::ListTabs))
        .expect("the tabs are listed again");
    assert!(relist > last_close, "{sent:?}");
    assert_eq!(list_requests(&sent), 1, "{sent:?}");
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &pane("p1", None)), tab("t9", &pane("p9", None))],
    });
    assert_eq!(tab_ids(&mut h), ["t9"], "a tab made meanwhile draws");
}

#[gpui::test]
fn a_crashed_start_drops_the_restored_views(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        json!({
            "active_tab_id": "t1",
            "popouts": [{
                "key": { "kind": "session", "session_id": "s1", "host": REMOTE },
                "window": null,
            }],
            "dashboard": { "view": "by_status" },
            "ui_font_family": "Fira Sans",
        })
        .to_string(),
    )
    .expect("seed native-ui.json");
    let mut h = asking_start(cx, &dir);

    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    h.click_on("start-prompt-without");

    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("native-ui.json")).expect("read the file"),
    )
    .expect("valid JSON");
    assert_eq!(saved["active_tab_id"], Value::Null);
    assert_eq!(saved["popouts"], json!([]));
    assert_ne!(saved["dashboard"]["view"], json!("by_status"));
    assert_eq!(saved["ui_font_family"], json!("Fira Sans"));
    assert_eq!(h.root(|root, _| root.popout_count()), 0);
}

#[gpui::test]
fn the_start_is_stable_after_the_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let guard = dir.path().join(GUARD_FILE);
    std::fs::write(&guard, r#"{"failed_starts":0,"last_panic":null}"#).expect("seed the guard");
    let mut h = Harness::open(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    assert!(guard.exists(), "still there before the window has passed");

    h.advance(STABLE_AFTER);

    assert_eq!(
        saved_guard(&guard),
        stable_guard(),
        "marked stable once the window has passed"
    );
}

fn saved_guard(guard: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(guard).expect("the guard stays"))
        .expect("valid JSON")
}

/// The guard of a run of this process that reached stable.
fn stable_guard() -> Value {
    json!({
        "failed_starts": 0,
        "last_panic": null,
        "ui_set_aside": false,
        "run": { "state": "stable", "pid": std::process::id() },
    })
}

/// One working session in one tab, so a close asks what to do with it.
fn working_session() -> Fixture {
    Fixture {
        sessions: vec![session("s1").in_repo("r1").status("working").build()],
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
        ..Fixture::default()
    }
}

#[gpui::test]
fn a_quit_through_the_exit_dialog_clears_the_run_marker(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let guard = dir.path().join(GUARD_FILE);
    assert_eq!(begin(dir.path()), StartMode::Normal);
    assert!(guard.exists(), "the start marked its run");
    let mut h = Harness::with(cx, &dir, &working_session());
    h.sent();
    let _ = h.cx.simulate_close();
    h.cx.run_until_parked();
    assert!(h.root(|root, _| root.exit_dialog_open()), "the close asks");

    h.click_on("exit-keep-running");

    assert_eq!(h.quit_requests(), 1);
    assert!(!guard.exists(), "a clean quit leaves nothing to ask about");
}

#[gpui::test]
fn restart_to_update_clears_the_run_marker(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let guard = dir.path().join(GUARD_FILE);
    let installer = dir.path().join("rustling-tulip-0.99.0-setup.exe");
    std::fs::write(&installer, b"stand-in installer").expect("write the stand-in installer");
    let updater = Arc::new(UpdaterRecorder::default());
    updater.offer("0.99.0", &installer);
    assert_eq!(begin(dir.path()), StartMode::Normal);
    let mut h = Harness::open_with_updater(cx, &dir, None, Arc::clone(&updater));
    h.load(&working_session());
    h.answer_scrollback("s1", b"");
    h.cx.run_until_parked();

    h.click_on("footer-update");

    assert_eq!(updater.installs(), [installer]);
    assert_eq!(h.quit_requests(), 1, "the client quits");
    assert!(!guard.exists(), "an update's quit is a clean end");
}

#[gpui::test]
fn quitting_with_the_prompt_up_keeps_the_run_marker(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let guard = seed_guard(&dir);
    let mode = begin(dir.path());
    assert_eq!(mode, StartMode::Ask);
    let mut h = Harness::open_in_mode(cx, &dir, mode);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    assert!(prompt_open(&mut h));

    quit(&mut h);

    assert_eq!(h.quit_requests(), 1, "the close quits at once");
    assert_eq!(
        saved_guard(&guard),
        json!({
            "failed_starts": 2,
            "last_panic": "x",
            "ui_set_aside": false,
            "run": { "state": "asking", "pid": std::process::id() },
        }),
        "the next start asks again, counting nothing"
    );
}

#[gpui::test]
fn a_start_after_a_stable_run_was_killed_asks(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let guard = dir.path().join(GUARD_FILE);
    std::fs::write(
        &guard,
        json!({
            "failed_starts": 0,
            "last_panic": null,
            "ui_set_aside": false,
            "run": { "state": "stable", "pid": u32::MAX },
        })
        .to_string(),
    )
    .expect("seed the guard");

    let mode = begin(dir.path());
    assert_eq!(mode, StartMode::Ask);
    let mut h = Harness::open_in_mode(cx, &dir, mode);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    assert!(prompt_open(&mut h), "the start asks about the tabs");
    assert_eq!(
        saved_count(&guard),
        json!(0),
        "a stable run's kill counts nothing"
    );
}

fn ratio_split(ratio: f32, first: GridNode, second: GridNode) -> GridNode {
    GridNode::Split {
        direction: SplitDirection::Horizontal,
        ratio,
        first: Box::new(first),
        second: Box::new(second),
    }
}

#[gpui::test]
fn bad_ratios_still_draw(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    let bad_ratios = tab(
        "ratios",
        &ratio_split(
            0.0,
            ratio_split(-1.0, pane("a", None), pane("b", None)),
            ratio_split(7.0, pane("c", None), pane("d", None)),
        ),
    );

    // Each draws in turn as the active tab: the first of a list is active.
    h.send(DaemonMessage::Tabs {
        tabs: vec![
            kind_tab("future", "a_kind_from_a_later_release"),
            bad_ratios.clone(),
        ],
    });
    h.bounds("start-prompt-open");
    assert_eq!(tab_ids(&mut h), ["future", "ratios"]);

    h.send(DaemonMessage::Tabs {
        tabs: vec![bad_ratios],
    });
    h.bounds("start-prompt-open");
    assert_eq!(tab_ids(&mut h), ["ratios"]);
}
