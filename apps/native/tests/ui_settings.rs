//! Settings specs: the tab list and its keys, the General tab's keep-awake
//! and copy-on-select rows, the App title tab's toggles and preview, and the
//! window title they shape.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::time::Duration;

use gpui::{Modifiers, Point, TestAppContext, point, px};
use protocol::{ClientMessage, DaemonMessage, SplitDirection, TabEntry};
use rustling_tulip_native::SidebarView;
use support::{Fixture, Harness, TestDir, pane, session, split, tab};

const TAB_SELECTORS: [&str; 6] = [
    "settings-tab-general",
    "settings-tab-notifications",
    "settings-tab-spawn-defaults",
    "settings-tab-worktrees",
    "settings-tab-appearance",
    "settings-tab-app-title",
];

/// Tab `t1`, `work`, active: pane `p1` shows the working session `s1`, pane
/// `p2` the idle `s2`. Tab `t2`, `other`, holds the empty pane `p3`.
fn two_panes() -> Fixture {
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s2")),
    );
    Fixture {
        sessions: vec![
            session("s1").status("working").build(),
            session("s2").build(),
        ],
        tabs: vec![
            named("t1", "work", &grid),
            named("t2", "other", &pane("p3", None)),
        ],
        ..Fixture::default()
    }
}

/// Tab `id` named `name`.
fn named(id: &str, name: &str, grid: &protocol::GridNode) -> TabEntry {
    let mut entry = tab(id, grid);
    name.clone_into(&mut entry.name);
    entry
}

/// Which part of the Settings frame has the keyboard.
fn settings_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).settings_focus(window))
}

fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
}

/// The harness on [`two_panes`], every scrollback answered, and the title
/// settled.
fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &two_panes());
    h.answer_scrollback("s1", b"");
    h.answer_scrollback("s2", b"");
    h.advance(Duration::from_millis(400));
    h.sent();
    h
}

fn title(h: &mut Harness<'_>) -> String {
    h.root(|root, _| root.window_title().to_owned())
}

fn shown_tab(h: &mut Harness<'_>) -> &'static str {
    h.root(|root, _| root.settings_tab())
}

fn saved_ui(dir: &TestDir) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("a saved layout");
    serde_json::from_str(&text).expect("native-ui.json is JSON")
}

/// A point in pane `p1`'s cell (`col`, 0), `dx` pixels off its centre.
fn near(h: &mut Harness<'_>, col: usize, dx: f32) -> Point<gpui::Pixels> {
    let at = h.cell_center("p1", col, 0);
    point(at.x + px(dx), at.y)
}

#[gpui::test]
fn settings_opens_on_general_with_tab_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    assert!(h.root(|root, _| root.settings_open()));
    assert_eq!(shown_tab(&mut h), "General");
    assert_eq!(
        h.root(|root, _| root.settings_tab_labels()),
        [
            "General",
            "Notifications",
            "Spawn defaults",
            "Worktrees",
            "Appearance",
            "App title"
        ]
    );
    let tops: Vec<f32> = TAB_SELECTORS
        .iter()
        .map(|selector| {
            let bounds = h.bounds(selector);
            assert!(bounds.origin.x >= px(0.0), "{selector} is painted");
            bounds.origin.y / px(1.0)
        })
        .collect();
    assert!(
        tops.windows(2).all(|pair| pair[0] < pair[1]),
        "painted top to bottom in order: {tops:?}"
    );
    assert!(h.bounds("settings-keep-awake-toggle").origin.x >= px(0.0));

    h.click_on("settings-tab-worktrees");
    assert_eq!(shown_tab(&mut h), "Worktrees");
    h.keys("escape");
    h.keys("ctrl-,");
    assert_eq!(shown_tab(&mut h), "General", "it opens on General again");
}

#[gpui::test]
fn down_moves_to_next_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    assert_eq!(settings_focus(&mut h), Some("list"), "it opens on the list");
    h.keys("down");
    assert_eq!(shown_tab(&mut h), "Notifications");
    h.keys("down down down down");
    assert_eq!(shown_tab(&mut h), "App title");
    h.keys("down");
    assert_eq!(shown_tab(&mut h), "App title", "Down stops at the last tab");
    h.keys("up");
    assert_eq!(shown_tab(&mut h), "Appearance");
    h.keys("up up up up up");
    assert_eq!(shown_tab(&mut h), "General", "Up stops at the first tab");
}

