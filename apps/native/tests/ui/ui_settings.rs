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
    AgentCommandEntry, AgentId, ClientMessage, DaemonMessage, KeepAwakeMode, OptionValue,
    OptionValues, SplitDirection, TabEntry,
};
use rustling_tulip_native::{
    Connection, LaunchCommandRow, NATIVE_PROTOCOL_VERSIONS, NetEvent, SidebarView, fonts,
};
use support::{Fixture, Harness, TestDir, pane, repo, session, split, tab};

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

/// Whether `native-ui.json`, if it was written at all, holds `spawn.<key>`.
fn saves_spawn_key(dir: &TestDir, key: &str) -> bool {
    std::fs::read_to_string(dir.path().join("native-ui.json")).is_ok_and(|text| {
        let saved: serde_json::Value = serde_json::from_str(&text).expect("native-ui.json is JSON");
        saved["spawn"].get(key).is_some()
    })
}

fn claude() -> AgentId {
    AgentId("claude".to_owned())
}

fn text(value: &str) -> OptionValue {
    OptionValue::Text(value.to_owned())
}

fn options(pairs: &[(&str, OptionValue)]) -> OptionValues {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

/// The `SetAgentSettings` messages the client sent since the last look.
fn sent_agent_settings(h: &mut Harness<'_>) -> Vec<(AgentId, OptionValues)> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetAgentSettings { agent, options } => Some((agent, options)),
            _ => None,
        })
        .collect()
}

/// The daemon broadcasts `options` as Claude's Settings values.
fn broadcast_claude(h: &mut Harness<'_>, options: OptionValues) {
    h.send(DaemonMessage::AgentSettings {
        settings: std::collections::BTreeMap::from([(claude(), options)]),
    });
}

/// The label of the choice the Claude page shows selected in `field`.
fn claude_choice(h: &mut Harness<'_>, field: &str) -> Option<String> {
    let field = field.to_owned();
    h.root(move |root, _| root.settings_field_choice("claude", &field))
}

fn approval_choice(h: &mut Harness<'_>) -> Option<String> {
    claude_choice(h, "permission_mode")
}

fn provider_choice(h: &mut Harness<'_>) -> Option<String> {
    claude_choice(h, "provider")
}

/// Whether the Claude page shows `field` locked by trusted launch.
fn claude_locked(h: &mut Harness<'_>, field: &str) -> bool {
    let field = field.to_owned();
    h.root(move |root, _| root.settings_field_locked("claude", &field))
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

    list_agents(&mut h, test_agent());
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
            "Claude",
            "Test agent"
        ],
        "one tab per listed agent, labelled with its descriptor's label, in the daemon's order"
    );
    assert!(
        drawn(&mut h, "settings-tab-test"),
        "an agent's tab selector names its tag"
    );
}

/// The test agent's descriptor: a second agent beside Claude, with a
/// choice, a toggle, and a text field the toggle hides.
fn test_agent() -> protocol::AgentDescriptor {
    agent_api::test_agent::DESCRIPTOR.clone()
}

fn test_agent_id() -> AgentId {
    AgentId("test-agent".to_owned())
}

/// The daemon lists its registered agents, then `second`.
fn list_agents(h: &mut Harness<'_>, second: protocol::AgentDescriptor) {
    let mut agents = agent_registry::descriptors();
    agents.push(second);
    h.send(DaemonMessage::Agents { agents });
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
fn sessions_under_repos_choice_saves_and_is_reached_by_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    h.keys("ctrl-,");
    h.click_on("settings-sessions-under-repos-busy");
    assert_eq!(saved_ui(&dir)["general"]["sessions_under_repos"], "busy");
    h.keys("escape");

    h.keys("ctrl-,");
    assert_eq!(settings_focus(&mut h), Some("list"));
    h.keys("tab tab tab tab tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-sessions-under-repos-every"),
        "one tab past the two Leaf density choices"
    );
}

