//! Settings specs: the tab list and its keys, the General tab's keep-awake
//! and copy-on-select rows, the App title tab's toggles and preview, and the
//! window title they shape.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::time::Duration;

use gpui::{Modifiers, Point, TestAppContext, point, px};
use protocol::{
    AgentCommandEntry, AgentId, ClientMessage, DaemonMessage, KeepAwakeMode, SplitDirection,
    TabEntry,
};
use rustling_tulip_native::{
    Connection, LaunchCommandRow, NATIVE_PROTOCOL_VERSIONS, NetEvent, SidebarView, fonts,
};
use support::{Fixture, Harness, TestDir, pane, session, split, tab};

const TAB_SELECTORS: [&str; 8] = [
    "settings-tab-general",
    "settings-tab-notifications",
    "settings-tab-spawn-defaults",
    "settings-tab-worktrees",
    "settings-tab-remote-access",
    "settings-tab-appearance",
    "settings-tab-app-title",
    "settings-tab-claude",
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
    h.cx.update(|window, cx| root.read(cx).settings_focus(window, cx))
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
        h.root(|root, _| root.settings_tab_rows()),
        [
            "General",
            "Notifications",
            "Spawn defaults",
            "Worktrees",
            "Remote access",
            "Appearance",
            "App title",
            "Agents",
            "Claude"
        ]
    );
    assert_eq!(
        h.root(|root, _| root.settings_tab_labels()),
        [
            "General",
            "Notifications",
            "Spawn defaults",
            "Worktrees",
            "Remote access",
            "Appearance",
            "App title",
            "Claude"
        ],
        "the tab-only probe leaves the group out"
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
    assert!(h.bounds("settings-keep-awake-when-busy").origin.x >= px(0.0));

    h.click_on("settings-tab-worktrees");
    assert_eq!(shown_tab(&mut h), "Worktrees");
    h.keys("escape");
    h.keys("ctrl-,");
    assert_eq!(shown_tab(&mut h), "General", "it opens on General again");
}

#[gpui::test]
fn settings_takes_the_dialog_frames_sizes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    for selector in TAB_SELECTORS {
        assert_eq!(
            h.bounds(selector).size.height,
            px(34.0),
            "{selector} is a 34 px row"
        );
    }
    let close = h.bounds("settings-close").size;
    assert_eq!(
        (close.width, close.height),
        (px(28.0), px(28.0)),
        "a 28 px close"
    );
    for selector in [
        "settings-keep-awake-always",
        "settings-sidebar-view-repos",
        "settings-leaf-density-compact",
    ] {
        assert_eq!(
            h.bounds(selector).size.height,
            px(32.0),
            "{selector} is a 32 px segment"
        );
    }
    let panel = h.bounds("settings-panel");
    let footer_close = h.bounds("settings-footer-close");
    assert!(
        footer_close.right() < panel.right() && footer_close.bottom() < panel.bottom(),
        "Close sits in the footer: {footer_close:?} in {panel:?}"
    );
}

#[gpui::test]
fn down_moves_to_next_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    assert_eq!(settings_focus(&mut h), Some("list"), "it opens on the list");
    h.keys("down");
    assert_eq!(shown_tab(&mut h), "Notifications");
    h.keys("down down down down down");
    assert_eq!(shown_tab(&mut h), "App title");
    h.keys("down");
    assert_eq!(
        shown_tab(&mut h),
        "Claude",
        "Down skips the Agents group row"
    );
    h.keys("down");
    assert_eq!(shown_tab(&mut h), "Claude", "Down stops at the last tab");
    h.keys("up");
    assert_eq!(shown_tab(&mut h), "App title");
    h.keys("up up up up up up up");
    assert_eq!(shown_tab(&mut h), "General", "Up stops at the first tab");
}

/// The daemon's keep-awake status for `mode`, as it sends it.
fn keep_awake_status(mode: KeepAwakeMode, active: bool) -> DaemonMessage {
    DaemonMessage::KeepAwakeStatus {
        enabled: mode != KeepAwakeMode::Off,
        mode,
        active,
    }
}

#[gpui::test]
fn keep_awake_choice_sends_the_mode(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(keep_awake_status(KeepAwakeMode::Off, false));
    h.keys("ctrl-,");
    assert_eq!(
        h.root(|root, _| root.keep_awake_mode()),
        Some(KeepAwakeMode::Off)
    );
    h.click_on("settings-keep-awake-when-busy");
    let modes: Vec<KeepAwakeMode> = h
        .sent()
        .iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetKeepAwakeMode { mode } => Some(*mode),
            _ => None,
        })
        .collect();
    assert_eq!(modes, [KeepAwakeMode::WhenBusy]);
    assert!(
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::SetKeepAwake { .. })),
        "the client sends only the mode message"
    );

    h.send(keep_awake_status(KeepAwakeMode::WhenBusy, false));
    assert_eq!(
        h.root(|root, _| root.keep_awake_mode()),
        Some(KeepAwakeMode::WhenBusy),
        "the daemon's echo selects the choice"
    );
}