#[gpui::test]
fn keep_awake_button_sends_toggle(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(DaemonMessage::KeepAwakeStatus {
        enabled: false,
        active: false,
    });
    h.keys("ctrl-,");
    assert_eq!(
        h.root(|root, _| root.keep_awake_button()),
        ("Keep this machine awake while sessions run", true)
    );
    h.click_on("settings-keep-awake-toggle");
    let toggles: Vec<bool> = h
        .sent()
        .iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetKeepAwake { enabled } => Some(*enabled),
            _ => None,
        })
        .collect();
    assert_eq!(toggles, [true]);

    h.send(DaemonMessage::KeepAwakeStatus {
        enabled: true,
        active: false,
    });
    assert_eq!(
        h.root(|root, _| root.keep_awake_button()),
        ("Allow sleep while sessions run", true)
    );
    h.click_on("settings-keep-awake-toggle");
    assert!(
        h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::SetKeepAwake { enabled: false })),
        "pressed again, it allows sleep"
    );
}

#[gpui::test]
fn keep_awake_status_texts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let status = |h: &mut Harness<'_>| h.root(|root, _| root.keep_awake_status());
    assert_eq!(status(&mut h), "(daemon not connected)");
    assert!(!h.root(|root, _| root.keep_awake_button()).1, "disabled");
    h.keys("ctrl-,");
    h.click_on("settings-keep-awake-toggle");
    assert!(
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::SetKeepAwake { .. })),
        "a disabled button sends nothing"
    );

    let cases = [
        (true, true, "Holding the machine awake — a session is live."),
        (true, false, "Sleep is allowed until a session starts."),
        (false, false, "The machine may sleep while sessions run."),
        (false, true, "The machine may sleep while sessions run."),
    ];
    for (enabled, active, text) in cases {
        h.send(DaemonMessage::KeepAwakeStatus { enabled, active });
        assert_eq!(status(&mut h), text, "enabled {enabled}, active {active}");
    }

    h.lose_connection();
    assert_eq!(
        status(&mut h),
        "(daemon not connected)",
        "reset on disconnect"
    );
}

#[gpui::test]
fn copy_on_select_setting_saved_and_honoured(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.pty("s1", b"hello world");
    let (from, to) = (near(&mut h, 0, -2.0), near(&mut h, 4, 2.0));
    h.drag(from, to, [Modifiers::none(); 2]);
    assert_eq!(h.clipboard().as_deref(), Some("hello"), "on by default");

    h.keys("ctrl-,");
    h.click_on("settings-terminal-copy-on-selection");
    assert_eq!(
        saved_ui(&dir)["general"]["copy_on_select"],
        false,
        "saved at once"
    );
    h.keys("escape");

    h.set_clipboard("other");
    let (from, to) = (near(&mut h, 6, -2.0), near(&mut h, 10, 2.0));
    h.drag(from, to, [Modifiers::none(); 2]);
    assert_eq!(
        h.clipboard().as_deref(),
        Some("other"),
        "a selection no longer writes the clipboard"
    );
}

#[gpui::test]
fn app_title_preview_follows_toggles(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-app-title");
    let preview = |h: &mut Harness<'_>| h.root(|root, _| root.title_preview());
    assert_eq!(preview(&mut h), "(1/3) Tab name — rustling-tulip");
    assert!(h.bounds("settings-title-preview").origin.x >= px(0.0));

    h.click_on("settings-title-busy-count");
    assert_eq!(preview(&mut h), "Tab name — rustling-tulip");
    h.click_on("settings-title-product-suffix");
    assert_eq!(preview(&mut h), "Tab name");
    h.click_on("settings-title-busy-count");
    assert_eq!(preview(&mut h), "(1/3) Tab name");

    let saved = saved_ui(&dir);
    assert_eq!(saved["title"]["show_count"], true);
    assert_eq!(saved["title"]["suffix"], false);
}

#[gpui::test]
fn window_title_debounced_350ms(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    assert_eq!(title(&mut h), "(1/2) work — rustling-tulip");

    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s2")),
    );
    h.send(DaemonMessage::TabUpdated {
        tab: named("t1", "build", &grid),
    });
    h.advance(Duration::from_millis(300));
    assert_eq!(title(&mut h), "(1/2) work — rustling-tulip", "not yet");
    h.advance(Duration::from_millis(60));
    assert_eq!(title(&mut h), "(1/2) build — rustling-tulip", "a rename");

    h.send(DaemonMessage::SessionUpdated {
        session: session("s2").status("working").build(),
        request_id: None,
    });
    h.advance(Duration::from_millis(300));
    assert_eq!(title(&mut h), "(1/2) build — rustling-tulip", "not yet");
    h.advance(Duration::from_millis(60));
    assert_eq!(
        title(&mut h),
        "(2/2) build — rustling-tulip",
        "a session working"
    );
}