#[gpui::test]
fn repo_sort_choice_saves_and_is_reached_by_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    h.keys("ctrl-,");
    h.click_on("settings-repo-sort-active");
    assert_eq!(saved_ui(&dir)["general"]["repo_sort"], "active");
    h.click_on("settings-repo-sort-manual");
    assert_eq!(saved_ui(&dir)["general"]["repo_sort"], "manual");
    h.keys("escape");

    h.keys("ctrl-,");
    assert_eq!(settings_focus(&mut h), Some("list"));
    h.keys("tab tab tab tab tab tab tab tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-repo-sort-manual"),
        "one tab past the three Sessions under repos choices"
    );
    h.keys("tab tab");
    assert_eq!(settings_focus(&mut h), Some("settings-repo-sort-needs-you"));
    h.keys("space");
    assert_eq!(saved_ui(&dir)["general"]["repo_sort"], "needs_you");
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

    h.keys("tab tab tab tab tab tab tab tab tab tab tab tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-general-open-text-externally"),
        "past Always, the two Default view, the two Leaf density, the three Sessions under repos \
         and the three Sort repos choices"
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
    h.keys(
        "shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab \
         shift-tab shift-tab shift-tab shift-tab shift-tab",
    );
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
        "settings-claude-permission-mode-cli-default",
        "settings-claude-provider-anthropic",
    ] {
        assert!(
            h.bounds(selector).origin.x < px(0.0),
            "{selector} moved to its agent page"
        );
    }

    h.click_on("settings-tab-claude");
    assert_eq!(shown_tab(&mut h), "Claude");
    for selector in [
        "settings-claude-permission-mode-cli-default",
        "settings-claude-permission-mode-accept-edits",
        "settings-claude-provider-anthropic",
    ] {
        assert!(
            h.bounds(selector).origin.x >= px(0.0),
            "{selector} is painted"
        );
    }
    h.sent();
    h.click_on("settings-claude-permission-mode-accept-edits");
    let accept_edits = options(&[("permission_mode", text("accept_edits"))]);
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(claude(), accept_edits.clone())],
        "sent to the daemon at once"
    );
    assert_eq!(
        approval_choice(&mut h).as_deref(),
        Some("CLI default"),
        "the row waits for the daemon's broadcast"
    );
    broadcast_claude(&mut h, accept_edits);
    assert_eq!(approval_choice(&mut h).as_deref(), Some("Accept edits"));

    h.click_on("settings-tab-spawn-defaults");
    h.click_on("settings-spawn-trusted");
    assert_eq!(saved_ui(&dir)["spawn"]["trusted"], true);
    assert!(
        !saves_spawn_key(&dir, "permission_mode"),
        "native-ui.json holds no approval mode"
    );
    assert_eq!(
        approval_choice(&mut h).as_deref(),
        Some("Accept edits"),
        "the value stays while trusted locks the rows"
    );
}