#[gpui::test]
fn keep_awake_status_texts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let status = |h: &mut Harness<'_>| h.root(|root, _| root.keep_awake_status());
    assert_eq!(status(&mut h), "(daemon not connected)");
    assert_eq!(h.root(|root, _| root.keep_awake_mode()), None, "locked");
    h.keys("ctrl-,");
    h.click_on("settings-keep-awake-always");
    assert!(
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::SetKeepAwakeMode { .. })),
        "a locked choice sends nothing"
    );

    let off = "The machine may sleep while sessions run.";
    let cases = [
        (
            KeepAwakeMode::Always,
            true,
            "Holding the machine awake — a session is live.",
        ),
        (
            KeepAwakeMode::Always,
            false,
            "Sleep is allowed until a session starts.",
        ),
        (
            KeepAwakeMode::WhenBusy,
            true,
            "Holding the machine awake — a session is busy.",
        ),
        (
            KeepAwakeMode::WhenBusy,
            false,
            "Sleep is allowed while no session is busy.",
        ),
        (KeepAwakeMode::Off, false, off),
        (KeepAwakeMode::Off, true, off),
    ];
    for (mode, active, text) in cases {
        h.send(keep_awake_status(mode, active));
        assert_eq!(status(&mut h), text, "mode {mode:?}, active {active}");
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
fn the_open_text_externally_row_toggles_and_saves(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    assert!(
        h.bounds("settings-general-open-text-externally").origin.x >= px(0.0),
        "the row is drawn"
    );
    h.click_on("settings-general-open-text-externally");
    assert_eq!(
        saved_ui(&dir)["general"]["open_text_externally"],
        true,
        "saved at once"
    );
    h.click_on("settings-general-open-text-externally");
    assert_eq!(saved_ui(&dir)["general"]["open_text_externally"], false);
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
fn leaf_density_choice_switches_the_leaves_saves_and_is_reached_by_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let leaf_height = |h: &mut Harness<'_>| h.bounds("leaf-s1").size.height;
    assert!(leaf_height(&mut h) > px(26.0), "Comfortable at first");

    h.keys("ctrl-,");
    h.click_on("settings-leaf-density-compact");
    assert_eq!(saved_ui(&dir)["general"]["leaf_density"], "compact");
    assert_eq!(leaf_height(&mut h), px(26.0), "one line");
    h.keys("escape");

    h.keys("ctrl-,");
    assert_eq!(settings_focus(&mut h), Some("list"));
    h.keys("tab tab tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-leaf-density-comfortable"),
        "after the two Default view choices"
    );
    h.keys("space");
    assert_eq!(saved_ui(&dir)["general"]["leaf_density"], "comfortable");
    assert!(leaf_height(&mut h) > px(26.0), "two lines again");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-leaf-density-compact")
    );
}

#[gpui::test]
fn tab_walks_general_controls_and_space_toggles(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(keep_awake_status(KeepAwakeMode::Off, false));
    h.keys("ctrl-,");
    assert_eq!(settings_focus(&mut h), Some("list"));

    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-keep-awake-off"));
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-keep-awake-when-busy")
    );
    h.keys("space");
    assert!(
        h.sent().iter().any(|msg| matches!(
            msg,
            ClientMessage::SetKeepAwakeMode {
                mode: KeepAwakeMode::WhenBusy
            }
        )),
        "Space presses the keep-awake choice"
    );

    h.keys("tab tab tab tab tab tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-general-open-text-externally"),
        "past Always, the two Default view and the two Leaf density choices"
    );
    h.keys("space");
    assert_eq!(saved_ui(&dir)["general"]["open_text_externally"], true);
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-terminal-copy-on-selection")
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
    h.keys("shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-keep-awake-off"),
        "back past every General control to the first keep-awake choice"
    );
    h.keys("shift-tab");
    assert_eq!(settings_focus(&mut h), Some("list"));
    assert_eq!(shown_tab(&mut h), "General");

    h.keys("down down down down down down");
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
    h.send(keep_awake_status(KeepAwakeMode::Off, false));
    h.keys("ctrl-,");
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-keep-awake-off"));
    h.keys("tab tab tab");
    assert_eq!(settings_focus(&mut h), Some("settings-sidebar-view-repos"));
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-sidebar-view-tabs"));
    h.keys("space");
    assert_eq!(saved_ui(&dir)["sidebar_view"], "tabs");
    assert_eq!(h.root(|root, _| root.sidebar_view()), SidebarView::Tabs);
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-leaf-density-comfortable"),
        "the Leaf density choices follow"
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
    assert!(
        h.bounds("settings-spawn-trusted").origin.x >= px(0.0),
        "the trusted launch toggle is painted"
    );
    for selector in [
        "settings-spawn-approval-cli-default",
        "settings-spawn-provider-anthropic",
    ] {
        assert!(
            h.bounds(selector).origin.x < px(0.0),
            "{selector} moved to its agent page"
        );
    }

    h.click_on("settings-tab-claude");
    assert_eq!(shown_tab(&mut h), "Claude");
    for selector in [
        "settings-spawn-approval-cli-default",
        "settings-spawn-approval-accept-edits",
        "settings-spawn-provider-anthropic",
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

    h.click_on("settings-tab-spawn-defaults");
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
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-spawn-trusted"));
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("list"),
        "trusted launch is the page's only control"
    );

    h.click_on("settings-spawn-trusted");
    assert_eq!(saved_ui(&dir)["spawn"]["trusted"], true);

    h.click_on("settings-tab-claude");
    assert!(
        h.bounds("settings-spawn-approval-locked").origin.x >= px(0.0),
        "the approval row says why it is dead"
    );
    assert!(
        h.bounds("settings-spawn-provider-locked").origin.x >= px(0.0),
        "the provider row says why it is dead"
    );
    assert_eq!(
        h.root(|root, _| root.settings_provider_locked_note()),
        "Trusted launch keeps the provider choice in effect; \
         this lock only stops editing it here.",
        "the provider note says the choice stays in effect"
    );
    h.click_on("settings-spawn-approval-accept-edits");
    assert!(
        h.bounds("settings-spawn-approval-accept-edits").origin.x >= px(0.0),
        "the disabled choice is still painted"
    );
    assert!(
        saved_ui(&dir)["spawn"]["permission_mode"].is_null(),
        "a click on it saves nothing"
    );
    h.click_on("settings-spawn-provider-deepseek");
    assert_eq!(
        saved_ui(&dir)["spawn"]["claude_provider"],
        "anthropic",
        "a click on the locked provider saves nothing"
    );
    h.click_on("settings-tab-claude");
    assert_eq!(settings_focus(&mut h), Some("list"));
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("list"),
        "every locked Claude row left the ring"
    );

    h.click_on("settings-tab-spawn-defaults");
    h.click_on("settings-spawn-trusted");
    assert_eq!(saved_ui(&dir)["spawn"]["trusted"], false);
    h.click_on("settings-tab-claude");
    h.click_on("settings-spawn-approval-accept-edits");
    assert_eq!(
        saved_ui(&dir)["spawn"]["permission_mode"],
        "accept_edits",
        "the choice works again"
    );
}