#[gpui::test]
fn window_title_debounce_restarts_on_second_change(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s2")),
    );
    h.send(DaemonMessage::TabUpdated {
        tab: named("t1", "build", &grid),
    });
    h.advance(Duration::from_millis(200));
    h.click_on("tab-t2");
    h.advance(Duration::from_millis(160));
    assert_eq!(
        title(&mut h),
        "(1/2) work — rustling-tulip",
        "360 ms after the first change, the second restarted the wait"
    );
    h.advance(Duration::from_millis(180));
    assert_eq!(title(&mut h), "(1/2) work — rustling-tulip", "not yet");
    h.advance(Duration::from_millis(20));
    assert_eq!(
        title(&mut h),
        "other — rustling-tulip",
        "350 ms after the second change; the rename in between never showed"
    );
}

#[gpui::test]
fn window_title_follows_a_local_tab_switch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.click_on("tab-t2");
    h.advance(Duration::from_millis(300));
    assert_eq!(title(&mut h), "(1/2) work — rustling-tulip", "not yet");
    h.advance(Duration::from_millis(60));
    assert_eq!(
        title(&mut h),
        "other — rustling-tulip",
        "no daemon message came; the click alone moves the title"
    );
}

#[gpui::test]
fn tab_walks_general_controls_and_space_toggles(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(DaemonMessage::KeepAwakeStatus {
        enabled: false,
        active: false,
    });
    h.keys("ctrl-,");
    assert_eq!(settings_focus(&mut h), Some("list"));

    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-keep-awake-toggle"));
    h.keys("space");
    assert!(
        h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::SetKeepAwake { enabled: true })),
        "Space presses the keep-awake button"
    );

    h.keys("tab tab tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-terminal-copy-on-selection"),
        "past the two Default view choices"
    );
    h.keys("space");
    assert_eq!(saved_ui(&dir)["general"]["copy_on_select"], false);
    h.keys("enter");
    assert_eq!(
        saved_ui(&dir)["general"]["copy_on_select"],
        true,
        "Enter presses it too"
    );

    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("list"), "back to the tab list");
    h.keys("shift-tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-terminal-copy-on-selection"),
        "Shift+Tab walks back"
    );
    h.keys("shift-tab shift-tab shift-tab shift-tab");
    assert_eq!(settings_focus(&mut h), Some("list"));
    assert_eq!(shown_tab(&mut h), "General");

    h.keys("down down down down down");
    assert_eq!(shown_tab(&mut h), "App title");
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-title-busy-count"));
    h.keys("space");
    assert_eq!(saved_ui(&dir)["title"]["show_count"], false);
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-title-product-suffix")
    );
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("list"));
}

#[gpui::test]
fn general_default_view_row_switches_the_open_sidebar_and_saves(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let tags = |h: &mut Harness<'_>| -> Vec<&'static str> {
        h.root(|root, _| {
            root.sidebar_containers()
                .iter()
                .map(|c| c.kind.tag())
                .collect()
        })
    };
    h.keys("ctrl-,");
    assert!(h.bounds("settings-sidebar-view-repos").origin.x >= px(0.0));

    h.click_on("settings-sidebar-view-tabs");
    assert_eq!(saved_ui(&dir)["sidebar_view"], "tabs", "saved at once");
    assert_eq!(
        h.root(|root, _| root.sidebar_view()),
        SidebarView::Tabs,
        "the open sidebar follows"
    );
    assert_eq!(tags(&mut h), ["TAB", "TAB"]);

    h.click_on("settings-sidebar-view-repos");
    assert_eq!(saved_ui(&dir)["sidebar_view"], "repos");
    assert_eq!(h.root(|root, _| root.sidebar_view()), SidebarView::Repos);
    assert!(!tags(&mut h).contains(&"TAB"));
}

#[gpui::test]
fn general_tab_ring_includes_the_default_view_choices(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(DaemonMessage::KeepAwakeStatus {
        enabled: false,
        active: false,
    });
    h.keys("ctrl-,");
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-keep-awake-toggle"));
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-sidebar-view-repos"));
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-sidebar-view-tabs"));
    h.keys("space");
    assert_eq!(saved_ui(&dir)["sidebar_view"], "tabs");
    assert_eq!(h.root(|root, _| root.sidebar_view()), SidebarView::Tabs);
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-terminal-copy-on-selection")
    );
    h.keys("shift-tab shift-tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-sidebar-view-repos"),
        "Shift+Tab walks back"
    );
    h.keys("enter");
    assert_eq!(saved_ui(&dir)["sidebar_view"], "repos");
}