#[gpui::test]
fn the_claude_page_rows_come_from_the_descriptor(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    assert_eq!(
        approval_choice(&mut h).as_deref(),
        Some("CLI default"),
        "a field whose default is no value shows the no-value choice"
    );
    assert_eq!(
        provider_choice(&mut h).as_deref(),
        Some("Anthropic"),
        "the provider shows its default's label"
    );
    assert_eq!(
        claude_choice(&mut h, "model"),
        None,
        "the model is not on the Settings page"
    );
    assert!(
        !drawn(&mut h, "settings-claude-model"),
        "no model row is drawn"
    );
    h.keys("tab");
    for stop in [
        "settings-claude-permission-mode-cli-default",
        "settings-claude-permission-mode-default",
        "settings-claude-permission-mode-accept-edits",
        "settings-claude-permission-mode-bypass-permissions",
        "settings-claude-permission-mode-plan",
        "settings-claude-provider-anthropic",
        "settings-claude-provider-deepseek",
    ] {
        assert_eq!(
            settings_focus(&mut h),
            Some(stop),
            "permission mode, then provider, in the descriptor's order"
        );
        h.keys("tab");
    }
    assert_eq!(
        settings_focus(&mut h),
        Some("list"),
        "no other field is on the page before the daemon reports its command"
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

    broadcast_claude(&mut h, options(&[("permission_mode", text("plan"))]));
    h.click_on("settings-spawn-trusted");
    assert_eq!(saved_ui(&dir)["spawn"]["trusted"], true);

    h.click_on("settings-tab-claude");
    assert_eq!(
        approval_choice(&mut h).as_deref(),
        Some("Plan"),
        "the locked row keeps showing the daemon's value"
    );
    assert!(
        h.bounds("settings-claude-permission-mode-locked").origin.x >= px(0.0),
        "the approval row says why it is dead"
    );
    assert!(claude_locked(&mut h, "permission_mode"));
    assert!(
        !claude_locked(&mut h, "provider"),
        "trusted launch locks only the fields marked for it"
    );
    assert!(
        !drawn(&mut h, "settings-claude-provider-locked"),
        "the provider row carries no locked note"
    );
    h.sent();
    h.click_on("settings-claude-permission-mode-accept-edits");
    assert!(
        h.bounds("settings-claude-permission-mode-accept-edits")
            .origin
            .x
            >= px(0.0),
        "the disabled choice is still painted"
    );
    assert_eq!(
        sent_agent_settings(&mut h),
        Vec::new(),
        "a click on it sends nothing"
    );
    h.click_on("settings-claude-provider-deepseek");
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(
            claude(),
            options(&[
                ("permission_mode", text("plan")),
                ("provider", text("deepseek")),
            ])
        )],
        "the provider stays editable under trusted launch"
    );
    h.click_on("settings-tab-claude");
    assert_eq!(settings_focus(&mut h), Some("list"));
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-claude-provider-anthropic"),
        "the locked approval row left the ring; the provider stays"
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-claude-provider-deepseek")
    );
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("list"));

    h.click_on("settings-tab-spawn-defaults");
    h.click_on("settings-spawn-trusted");
    assert_eq!(saved_ui(&dir)["spawn"]["trusted"], false);
    h.click_on("settings-tab-claude");
    broadcast_claude(&mut h, options(&[("permission_mode", text("plan"))]));
    h.sent();
    h.click_on("settings-claude-permission-mode-accept-edits");
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(
            claude(),
            options(&[("permission_mode", text("accept_edits"))])
        )],
        "the choice works again"
    );
}

#[gpui::test]
fn claude_provider_default_saves_and_seeds_the_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "C:/r1")];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    h.sent();
    tab_to(&mut h, "settings-claude-provider-deepseek");
    h.keys("enter");
    let deepseek = options(&[("provider", text("deepseek"))]);
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(claude(), deepseek.clone())]
    );
    assert_eq!(
        provider_choice(&mut h).as_deref(),
        Some("Anthropic"),
        "the row waits for the daemon's broadcast"
    );
    broadcast_claude(&mut h, deepseek);
    assert_eq!(provider_choice(&mut h).as_deref(), Some("DeepSeek"));
    h.click_on("settings-claude-provider-anthropic");
    let anthropic = options(&[("provider", text("anthropic"))]);
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(claude(), anthropic.clone())]
    );
    broadcast_claude(&mut h, anthropic);
    assert_eq!(provider_choice(&mut h).as_deref(), Some("Anthropic"));
    h.click_on("settings-claude-provider-deepseek");
    let deepseek = options(&[("provider", text("deepseek"))]);
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(claude(), deepseek.clone())]
    );
    broadcast_claude(&mut h, deepseek);
    assert_eq!(provider_choice(&mut h).as_deref(), Some("DeepSeek"));
    h.keys("escape");
    assert!(!h.root(|root, _| root.settings_open()));

    h.click_on("sidebar-add-session");
    assert!(h.root(|root, _| root.spawn_dialog_open()));
    let chosen = h.root(|root, _| root.spawn_dialog_selected());
    assert!(
        chosen.contains(&"spawn-opt-provider-deepseek".to_owned()),
        "the daemon's value seeds the dialog: {chosen:?}"
    );
    assert!(
        !saves_spawn_key(&dir, "claude_provider"),
        "native-ui.json holds no provider"
    );
}