#[gpui::test]
fn claude_provider_default_saves_and_seeds_the_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    tab_to(&mut h, "settings-spawn-provider-deepseek");
    h.keys("enter");
    assert_eq!(saved_ui(&dir)["spawn"]["claude_provider"], "deepseek");
    h.click_on("settings-spawn-provider-anthropic");
    assert_eq!(saved_ui(&dir)["spawn"]["claude_provider"], "anthropic");
    h.click_on("settings-spawn-provider-deepseek");
    assert_eq!(saved_ui(&dir)["spawn"]["claude_provider"], "deepseek");
    h.keys("escape");
    assert!(!h.root(|root, _| root.settings_open()));
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
        Some("list"),
        "the page's one control is done"
    );

    h.send(agent_commands("", "", None));
    h.click_on("settings-tab-claude");
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
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-spawn-provider-anthropic")
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-spawn-provider-deepseek")
    );
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-agent-program"));
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-agent-args"));
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("list"),
        "Extra arguments is the page's last row"
    );
    h.keys("shift-tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-agent-args"),
        "Shift+Tab walks back"
    );
}

const PROGRAM_REFUSAL: &str =
    "The launch command's Program cannot contain a newline or other control character.";
const UNCLOSED_QUOTE: &str = "The extra arguments have an unclosed quote.";
const RESERVED_FLAG: &str =
    "--settings is set by rustling-tulip; remove it from the extra arguments.";
const ARGS_CONTROL: &str =
    "The launch command's Extra arguments cannot contain a newline or other control character.";

/// The daemon's launch commands: Claude's `program` and `args`, and the
/// variable that overrides the program on the host.
fn agent_commands(program: &str, args: &str, program_env: Option<&str>) -> DaemonMessage {
    DaemonMessage::AgentCommands {
        commands: vec![AgentCommandEntry {
            agent: AgentId("claude".to_owned()),
            program: program.to_owned(),
            args: args.to_owned(),
            program_env: program_env.map(str::to_owned),
        }],
    }
}

/// The harness with the daemon's launch command `stored`, on the Claude
/// page, nothing sent yet.
fn claude_page<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    stored: DaemonMessage,
) -> Harness<'a> {
    let mut h = opened(cx, dir);
    h.send(stored);
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    h.sent();
    h
}

/// The `(program, args)` of every launch command the client sent.
fn launch_sets(sent: &[ClientMessage]) -> Vec<(String, String)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetAgentCommand {
                agent,
                program,
                args,
            } if agent.0 == "claude" => Some((program.clone(), args.clone())),
            _ => None,
        })
        .collect()
}

/// Types `text`, a space as the space key.
fn type_line(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text
        .chars()
        .map(|c| {
            if c == ' ' {
                "space".to_owned()
            } else {
                c.to_string()
            }
        })
        .collect();
    h.keys(&keys.join(" "));
}

fn launch_fields(h: &mut Harness<'_>) -> (Option<String>, Option<String>) {
    h.root(|root, _| (root.launch_program(), root.launch_args()))
}

fn refusal(h: &mut Harness<'_>) -> Option<(LaunchCommandRow, String)> {
    h.root(|root, _| root.launch_command_refusal())
}

fn toast_titles(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| toast.title.clone())
            .collect()
    })
}

fn refuse(h: &mut Harness<'_>, message: &str) {
    h.send(DaemonMessage::Error {
        message: message.to_owned(),
        request_id: None,
    });
}

fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

