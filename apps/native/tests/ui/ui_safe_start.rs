//! Safe-start specs: a start after a panicked one closes the tabs This PC
//! sends before any is drawn, keeps a dated copy of them and says so; a normal
//! start does none of it, and bad saved shapes still draw. Each spec runs in a
//! process of its own, so the start guard's statics start fresh.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::TestAppContext;
use protocol::{ClientMessage, DaemonMessage, GridNode, SplitDirection, TabEntry};
use rustling_tulip_native::start_guard::{
    GUARD_FILE, LAYOUT_COPY_PREFIX, NOTICE_TITLE, STABLE_AFTER, StartMode,
};
use rustling_tulip_native::target::Target;
use serde_json::{Value, json};
use support::{Harness, REMOTE, TestDir, pane, tab};

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

fn safe_start<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::open_in_mode(cx, dir, StartMode::CloseTabs);
    h.sent();
    h
}

fn notice_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.start_notice_open())
}

fn tab_ids(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.tab_ids())
}

fn layout_copies(dir: &TestDir) -> Vec<String> {
    std::fs::read_dir(dir.path())
        .expect("list the config dir")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(LAYOUT_COPY_PREFIX))
        })
        .map(|path| std::fs::read_to_string(path).expect("read a copy"))
        .collect()
}

#[gpui::test]
fn a_normal_start_keeps_the_tabs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    h.sent();

    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    assert_eq!(tab_ids(&mut h), ["t1", "d1"]);
    assert_eq!(closed(&h.sent()), Vec::<String>::new());
    assert!(!notice_open(&mut h));
}

#[gpui::test]
fn a_safe_start_draws_no_tab_and_closes_them_all(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = safe_start(cx, &dir);

    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    assert_eq!(tab_ids(&mut h), Vec::<String>::new());
    assert_eq!(closed(&h.sent()), ["t1", "d1"]);
    let copies = layout_copies(&dir);
    assert_eq!(copies.len(), 1);
    assert!(copies[0].contains("\"t1\"") && copies[0].contains("\"d1\""));
    assert!(notice_open(&mut h));
    assert_eq!(NOTICE_TITLE, "Opened with your tabs closed");
    let lines = h
        .root(|root, _| root.start_notice_lines())
        .expect("the notice is up");
    assert!(lines.iter().any(|line| line.contains(LAYOUT_COPY_PREFIX)));
}

#[gpui::test]
fn the_removed_tabs_replies_change_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = safe_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    for tab_id in ["t1", "d1"] {
        h.send(DaemonMessage::TabRemoved {
            tab_id: tab_id.to_owned(),
        });
    }

    assert_eq!(tab_ids(&mut h), Vec::<String>::new());
    assert!(notice_open(&mut h));
}

#[gpui::test]
fn a_later_tab_list_is_used_as_normal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = safe_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    h.sent();

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t2", &pane("p2", None))],
    });

    assert_eq!(tab_ids(&mut h), ["t2"]);
    assert_eq!(closed(&h.sent()), Vec::<String>::new());
}

#[gpui::test]
fn a_safe_start_with_no_layout_says_so_with_no_copy(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = safe_start(cx, &dir);

    h.send(DaemonMessage::LayoutInitRequired {
        has_legacy: false,
        active_session_count: 0,
        clonable: Vec::new(),
    });

    let lines = h
        .root(|root, _| root.start_notice_lines())
        .expect("the notice is up");
    assert!(
        !lines
            .iter()
            .any(|line| line.contains("A copy of the old layout")),
        "{lines:?}"
    );
    assert_eq!(layout_copies(&dir), Vec::<String>::new());
}

#[gpui::test]
fn a_remote_hosts_tab_list_does_not_end_the_safe_start(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = safe_start(cx, &dir);
    let remote = Target::tailnet(REMOTE, None);
    {
        let mut host = h.host(&remote);
        host.connect();
        host.sent();
        host.send(DaemonMessage::Tabs { tabs: two_tabs() });
        assert_eq!(closed(&host.sent()), Vec::<String>::new());
    }
    assert_eq!(closed(&h.sent()), Vec::<String>::new());
    assert!(!notice_open(&mut h));

    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

    assert_eq!(closed(&h.sent()), ["t1", "d1"]);
    assert!(notice_open(&mut h));
}

#[gpui::test]
fn a_safe_start_drops_the_restored_views(cx: &mut TestAppContext) {
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
    let mut h = safe_start(cx, &dir);

    h.send(DaemonMessage::Tabs { tabs: two_tabs() });

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
fn the_notice_goes_on_ok(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = safe_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    assert!(notice_open(&mut h));

    h.click_on("start-notice-ok");
    assert!(!notice_open(&mut h), "OK closes it");
}

#[gpui::test]
fn the_notice_goes_on_esc(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = safe_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    assert!(notice_open(&mut h));

    h.keys("escape");

    assert!(!notice_open(&mut h), "Esc closes it");
}

#[gpui::test]
fn the_notice_goes_on_enter(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = safe_start(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    assert!(notice_open(&mut h));

    h.keys("enter");

    assert!(!notice_open(&mut h), "Enter closes it");
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

    assert!(!guard.exists(), "gone once the window has passed");
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
    h.bounds("start-notice-ok");
    assert_eq!(tab_ids(&mut h), ["future", "ratios"]);

    h.send(DaemonMessage::Tabs {
        tabs: vec![bad_ratios],
    });
    h.bounds("start-notice-ok");
    assert_eq!(tab_ids(&mut h), ["ratios"]);
}