#[gpui::test]
fn a_settings_row_keeps_the_other_claude_settings(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    broadcast_claude(&mut h, options(&[("provider", text("deepseek"))]));
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    h.sent();
    h.click_on("settings-claude-permission-mode-plan");
    let plan = options(&[
        ("permission_mode", text("plan")),
        ("provider", text("deepseek")),
    ]);
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(claude(), plan.clone())],
        "the provider rides along with the changed mode"
    );
    broadcast_claude(&mut h, plan);
    assert_eq!(approval_choice(&mut h).as_deref(), Some("Plan"));
    h.click_on("settings-claude-permission-mode-cli-default");
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(
            claude(),
            options(&[
                ("permission_mode", OptionValue::Null),
                ("provider", text("deepseek")),
            ])
        )],
        "CLI default is sent as no value"
    );
}

#[gpui::test]
fn two_quick_settings_clicks_keep_both_keys(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    h.sent();
    h.click_on("settings-claude-permission-mode-plan");
    h.click_on("settings-claude-provider-deepseek");
    let plan = options(&[("permission_mode", text("plan"))]);
    let both = options(&[
        ("permission_mode", text("plan")),
        ("provider", text("deepseek")),
    ]);
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(claude(), plan), (claude(), both.clone())],
        "the second click keeps the first one's key before any broadcast"
    );
    assert_eq!(
        approval_choice(&mut h).as_deref(),
        Some("CLI default"),
        "the rows still wait for the daemon"
    );
    broadcast_claude(&mut h, both);
    h.click_on("settings-claude-permission-mode-default");
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(
            claude(),
            options(&[
                ("permission_mode", text("default")),
                ("provider", text("deepseek")),
            ])
        )],
        "a broadcast starts the next change from the daemon's map"
    );
}

/// A connection to the daemon that has said `Welcome`.
fn welcomed() -> Connection {
    let mut conn = Connection::new();
    conn.on_connecting();
    conn.on_socket_open(4242);
    let version = NATIVE_PROTOCOL_VERSIONS.first().copied().unwrap_or(1);
    conn.on_welcome(version, support::daemon_features());
    conn
}

#[gpui::test]
fn the_claude_rows_are_disabled_while_disconnected(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    let mut dropped = welcomed();
    dropped.on_closed("the daemon went away".to_owned());
    h.event(NetEvent::State(dropped));
    assert!(h.root(|root, _| root.settings_open()));
    assert_ne!(
        shown_tab(&mut h),
        "General",
        "a disconnect keeps the agent page shown"
    );
    assert!(
        !h.root(|root, _| root.settings_tab_rows())
            .contains(&"Claude"),
        "a disconnected client lists no agent tab"
    );
    for selector in [
        "settings-claude-permission-mode-plan",
        "settings-claude-provider-deepseek",
    ] {
        assert!(
            !drawn(&mut h, selector),
            "{selector}: a disconnected client lists no agent to draw"
        );
    }
    assert_eq!(approval_choice(&mut h), None);
    h.sent();
    assert_eq!(
        sent_agent_settings(&mut h),
        Vec::new(),
        "nothing is sent while disconnected"
    );
}