#[gpui::test]
fn launch_command_rows_hidden_until_agent_commands_arrive(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    assert!(
        !drawn(&mut h, "settings-agent-program"),
        "no Program row yet"
    );
    assert!(
        !drawn(&mut h, "settings-agent-args"),
        "no Extra arguments row yet"
    );
    tab_to(&mut h, "settings-spawn-provider-deepseek");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("list"),
        "the ring holds no launch rows before the daemon reports"
    );

    h.send(agent_commands("wrapper", "verbose", None));
    assert!(drawn(&mut h, "settings-agent-program"));
    assert!(drawn(&mut h, "settings-agent-args"));
    assert_eq!(
        launch_fields(&mut h),
        (Some("wrapper".to_owned()), Some("verbose".to_owned())),
        "the untouched rows take the daemon's command"
    );
}

#[gpui::test]
fn launch_command_rows_disabled_without_the_feature(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.welcome_without_features();
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    assert_eq!(
        h.root(|root, _| root.settings_launch_command_tip()),
        Some("Restart the daemon to set the launch command: .\\rt.ps1 restart")
    );
    assert!(
        drawn(&mut h, "settings-agent-program"),
        "Program is painted, dimmed"
    );
    assert!(
        drawn(&mut h, "settings-agent-args"),
        "Extra arguments is painted, dimmed"
    );
    h.click_on("settings-agent-args");
    type_line(&mut h, "verbose");
    h.keys("enter");
    assert_eq!(
        launch_sets(&h.sent()),
        Vec::<(String, String)>::new(),
        "nothing is sent"
    );
    h.click_on("settings-spawn-provider-deepseek");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("list"),
        "Tab skips the disabled launch rows"
    );

    h.welcome_with_features();
    assert_eq!(h.root(|root, _| root.settings_launch_command_tip()), None);
}

#[gpui::test]
fn enter_in_program_sends_both_drafts_trimmed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-program");
    type_line(&mut h, " wrapper ");
    h.click_on("settings-agent-args");
    type_line(&mut h, " verbose ");
    assert_eq!(
        launch_sets(&h.sent()),
        Vec::<(String, String)>::new(),
        "typing sends nothing"
    );
    h.click_on("settings-agent-program");
    h.keys("enter");
    assert_eq!(
        launch_sets(&h.sent()),
        [("wrapper".to_owned(), "verbose".to_owned())]
    );
}

#[gpui::test]
fn enter_in_args_sends_both_drafts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("wrapper", "", None));
    h.click_on("settings-agent-args");
    type_line(&mut h, "verbose");
    h.keys("enter");
    assert_eq!(
        launch_sets(&h.sent()),
        [("wrapper".to_owned(), "verbose".to_owned())],
        "the untyped Program rides along"
    );
}

#[gpui::test]
fn clearing_both_rows_sends_a_blank_pair(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("wrapper", "verbose", None));
    h.click_on("settings-agent-program");
    h.keys("ctrl-a backspace");
    h.click_on("settings-agent-args");
    h.keys("ctrl-a backspace");
    h.keys("enter");
    assert_eq!(
        launch_sets(&h.sent()),
        [(String::new(), String::new())],
        "Enter on empty rows clears the command"
    );
    h.send(agent_commands("", "", None));
    assert!(h.root(|root, _| root.launch_saved()));
    assert_eq!(
        launch_fields(&mut h),
        (Some(String::new()), Some(String::new()))
    );
}

#[gpui::test]
fn saved_shows_after_the_echo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-program");
    type_line(&mut h, "wrapper");
    h.keys("enter");
    assert_eq!(launch_sets(&h.sent()).len(), 1);
    assert!(
        !h.root(|root, _| root.launch_saved()),
        "not Saved before the echo"
    );
    h.send(agent_commands("wrapper", "", None));
    assert!(h.root(|root, _| root.launch_saved()));
    assert!(drawn(&mut h, "settings-agent-saved"));
    type_line(&mut h, "x");
    assert!(
        !h.root(|root, _| root.launch_saved()),
        "an edit since the save hides Saved"
    );
}

#[gpui::test]
fn program_refusal_shows_under_program_and_skips_the_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-program");
    type_line(&mut h, "wrapper");
    h.keys("enter");
    refuse(&mut h, PROGRAM_REFUSAL);
    assert_eq!(
        refusal(&mut h),
        Some((LaunchCommandRow::Program, PROGRAM_REFUSAL.to_owned()))
    );
    assert!(drawn(&mut h, "settings-agent-program-error"));
    assert_eq!(
        toast_titles(&mut h),
        Vec::<String>::new(),
        "the refusal raises no toast"
    );
    assert!(!h.root(|root, _| root.launch_saved()));

    refuse(&mut h, PROGRAM_REFUSAL);
    assert_eq!(
        toast_titles(&mut h).len(),
        1,
        "the claimed refusal ended the apply, so the next one toasts"
    );
}

#[gpui::test]
fn args_refusal_shows_under_extra_arguments(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-args");
    for text in [UNCLOSED_QUOTE, RESERVED_FLAG, ARGS_CONTROL] {
        h.keys("enter");
        refuse(&mut h, text);
        assert_eq!(
            refusal(&mut h),
            Some((LaunchCommandRow::Args, text.to_owned())),
            "{text}"
        );
    }
    assert!(drawn(&mut h, "settings-agent-args-error"));
    assert_eq!(toast_titles(&mut h), Vec::<String>::new());
}

#[gpui::test]
fn refusal_clears_on_the_next_edit(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-args");
    type_line(&mut h, "open");
    h.keys("enter");
    refuse(&mut h, UNCLOSED_QUOTE);
    assert!(refusal(&mut h).is_some());
    type_line(&mut h, "x");
    assert_eq!(refusal(&mut h), None, "the edit clears the refusal");
}

