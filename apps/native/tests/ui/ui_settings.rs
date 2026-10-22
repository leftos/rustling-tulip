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
use protocol::{ClientMessage, DaemonMessage, SplitDirection, TabEntry};
use rustling_tulip_native::SidebarView;
use support::{Fixture, Harness, TestDir, pane, session, split, tab};

const TAB_SELECTORS: [&str; 7] = [
    "settings-tab-general",
    "settings-tab-notifications",
    "settings-tab-spawn-defaults",
    "settings-tab-worktrees",
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
    assert!(h.bounds("settings-keep-awake-toggle").origin.x >= px(0.0));

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
    h.keys("down down down down");
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

    h.keys("tab tab tab tab tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-terminal-copy-on-selection"),
        "past the two Default view and the two Leaf density choices"
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
    h.keys("shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab");
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
fn claude_provider_default_is_disabled_without_the_feature(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        serde_json::json!({ "spawn": { "claude_provider": "deepseek" } }).to_string(),
    )
    .expect("seed native-ui.json");
    let mut h = opened(cx, &dir);
    h.welcome_without_features();
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    assert_eq!(
        h.root(|root, _| root.settings_provider_tip()),
        Some("Restart the daemon to use DeepSeek: .\\rt.ps1 restart")
    );
    assert_eq!(
        h.root(|root, _| root.settings_provider_choice()),
        "Anthropic",
        "the disabled row shows Anthropic over a saved DeepSeek default"
    );
    for selector in [
        "settings-spawn-provider-anthropic",
        "settings-spawn-provider-deepseek",
    ] {
        assert!(
            h.bounds(selector).origin.x >= px(0.0),
            "{selector} is still painted, dimmed"
        );
    }
    h.click_on("settings-spawn-provider-anthropic");
    assert_eq!(
        saved_ui(&dir)["spawn"]["claude_provider"],
        "deepseek",
        "a click on the disabled choice saves nothing"
    );
    h.click_on("settings-spawn-approval-plan");
    assert_eq!(settings_focus(&mut h), Some("settings-spawn-approval-plan"));
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("list"),
        "Tab skips the disabled provider row and leaves the page"
    );
    assert_eq!(
        saved_ui(&dir)["spawn"]["claude_provider"],
        "deepseek",
        "the saved default is kept"
    );

    h.welcome_with_features();
    assert_eq!(h.root(|root, _| root.settings_provider_tip()), None);
    assert_eq!(
        h.root(|root, _| root.settings_provider_choice()),
        "DeepSeek",
        "the saved default shows again once a capable daemon connects"
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

    h.send(DaemonMessage::Repos {
        repos: vec![support::repo("r1", "C:/r1")],
    });
    h.click_on("sidebar-add-session");
    assert!(h.root(|root, _| root.spawn_dialog_open()));
    let chosen = h.root(|root, _| root.spawn_dialog_selected());
    assert!(
        chosen.contains(&"spawn-provider-deepseek".to_owned()),
        "a new dialog starts on the saved provider: {chosen:?}"
    );
    assert_eq!(
        h.root(|root, _| root.spawn_dialog_provider_tip()),
        None,
        "the fake daemon routes providers"
    );
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
    assert_eq!(
        settings_focus(&mut h),
        Some("list"),
        "the provider is the page's last row"
    );
    h.keys("shift-tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-spawn-provider-deepseek"),
        "Shift+Tab walks back"
    );
}

#[gpui::test]
fn agents_group_row_is_not_selectable(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.keys("down down down down down");
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