#[gpui::test]
fn the_claude_rows_wait_for_agent_settings(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    let mut dropped = welcomed();
    dropped.on_closed("the daemon went away".to_owned());
    h.event(NetEvent::State(dropped));
    h.event(NetEvent::State(welcomed()));
    h.send(DaemonMessage::Agents {
        agents: agent_registry::descriptors(),
    });
    assert_eq!(
        shown_tab(&mut h),
        "Claude",
        "the reconnect keeps Claude's page"
    );
    h.click_on("settings-tab-claude");
    h.sent();
    h.click_on("settings-claude-permission-mode-plan");
    assert_eq!(
        sent_agent_settings(&mut h),
        Vec::new(),
        "a click before this connection's AgentSettings sends nothing"
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("list"),
        "the Claude rows are out of the ring"
    );

    broadcast_claude(&mut h, options(&[("provider", text("deepseek"))]));
    h.click_on("settings-claude-permission-mode-plan");
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(
            claude(),
            options(&[
                ("permission_mode", text("plan")),
                ("provider", text("deepseek")),
            ])
        )],
        "the rows take clicks once the stored values arrived"
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

    h.send(agent_commands("", "", None));
    h.click_on("settings-tab-claude");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-claude-permission-mode-cli-default")
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-claude-permission-mode-default")
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-claude-permission-mode-accept-edits")
    );
    h.sent();
    h.keys("space");
    let accept_edits = options(&[("permission_mode", text("accept_edits"))]);
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(claude(), accept_edits.clone())]
    );
    broadcast_claude(&mut h, accept_edits);
    assert_eq!(approval_choice(&mut h).as_deref(), Some("Accept edits"));
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-claude-permission-mode-bypass-permissions")
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-claude-permission-mode-plan")
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-claude-provider-anthropic")
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-claude-provider-deepseek")
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

    h.click_on("settings-tab-spawn-defaults");
    h.click_on("settings-spawn-trusted");
    h.click_on("settings-tab-claude");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-claude-provider-anthropic"),
        "under trusted launch the walk starts at the provider"
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
    tab_to(&mut h, "settings-claude-provider-deepseek");
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
    h.click_on("settings-claude-provider-deepseek");
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
    assert_ne!(
        shown_tab(&mut h),
        "General",
        "a disconnect keeps the agent page shown"
    );
    assert!(
        !drawn(&mut h, "settings-agent-program"),
        "no launch rows are drawn"
    );
}

/// A launch-command entry for agent `agent`.
fn command_entry(agent: &str, program: &str, args: &str) -> AgentCommandEntry {
    AgentCommandEntry {
        agent: AgentId(agent.to_owned()),
        program: program.to_owned(),
        args: args.to_owned(),
        program_env: None,
    }
}

/// The `(agent, program, args)` of every launch command the client sent.
fn command_sets(sent: &[ClientMessage]) -> Vec<(String, String, String)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetAgentCommand {
                agent,
                program,
                args,
            } => Some((agent.0.clone(), program.clone(), args.clone())),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn launch_command_drafts_are_kept_per_agent(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    list_agents(&mut h, test_agent());
    h.send(DaemonMessage::AgentCommands {
        commands: vec![
            command_entry("claude", "", ""),
            command_entry("test-agent", "", ""),
        ],
    });
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    h.click_on("settings-agent-program");
    type_line(&mut h, "wrapper");
    h.click_on("settings-tab-test");
    assert_eq!(shown_tab(&mut h), "Test agent");
    assert_eq!(
        launch_fields(&mut h),
        (Some(String::new()), Some(String::new())),
        "the second agent's page has its own launch fields"
    );
    h.click_on("settings-agent-args");
    type_line(&mut h, "verbose");
    h.sent();
    h.keys("enter");
    assert_eq!(
        command_sets(&h.sent()),
        [("test-agent".to_owned(), String::new(), "verbose".to_owned())],
        "Apply sends the shown page's agent's command only"
    );
    h.click_on("settings-tab-claude");
    assert_eq!(
        launch_fields(&mut h),
        (Some("wrapper".to_owned()), Some(String::new())),
        "Claude's draft survives the other page"
    );
    h.click_on("settings-tab-test");
    assert_eq!(
        launch_fields(&mut h),
        (Some(String::new()), Some("verbose".to_owned())),
        "and the second page keeps its own"
    );
}