#[gpui::test]
fn unrelated_error_while_pending_still_toasts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-program");
    type_line(&mut h, "wrapper");
    h.keys("enter");
    refuse(&mut h, "worktree delete failed");
    assert_eq!(
        refusal(&mut h),
        None,
        "another refusal is not the launch command's"
    );
    assert_eq!(toast_titles(&mut h).len(), 1, "it toasts as before");
    refuse(&mut h, PROGRAM_REFUSAL);
    assert_eq!(
        refusal(&mut h),
        None,
        "the other error ended the apply, so nothing waits to claim this one"
    );
    assert_eq!(toast_titles(&mut h).len(), 2);
}

#[gpui::test]
fn reselecting_the_claude_tab_keeps_drafts_and_pending_apply(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-program");
    type_line(&mut h, "wrapper");
    h.keys("enter");
    h.click_on("settings-agent-args");
    type_line(&mut h, "verbose");
    h.click_on("settings-tab-claude");
    h.keys("up down");
    assert_eq!(shown_tab(&mut h), "Claude");
    assert_eq!(
        launch_fields(&mut h),
        (Some("wrapper".to_owned()), Some("verbose".to_owned())),
        "the drafts survive selecting the tab again"
    );
    refuse(&mut h, PROGRAM_REFUSAL);
    assert_eq!(
        refusal(&mut h),
        Some((LaunchCommandRow::Program, PROGRAM_REFUSAL.to_owned())),
        "the apply still waits for its answer"
    );
    assert_eq!(toast_titles(&mut h), Vec::<String>::new());
}

#[gpui::test]
fn refusal_after_switching_tabs_shows_on_return(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-program");
    type_line(&mut h, "wrapper");
    h.keys("enter");
    h.click_on("settings-tab-general");
    refuse(&mut h, PROGRAM_REFUSAL);
    assert_eq!(toast_titles(&mut h), Vec::<String>::new());
    h.click_on("settings-tab-claude");
    assert_eq!(
        refusal(&mut h),
        Some((LaunchCommandRow::Program, PROGRAM_REFUSAL.to_owned())),
        "the refusal waits on the Claude page"
    );
    assert!(drawn(&mut h, "settings-agent-program-error"));
}

#[gpui::test]
fn a_failed_apply_with_another_error_does_not_count_the_next_broadcast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-program");
    type_line(&mut h, "wrapper");
    h.keys("enter");
    refuse(&mut h, "worktree delete failed");
    assert_eq!(toast_titles(&mut h).len(), 1);
    h.click_on("settings-agent-program");
    h.keys("ctrl-a backspace");
    h.send(agent_commands("", "", None));
    assert!(
        !h.root(|root, _| root.launch_saved()),
        "another client's broadcast is no echo of the failed apply"
    );
}

#[gpui::test]
fn disconnect_hides_the_launch_rows_without_the_restart_tip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("wrapper", "", None));
    // A connection that was open drops: no overlay, so Settings stays.
    let mut dropped = Connection::new();
    dropped.on_connecting();
    dropped.on_socket_open(4242);
    let version = NATIVE_PROTOCOL_VERSIONS.first().copied().unwrap_or(1);
    dropped.on_welcome(version, support::daemon_features());
    dropped.on_closed("the daemon went away".to_owned());
    h.event(NetEvent::State(dropped));
    assert!(h.root(|root, _| root.settings_open()));
    assert_eq!(
        h.root(|root, _| root.settings_launch_command_tip()),
        None,
        "a lost connection is no older daemon"
    );
    h.click_on("settings-spawn-provider-deepseek");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("list"),
        "no launch rows in the ring"
    );
}

#[gpui::test]
fn typing_after_enter_survives_the_echo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-program");
    type_line(&mut h, "wrapper");
    h.keys("enter");
    type_line(&mut h, "x");
    h.send(agent_commands("wrapper", "", None));
    assert_eq!(
        launch_fields(&mut h).0.as_deref(),
        Some("wrapperx"),
        "the echo keeps what was typed after Enter"
    );
    assert!(!h.root(|root, _| root.launch_saved()));
}

#[gpui::test]
fn broadcast_updates_an_untouched_row_only(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-program");
    type_line(&mut h, "mine");
    h.send(agent_commands("theirs", "verbose", None));
    assert_eq!(
        launch_fields(&mut h),
        (Some("mine".to_owned()), Some("verbose".to_owned())),
        "another client's change fills only the untouched row"
    );
}

#[gpui::test]
fn program_env_locks_the_program_row_and_names_the_variable(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let stored = agent_commands("wrapper", "", Some("RUSTLING_TULIP_CLAUDE"));
    let mut h = claude_page(cx, &dir, stored);
    assert!(h.root(|root, _| root.launch_program_read_only()));
    assert_eq!(
        h.root(|root, _| root.launch_program_env_note()).as_deref(),
        Some("Set by RUSTLING_TULIP_CLAUDE")
    );
    assert!(drawn(&mut h, "settings-agent-program-env"));
    h.click_on("settings-agent-program");
    type_line(&mut h, "x");
    assert_eq!(
        launch_fields(&mut h).0.as_deref(),
        Some("wrapper"),
        "the read-only row takes no typing"
    );
    tab_to(&mut h, "settings-spawn-provider-deepseek");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-agent-args"),
        "Tab skips the read-only Program row"
    );
    type_line(&mut h, "verbose");
    h.keys("enter");
    assert_eq!(
        launch_sets(&h.sent()),
        [("wrapper".to_owned(), "verbose".to_owned())],
        "Extra arguments still applies"
    );
    h.send(agent_commands("wrapper", "verbose", None));
    assert!(
        !h.root(|root, _| root.launch_program_read_only()),
        "the row unlocks once the variable is gone"
    );
}