#[gpui::test]
fn spawn_defaults_tab_shows_choices_and_saves(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-spawn-defaults");
    assert_eq!(shown_tab(&mut h), "Spawn defaults");
    for selector in [
        "settings-spawn-trusted",
        "settings-spawn-approval-cli-default",
        "settings-spawn-approval-accept-edits",
        "settings-spawn-codex-sandbox-cli-default",
        "settings-spawn-codex-sandbox-workspace-write",
    ] {
        assert!(
            h.bounds(selector).origin.x >= px(0.0),
            "{selector} is painted"
        );
    }

    h.click_on("settings-spawn-approval-accept-edits");
    assert_eq!(
        saved_ui(&dir)["spawn"]["permission_mode"],
        "accept_edits",
        "saved at once"
    );
    h.click_on("settings-spawn-codex-sandbox-workspace-write");
    assert_eq!(saved_ui(&dir)["spawn"]["codex_sandbox"], "workspace-write");
    h.click_on("settings-spawn-trusted");
    assert_eq!(saved_ui(&dir)["spawn"]["trusted"], true);
    assert_eq!(
        saved_ui(&dir)["spawn"]["permission_mode"],
        "accept_edits",
        "the values stay saved while trusted locks the rows"
    );
}

#[gpui::test]
fn spawn_defaults_trusted_disables_choices_and_leaves_ring(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-spawn-defaults");
    assert!(
        h.bounds("settings-spawn-approval-locked").origin.x < px(0.0),
        "no locked note while trusted is off"
    );

    h.click_on("settings-spawn-trusted");
    assert_eq!(saved_ui(&dir)["spawn"]["trusted"], true);
    for selector in [
        "settings-spawn-approval-locked",
        "settings-spawn-codex-sandbox-locked",
    ] {
        assert!(
            h.bounds(selector).origin.x >= px(0.0),
            "{selector} shows why the choices are dead"
        );
    }

    h.click_on("settings-spawn-approval-accept-edits");
    assert!(
        h.bounds("settings-spawn-approval-accept-edits").origin.x >= px(0.0),
        "the disabled choice is still painted"
    );
    assert!(
        saved_ui(&dir)["spawn"]["permission_mode"].is_null(),
        "a click on it saves nothing"
    );

    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("list"),
        "the locked choices left the ring"
    );
    h.keys("shift-tab");
    assert_eq!(settings_focus(&mut h), Some("settings-spawn-trusted"));
}

#[gpui::test]
fn tab_walks_spawn_default_choices(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-spawn-defaults");
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-spawn-trusted"));
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-spawn-approval-cli-default")
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-spawn-approval-default")
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-spawn-approval-accept-edits")
    );
    h.keys("space");
    assert_eq!(saved_ui(&dir)["spawn"]["permission_mode"], "accept_edits");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-spawn-approval-bypass-permissions")
    );
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-spawn-approval-plan"));
    h.keys("tab tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-spawn-codex-sandbox-read-only")
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-spawn-codex-sandbox-workspace-write")
    );
    h.keys("enter");
    assert_eq!(saved_ui(&dir)["spawn"]["codex_sandbox"], "workspace-write");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-spawn-codex-sandbox-danger-full-access")
    );
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("list"), "back to the tab list");
    h.keys("shift-tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-spawn-codex-sandbox-danger-full-access"),
        "Shift+Tab walks back"
    );
}

#[gpui::test]
fn tab_in_appearance_hex_field_stays_in_appearance(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-appearance");
    h.click_on("appearance-accent-hex");
    h.keys("tab");
    assert_eq!(shown_tab(&mut h), "Appearance");
    type_text(&mut h, "#12AB34");
    assert!(
        h.root(|root, cx| root.appearance_can_apply("accent", cx)),
        "the hex field kept the keyboard"
    );
}

#[gpui::test]
fn window_title_without_suffix_or_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-app-title");
    h.click_on("settings-title-product-suffix");
    h.advance(Duration::from_millis(360));
    assert_eq!(title(&mut h), "(1/2) work");
    h.click_on("settings-title-busy-count");
    h.advance(Duration::from_millis(360));
    assert_eq!(title(&mut h), "work");
    h.click_on("settings-title-product-suffix");
    h.advance(Duration::from_millis(360));
    assert_eq!(title(&mut h), "work — rustling-tulip");
}