#[gpui::test]
fn a_second_agents_page_lists_its_settings_fields(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let mut agent = test_agent();
    let toggle = agent
        .fields
        .iter()
        .find(|field| field.id == "sandbox")
        .cloned()
        .expect("the test agent has a toggle");
    agent.fields.push(protocol::OptionField {
        id: "future".to_owned(),
        label: "future".to_owned(),
        kind: protocol::FieldKind::Unknown,
        ..toggle
    });
    list_agents(&mut h, agent);
    h.keys("ctrl-,");
    h.click_on("settings-tab-test");
    assert_eq!(shown_tab(&mut h), "Test agent");
    assert_eq!(
        h.root(|root, _| root.settings_field_choice("test", "mode"))
            .as_deref(),
        Some("fast"),
        "the choice shows its default"
    );
    for selector in [
        "settings-test-mode-fast",
        "settings-test-mode-slow",
        "settings-test-sandbox",
        "settings-test-profile",
        "settings-test-future",
    ] {
        assert!(drawn(&mut h, selector), "{selector} is drawn");
    }
    assert_eq!(
        h.root(|root, _| root.settings_page_fields("test")),
        ["mode", "sandbox", "profile", "future"],
        "the page holds its Settings fields in the descriptor's order"
    );
    assert_eq!(
        h.root(|root, _| root.settings_field_note("test", "future")),
        Some("Update the client to change this"),
        "a kind this client does not know says so"
    );
    h.sent();
    h.click_on("settings-test-sandbox");
    let sandboxed = options(&[("sandbox", OptionValue::Bool(true))]);
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(test_agent_id(), sandboxed.clone())],
        "the toggle writes the second agent's settings"
    );
    h.send(DaemonMessage::AgentSettings {
        settings: std::collections::BTreeMap::from([(test_agent_id(), sandboxed)]),
    });
    assert_eq!(
        h.root(|root, _| root.settings_page_fields("test")),
        ["mode", "sandbox", "future"],
        "a field a when rule hides is absent"
    );
}

#[gpui::test]
fn a_field_a_when_rule_hides_keeps_the_keyboard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    list_agents(&mut h, test_agent());
    h.keys("ctrl-,");
    h.click_on("settings-tab-test");
    h.keys("tab tab tab tab");
    assert_eq!(settings_focus(&mut h), Some("settings-test-profile"));
    h.send(DaemonMessage::AgentSettings {
        settings: std::collections::BTreeMap::from([(
            test_agent_id(),
            options(&[("sandbox", OptionValue::Bool(true))]),
        )]),
    });
    assert_eq!(
        h.root(|root, _| root.settings_page_fields("test")),
        ["mode", "sandbox"],
        "the sandbox rule hid profile"
    );
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-test-mode-fast"),
        "the keyboard moves to the stop after profile in the full order: past the launch rows \
         the daemon has not reported, round to the first"
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-test-mode-slow"),
        "Tab walks on from there"
    );
}

#[gpui::test]
fn a_text_setting_applies_on_enter_and_on_blur(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    list_agents(&mut h, test_agent());
    h.keys("ctrl-,");
    h.click_on("settings-tab-test");
    h.sent();
    h.click_on("settings-test-profile");
    assert_eq!(settings_focus(&mut h), Some("settings-test-profile"));
    type_line(&mut h, "work");
    assert_eq!(
        sent_agent_settings(&mut h),
        Vec::new(),
        "typing alone sends nothing"
    );
    h.keys("enter");
    let work = options(&[("profile", text("work"))]);
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(test_agent_id(), work.clone())],
        "Enter applies the field"
    );
    h.send(DaemonMessage::AgentSettings {
        settings: std::collections::BTreeMap::from([(test_agent_id(), work)]),
    });
    type_line(&mut h, "s");
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("list"));
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(test_agent_id(), options(&[("profile", text("works"))]))],
        "leaving the field applies it"
    );
    h.click_on("settings-test-profile");
    h.keys("tab");
    assert_eq!(
        sent_agent_settings(&mut h),
        Vec::new(),
        "leaving an unedited field sends nothing"
    );
}

#[gpui::test]
fn an_edited_text_setting_applies_when_settings_closes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    list_agents(&mut h, test_agent());
    h.keys("ctrl-,");
    h.click_on("settings-tab-test");
    h.click_on("settings-test-profile");
    type_line(&mut h, "work");
    h.sent();
    h.keys("escape");
    assert!(!h.root(|root, _| root.settings_open()));
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(test_agent_id(), options(&[("profile", text("work"))]))],
        "closing Settings applies the edited field as Enter would"
    );
}