#[gpui::test]
fn trusted_launch_leaves_the_launch_rows_in_the_ring(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(agent_commands("", "", None));
    h.keys("ctrl-,");
    h.click_on("settings-tab-spawn-defaults");
    h.click_on("settings-spawn-trusted");
    assert_eq!(saved_ui(&dir)["spawn"]["trusted"], true);
    h.click_on("settings-tab-claude");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-agent-program"),
        "the locked Claude rows leave the ring; the launch rows stay"
    );
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-agent-args"));
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("list"));
    h.keys("tab");
    type_line(&mut h, "wrapper");
    h.keys("enter");
    assert_eq!(
        launch_sets(&h.sent()),
        [("wrapper".to_owned(), String::new())],
        "the launch rows still apply under trusted launch"
    );
}

#[gpui::test]
fn agents_group_row_is_not_selectable(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.keys("down down down down down down");
    assert_eq!(shown_tab(&mut h), "App title");
    h.keys("down");
    assert_eq!(shown_tab(&mut h), "Claude", "Down skips the group row");
    h.keys("up");
    assert_eq!(shown_tab(&mut h), "App title");

    let at = point(
        h.bounds("settings-tab-app-title").origin.x + px(8.0),
        h.bounds("settings-tab-claude").origin.y - px(10.0),
    );
    h.click(at, Modifiers::none());
    assert_eq!(
        shown_tab(&mut h),
        "App title",
        "a click on the group row selects nothing and opens no page"
    );
}

#[gpui::test]
fn tab_rows_list_every_tab_of_the_tab_enum_in_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    let mut rows = h.root(|root, _| root.settings_tab_rows());
    // The list interleaves the "Agents" group row; the tabs it draws must be
    // SettingsTab::ALL in the same order, or Down/Up leaves a tab unreachable.
    rows.retain(|label| *label != "Agents");
    assert_eq!(
        rows,
        h.root(|root, _| root.settings_tab_labels()),
        "the tab list's tabs are SettingsTab::ALL in order"
    );
}

fn can_apply(h: &mut Harness<'_>, row: &str) -> bool {
    let row = row.to_owned();
    h.root(move |root, cx| root.appearance_can_apply(&row, cx))
}

fn appearance_hint(h: &mut Harness<'_>, row: &str) -> String {
    let row = row.to_owned();
    h.root(move |root, _| root.appearance_hint(&row))
        .expect("the editor is open")
}

/// Settings open on its Appearance tab, the keyboard on the tab list.
fn appearance_tab<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = opened(cx, dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-appearance");
    assert_eq!(settings_focus(&mut h), Some("list"));
    h
}

/// Presses Tab until `stop` has the keyboard.
fn tab_to(h: &mut Harness<'_>, stop: &str) {
    for _ in 0..30 {
        if settings_focus(h) == Some(stop) {
            return;
        }
        h.keys("tab");
    }
    assert_eq!(settings_focus(h), Some(stop), "Tab never reached {stop}");
}

#[gpui::test]
fn tab_in_appearance_hex_field_leaves_for_the_next_stop(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    h.click_on("appearance-accent-hex");
    assert_eq!(settings_focus(&mut h), Some("appearance-accent-hex"));
    h.keys("tab");
    assert_eq!(shown_tab(&mut h), "Appearance");
    assert_eq!(
        settings_focus(&mut h),
        Some("appearance-background-presets"),
        "Apply has nothing to apply, so Tab passes it"
    );
    type_text(&mut h, "#12AB34");
    assert!(
        !can_apply(&mut h, "accent"),
        "the field gave up the keyboard"
    );

    h.keys("shift-tab");
    assert_eq!(settings_focus(&mut h), Some("appearance-accent-hex"));
    type_text(&mut h, "#12AB34");
    assert!(
        can_apply(&mut h, "accent"),
        "typing reaches the field again"
    );
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("appearance-accent-apply"));
    h.keys("enter");
    assert_eq!(
        appearance_hint(&mut h, "accent"),
        "Resolved: #12ab34 from current"
    );
}

#[gpui::test]
fn tab_walks_the_appearance_stops_and_shift_tab_walks_back(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    let ring = [
        "appearance-accent-presets",
        "appearance-accent-hex",
        "appearance-background-presets",
        "appearance-background-hex",
        "appearance-ui-family-filter",
        "appearance-ui-family-list",
        "appearance-family-filter",
        "appearance-family-list",
        "appearance-size-down",
        "appearance-size-up",
        "appearance-bold",
    ];
    for stop in ring {
        h.keys("tab");
        assert_eq!(settings_focus(&mut h), Some(stop));
    }
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("list"), "back to the tab list");
    for stop in ring.iter().rev() {
        h.keys("shift-tab");
        assert_eq!(settings_focus(&mut h), Some(*stop), "Shift+Tab walks back");
    }
    h.keys("shift-tab");
    assert_eq!(settings_focus(&mut h), Some("list"));
}