/// The Settings placeholder of field `field` on the page of the agent
/// tagged `tag`.
fn text_placeholder(h: &mut Harness<'_>, tag: &str, field: &str) -> Option<String> {
    let (tag, field) = (tag.to_owned(), field.to_owned());
    h.root(move |root, _| root.settings_text_placeholder(&tag, &field))
}

#[gpui::test]
fn a_model_placeholder_follows_the_field_it_depends_on(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let mut agent = test_agent();
    agent
        .fields
        .iter_mut()
        .find(|field| field.id == "profile")
        .expect("the test agent has a text field")
        .when
        .push(protocol::FieldRule {
            if_field: "mode".to_owned(),
            equals: text("slow"),
            then: protocol::FieldOverride {
                hidden: None,
                placeholder: Some("slow profile".to_owned()),
                suggestions: None,
            },
        });
    list_agents(&mut h, agent);
    h.keys("ctrl-,");
    h.click_on("settings-tab-test");
    let before = text_placeholder(&mut h, "test", "profile");
    assert!(before.is_some(), "the page has the text field's input");
    assert_ne!(before.as_deref(), Some("slow profile"));
    h.send(DaemonMessage::AgentSettings {
        settings: std::collections::BTreeMap::from([(
            test_agent_id(),
            options(&[("mode", text("slow"))]),
        )]),
    });
    assert_eq!(
        text_placeholder(&mut h, "test", "profile").as_deref(),
        Some("slow profile"),
        "the rule on the field it depends on sets the placeholder"
    );
    h.send(DaemonMessage::AgentSettings {
        settings: std::collections::BTreeMap::from([(test_agent_id(), OptionValues::new())]),
    });
    assert_eq!(
        text_placeholder(&mut h, "test", "profile"),
        before,
        "and gives it back once the rule no longer matches"
    );
}

#[gpui::test]
fn a_disconnect_keeps_the_agent_page_until_the_next_welcome(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-args");
    type_line(&mut h, "verbose");
    let mut dropped = welcomed();
    dropped.on_closed("the daemon went away".to_owned());
    h.event(NetEvent::State(dropped));
    assert!(h.root(|root, _| root.settings_open()));
    assert_eq!(
        shown_tab(&mut h),
        "claude",
        "the agent page stays shown, named by its id while no descriptor is at hand"
    );
    assert_eq!(
        h.root(|root, _| root.settings_page_fields("claude")),
        Vec::<String>::new(),
        "no option rows while disconnected"
    );
    assert_eq!(launch_fields(&mut h).1.as_deref(), Some("verbose"));

    h.event(NetEvent::State(welcomed()));
    h.send(DaemonMessage::Agents {
        agents: agent_registry::descriptors(),
    });
    h.send(agent_commands("", "", None));
    assert_eq!(
        shown_tab(&mut h),
        "Claude",
        "the same page after the reconnect"
    );
    assert_eq!(
        launch_fields(&mut h),
        (Some(String::new()), Some("verbose".to_owned())),
        "the draft survives the reconnect"
    );

    h.send(DaemonMessage::Agents {
        agents: vec![test_agent()],
    });
    assert_eq!(
        shown_tab(&mut h),
        "General",
        "a daemon that no longer lists the agent drops its page"
    );
    assert_eq!(launch_fields(&mut h), (None, None), "and its drafts");
}