#[gpui::test]
fn appearance_swatch_line_lands_on_the_chosen_swatch_and_arrows_move_and_enter_picks(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    h.click_on("appearance-accent-preset-sky");
    h.click_on("settings-tab-appearance");
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("appearance-accent-presets"));
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("appearance-accent-recents"),
        "the recent line is a stop of its own"
    );
    h.keys("shift-tab");

    // Lands on Sky; Right to Lilac, the last, stays there; Left twice.
    h.keys("right right left left");
    assert_eq!(
        appearance_hint(&mut h, "accent"),
        "Resolved: #8fd3e8 from current",
        "the arrows only move the cursor"
    );
    h.keys("enter");
    assert_eq!(
        appearance_hint(&mut h, "accent"),
        "Resolved: #8fb3f0 from current",
        "Periwinkle, left of Sky"
    );
    assert_eq!(
        h.root(|root, _| root.appearance_recent_rows("accent")),
        [
            "appearance-accent-recent-8fb3f0",
            "appearance-accent-recent-8fd3e8"
        ],
        "a keyboard pick joins the recent colours"
    );
    h.keys("left left left left space");
    assert_eq!(
        appearance_hint(&mut h, "accent"),
        "Resolved: #f07a62 from current",
        "Left stops at the first swatch; Space picks too"
    );
}

#[gpui::test]
fn appearance_family_list_down_highlights_and_enter_picks(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    tab_to(&mut h, "appearance-family-list");
    h.keys("down enter");
    assert_eq!(
        appearance_hint(&mut h, "family"),
        "Resolved: Geist Mono from app",
        "Down from the reset row passes the Bundled header"
    );
    h.keys("down space");
    assert_eq!(
        appearance_hint(&mut h, "family"),
        "Resolved: Fira Code from app"
    );
    h.keys("up up up enter");
    assert_eq!(
        appearance_hint(&mut h, "family"),
        "Resolved: default cascade from built-in",
        "Up stops at the reset row"
    );
}

#[gpui::test]
fn settings_footer_stays_visible_on_a_short_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    let height = 420.0;
    h.cx.simulate_resize(gpui::size(px(1000.0), px(height)));
    h.cx.run_until_parked();
    let close = h.bounds("settings-footer-close");
    assert!(
        close.bottom() <= px(height),
        "Close ends at {:?}, below the {height}px window",
        close.bottom()
    );
    let content = h.bounds("settings-content");
    assert!(
        content.bottom() <= close.top(),
        "the content scrolls above the footer: {content:?}, {close:?}"
    );
}

/// The family the interface draws in.
fn ui_family(h: &mut Harness<'_>) -> String {
    h.cx.update(|_, cx| fonts::ui_family(cx).to_string())
}

#[gpui::test]
fn a_ui_font_pick_redraws_the_interface_and_is_saved_and_reset_restores_the_default(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    assert_eq!(ui_family(&mut h), "Geist", "nothing picked: the default");

    h.click_on("appearance-ui-family-option-Inter");
    assert_eq!(
        saved_ui(&dir)["ui_font_family"],
        "Inter",
        "saved on this machine"
    );
    assert_eq!(ui_family(&mut h), "Inter", "the pick draws at once");

    // A family the test text system knows: one of gpui's fallbacks.
    h.click_on("appearance-ui-family-filter");
    type_text(&mut h, "segoe");
    h.click_on("appearance-ui-family-option-Segoe UI");
    assert_eq!(ui_family(&mut h), "Segoe UI", "the pick draws at once");
    assert_eq!(saved_ui(&dir)["ui_font_family"], "Segoe UI");
    assert_eq!(
        h.root(|root, cx| root.pane_font("p1", cx).and_then(|font| font.family)),
        None,
        "the terminal font is not the UI font"
    );

    h.click_on("appearance-ui-family-reset");
    assert_eq!(
        ui_family(&mut h),
        "Geist",
        "Use built-in restores the default"
    );
    assert_eq!(saved_ui(&dir)["ui_font_family"], serde_json::Value::Null);
}

#[gpui::test]
fn appearance_size_buttons_are_two_stops_and_enter_steps(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    tab_to(&mut h, "appearance-size-down");
    h.keys("enter");
    assert_eq!(appearance_hint(&mut h, "size"), "Resolved: 12px from app");
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("appearance-size-up"));
    h.keys("enter space");
    assert_eq!(appearance_hint(&mut h, "size"), "Resolved: 14px from app");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("appearance-size-reset"),
        "the reset link is a stop while the level sets the size"
    );
    h.keys("enter");
    assert_eq!(
        appearance_hint(&mut h, "size"),
        "Resolved: 13px from built-in"
    );
}

#[gpui::test]
fn appearance_bold_checkbox_toggles_with_space(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    tab_to(&mut h, "appearance-bold");
    h.keys("space");
    assert_eq!(appearance_hint(&mut h, "bold"), "Resolved: bold from app");
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("appearance-bold-reset"));
    h.keys("enter");
    assert_eq!(
        appearance_hint(&mut h, "bold"),
        "Resolved: normal from built-in"
    );
}