#[gpui::test]
fn tab_from_a_text_field_follows_its_field_when_an_earlier_one_hides(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let mut agent = test_agent();
    let profile = agent
        .fields
        .iter()
        .find(|field| field.id == "profile")
        .cloned()
        .expect("the test agent has a text field");
    agent.fields.push(protocol::OptionField {
        id: "second".to_owned(),
        label: "second".to_owned(),
        when: Vec::new(),
        ..profile
    });
    list_agents(&mut h, agent);
    h.send(DaemonMessage::AgentCommands {
        commands: vec![
            command_entry("claude", "", ""),
            command_entry("test-agent", "", ""),
        ],
    });
    h.keys("ctrl-,");
    h.click_on("settings-tab-test");
    h.click_on("settings-test-second");
    type_line(&mut h, "x");
    let sandboxed = options(&[("sandbox", OptionValue::Bool(true))]);
    h.send(DaemonMessage::AgentSettings {
        settings: std::collections::BTreeMap::from([(test_agent_id(), sandboxed)]),
    });
    assert_eq!(
        h.root(|root, _| root.settings_page_fields("test")),
        ["mode", "sandbox", "second"],
        "the rule hid the field before the focused one"
    );
    h.sent();
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-agent-program"),
        "Tab leaves the focused field for the control after it"
    );
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(
            test_agent_id(),
            options(&[("sandbox", OptionValue::Bool(true)), ("second", text("x"))])
        )],
        "Tab applies the focused field's own value"
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
    tab_to(&mut h, "settings-claude-provider-deepseek");
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
        Some("settings-claude-provider-anthropic"),
        "the locked approval row leaves the ring; the provider stays"
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-claude-provider-deepseek")
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-agent-program"),
        "the launch rows stay"
    );
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-agent-args"));
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("list"));
    tab_to(&mut h, "settings-agent-program");
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

    list_agents(&mut h, test_agent());
    h.click_on("settings-tab-claude");
    h.keys("down");
    assert_eq!(
        shown_tab(&mut h),
        "Test agent",
        "Down walks on to the next agent's tab"
    );
    let mut dropped = welcomed();
    dropped.on_closed("the daemon went away".to_owned());
    h.event(NetEvent::State(dropped));
    assert!(
        !h.root(|root, _| root.settings_tab_rows())
            .contains(&"Agents"),
        "the group row is drawn only while the daemon lists agents"
    );
}

#[gpui::test]
fn tab_rows_list_every_tab_of_the_tab_enum_in_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    list_agents(&mut h, test_agent());
    h.keys("ctrl-,");
    let mut rows = h.root(|root, _| root.settings_tab_rows());
    // The list interleaves the "Agents" group row; the tabs it draws must be
    // the tabs Down and Up walk, in the same order, or a tab is unreachable.
    rows.retain(|label| *label != "Agents");
    assert_eq!(
        rows,
        h.root(|root, _| root.settings_tab_labels()),
        "the tab list's tabs are the walked tabs in order"
    );
    assert_eq!(
        rows.last().copied(),
        Some("Test agent"),
        "each listed agent's tab is walked"
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

/// The paths the client asked the daemon to set as the worktrees root.
fn worktrees_root_sets(sent: &[ClientMessage]) -> Vec<Option<String>> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetWorktreesRoot { path } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn a_refused_worktrees_save_stops_waiting(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(DaemonMessage::WorktreesRootChanged {
        root: "C:/default".to_owned(),
        is_override: false,
    });
    h.keys("ctrl-,");
    h.click_on("settings-tab-worktrees");
    assert_eq!(
        h.root(|root, _| root.worktrees_path()).as_deref(),
        Some(""),
        "the field holds the override only"
    );
    h.click_on("settings-worktrees-root-input");
    type_text(&mut h, "X:/nope");
    h.sent();
    h.click_on("settings-worktrees-root-save");
    assert_eq!(worktrees_root_sets(&h.sent()), [Some("X:/nope".to_owned())]);
    refuse(
        &mut h,
        "creating worktrees root X:\\nope: Access is denied. (os error 5)",
    );
    assert_eq!(toast_titles(&mut h).len(), 1, "the refusal still toasts");

    h.send(DaemonMessage::WorktreesRootChanged {
        root: "D:/other".to_owned(),
        is_override: true,
    });
    assert_eq!(
        h.root(|root, _| root.worktrees_path()).as_deref(),
        Some("X:/nope"),
        "a refused apply is not another client's change to overwrite"
    );
    assert_eq!(
        h.root(|root, _| root.worktrees_save_button()).0,
        "Save",
        "the save the daemon refused never shows Saved"
    );
    assert_eq!(
        h.root(|root, _| root.worktrees_active_line()),
        "Active: D:/other — user override",
        "another client's root still lands"
    );
}