#[gpui::test]
fn appearance_hex_and_filter_fields_draw_the_focus_ring(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    for field in ["appearance-accent-hex", "appearance-family-filter"] {
        let ring = format!("{field}-ring");
        assert!(h.bounds(&ring).origin.x < px(0.0), "{field}: no ring yet");
        tab_to(&mut h, field);
        let frame = h.bounds(field);
        let drawn = h.bounds(&ring);
        assert!(drawn.origin.x >= px(0.0), "{field}: the ring is drawn");
        assert!(
            drawn.origin.x < frame.origin.x && drawn.size.width > frame.size.width,
            "{field}: the ring sits round the frame"
        );
    }
}

#[gpui::test]
fn appearance_apply_hands_the_keyboard_to_the_stop_after_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    h.click_on("appearance-accent-hex");
    type_text(&mut h, "#12AB34");
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("appearance-accent-apply"));
    h.keys("enter");
    assert_eq!(
        appearance_hint(&mut h, "accent"),
        "Resolved: #12ab34 from current"
    );
    assert_eq!(
        settings_focus(&mut h),
        Some("appearance-accent-reset"),
        "Apply set the accent, which turned on the reset link after it"
    );

    h.click_on("appearance-background-preset-ink");
    h.click_on("appearance-background-hex");
    type_text(&mut h, "#123456");
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("appearance-background-apply"));
    h.keys("enter");
    assert_eq!(
        appearance_hint(&mut h, "background"),
        "Resolved: #123456 from current"
    );
    assert_eq!(
        settings_focus(&mut h),
        Some("appearance-background-reset"),
        "the background reset link followed Apply"
    );
}

#[gpui::test]
fn appearance_size_reset_hands_the_keyboard_to_the_stop_after_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    tab_to(&mut h, "appearance-size-up");
    h.keys("enter tab");
    assert_eq!(settings_focus(&mut h), Some("appearance-size-reset"));
    h.keys("enter");
    assert_eq!(
        appearance_hint(&mut h, "size"),
        "Resolved: 13px from built-in"
    );
    assert_eq!(
        settings_focus(&mut h),
        Some("appearance-bold"),
        "the reset link is off now, so the keyboard moved on to bold"
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("list"),
        "the ring walks on from bold"
    );
}

#[gpui::test]
fn a_click_on_an_appearance_control_moves_the_keyboard_there(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    tab_to(&mut h, "appearance-bold");
    h.click_on("appearance-accent-preset-sky");
    assert_eq!(settings_focus(&mut h), Some("appearance-accent-presets"));
    h.keys("left space");
    assert_eq!(
        appearance_hint(&mut h, "accent"),
        "Resolved: #8fb3f0 from current",
        "the cursor stood on the clicked swatch: Left went to Periwinkle"
    );
    assert_eq!(
        appearance_hint(&mut h, "bold"),
        "Resolved: normal from built-in",
        "Space was the swatch line's, not bold's"
    );

    h.click_on("appearance-family-option-Fira Code");
    assert_eq!(settings_focus(&mut h), Some("appearance-family-list"));
    h.keys("down enter");
    assert_eq!(
        appearance_hint(&mut h, "family"),
        "Resolved: JetBrains Mono from app",
        "the highlight stood on the clicked family"
    );

    h.click_on("appearance-size-up");
    assert_eq!(settings_focus(&mut h), Some("appearance-size-up"));
}

#[gpui::test]
fn a_family_row_click_keeps_the_clicked_row_in_view(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    let names = h.root(rustling_tulip_native::RootView::appearance_family_names);
    let far = names.last().expect("a family").clone();
    let before_far = names
        .len()
        .checked_sub(2)
        .and_then(|at| names.get(at))
        .expect("two families")
        .clone();
    let far_row = format!("appearance-family-option-{far}");
    tab_to(&mut h, "appearance-family-list");
    for _ in 0..names.len() {
        h.keys("down");
    }
    h.keys("shift-tab");
    assert_eq!(settings_focus(&mut h), Some("appearance-family-filter"));
    let list = h.bounds("appearance-family-list");
    let shown = |row: gpui::Bounds<gpui::Pixels>| {
        row.origin.y >= list.origin.y && row.bottom() <= list.bottom()
    };
    assert!(
        h.bounds("appearance-family-reset").origin.y < list.origin.y,
        "the list scrolled past its first row"
    );
    assert!(
        shown(h.bounds(&far_row)),
        "{far} is in view before the click"
    );

    h.click_on(&far_row);
    assert_eq!(settings_focus(&mut h), Some("appearance-family-list"));
    assert!(
        shown(h.bounds(&far_row)),
        "{far} stays in view after the click"
    );
    assert_eq!(
        appearance_hint(&mut h, "family"),
        format!("Resolved: {far} from app")
    );
    h.keys("up enter");
    assert_eq!(
        appearance_hint(&mut h, "family"),
        format!("Resolved: {before_far} from app"),
        "the highlight stood on the clicked row"
    );
}

#[gpui::test]
fn a_click_in_the_hex_field_frame_keeps_the_field(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    let field = h.bounds("appearance-accent-hex");
    let padding = point(field.origin.x + px(4.0), field.center().y);
    h.click(padding, Modifiers::none());
    assert_eq!(settings_focus(&mut h), Some("appearance-accent-hex"));
    type_text(&mut h, "#12AB34");
    assert!(can_apply(&mut h, "accent"), "typed text reaches the field");
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
