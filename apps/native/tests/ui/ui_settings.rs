//! Settings specs: the tab list and its keys, the General tab's keep-awake
//! and copy-on-select rows, the App title tab's toggles and preview, and the
//! window title they shape.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::time::Duration;

use gpui::{FontWeight, Modifiers, Point, TestAppContext, point, px};
use protocol::{
    AgentCommandEntry, AgentId, ClientMessage, DaemonMessage, KeepAwakeMode, OptionValue,
    OptionValues, SplitDirection, TabEntry, error_codes,
};
use rustling_tulip_native::{
    BadgeOverlay, Connection, LaunchCommandRow, NATIVE_PROTOCOL_VERSIONS, NetEvent, OpenOutcome,
    PopoutKey, SidebarView, TabBadge, UserHooksRow, chat::ChatMode, fonts,
};
use support::{Fixture, Harness, Opened, TestDir, pane, repo, session, split, tab};

#[cfg(windows)]
#[gpui::test]
fn the_toast_style_row_sets_the_choice(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.keys("ctrl-,");
    h.click_on("settings-tab-notifications");

    h.scroll_into_view("settings-content", "settings-toast-style-app");
    h.click_on("settings-toast-style-app");
    h.click_on("settings-apply");
    assert_eq!(saved_ui(&dir)["notifications"]["toast_style"], "app");

    h.click_on("settings-toast-style-os");
    h.click_on("settings-apply");
    assert_eq!(saved_ui(&dir)["notifications"]["toast_style"], "os");
}

#[gpui::test]
fn the_finished_grace_row_sets_the_choice(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.keys("ctrl-,");
    h.click_on("settings-tab-notifications");
    for (selector, secs) in [
        ("settings-finished-grace-0", 0),
        ("settings-finished-grace-2", 2),
        ("settings-finished-grace-5", 5),
        ("settings-finished-grace-10", 10),
        ("settings-finished-grace-20", 20),
    ] {
        assert!(
            h.bounds(selector).origin.x >= px(0.0),
            "{selector} is painted"
        );
        h.click_on(selector);
        h.click_on("settings-apply");
        assert_eq!(
            saved_ui(&dir)["notifications"]["finished_grace_secs"],
            serde_json::json!(secs)
        );
    }
}

#[gpui::test]
fn the_finished_grace_row_hides_with_the_toggle(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.keys("ctrl-,");
    h.click_on("settings-tab-notifications");
    h.click_on("settings-notify-finished");
    assert_eq!(settings_focus(&mut h), Some("settings-notify-finished"));
    h.keys("tab tab tab tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-notify-skip-on-screen"),
        "the row hides with the Finished toggle"
    );
    h.click_on("settings-notify-finished");
    assert_eq!(settings_focus(&mut h), Some("settings-notify-finished"));
    h.keys("tab tab tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-finished-grace-0"),
        "the row returns with the toggle"
    );
}

const TAB_SELECTORS: [&str; 8] = [
    "settings-tab-general",
    "settings-tab-notifications",
    "settings-tab-dashboard",
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
    settings_sets(&h.sent())
}

/// The `(agent, options)` of every `SetAgentSettings` among `sent`.
fn settings_sets(sent: &[ClientMessage]) -> Vec<(AgentId, OptionValues)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetAgentSettings { agent, options, .. } => {
                Some((agent.clone(), options.clone()))
            }
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
            "Dashboard",
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
            "Dashboard",
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
            "Dashboard",
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
    let footer_cancel = h.bounds("settings-cancel");
    assert!(
        footer_cancel.right() < panel.right() && footer_cancel.bottom() < panel.bottom(),
        "Cancel sits in the footer: {footer_cancel:?} in {panel:?}"
    );
}

#[gpui::test]
fn settings_panel_keeps_one_size_across_categories(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    let first = h.bounds("settings-panel").size;
    for selector in TAB_SELECTORS {
        h.click_on(selector);
        let size = h.bounds("settings-panel").size;
        assert_eq!(
            size, first,
            "{selector} resizes the panel: {first:?} became {size:?}"
        );
    }
}

/// The thumb the Settings content's scrollbar shows now, after a frame.
fn scroll_thumb(h: &mut Harness<'_>) -> Option<rustling_tulip_native::Thumb> {
    h.bounds("settings-content");
    let root = h.root.clone();
    h.cx.update(|_, cx| root.read(cx).settings_scroll_thumb())
}

#[gpui::test]
fn the_general_page_shows_a_scrollbar_when_it_overflows_and_the_short_page_shows_none(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    // The window the Settings shots are taken at.
    h.cx.simulate_resize(gpui::size(px(1000.0), px(640.0)));
    h.keys("ctrl-,");
    let viewport = h.bounds("settings-content").size.height / px(1.0);
    let thumb = scroll_thumb(&mut h).expect("General overflows, so it shows a thumb");
    assert!(
        thumb.top == 0.0 && thumb.height >= 24.0 && thumb.height < viewport,
        "a thumb at the top, shorter than the {viewport} px viewport: {thumb:?}"
    );

    h.click_on("settings-tab-app-title");
    assert_eq!(scroll_thumb(&mut h), None, "App title fits, so no thumb");
}

#[gpui::test]
fn down_moves_to_next_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    assert_eq!(settings_focus(&mut h), Some("list"), "it opens on the list");
    h.keys("down");
    assert_eq!(shown_tab(&mut h), "Notifications");
    h.keys("down");
    assert_eq!(shown_tab(&mut h), "Dashboard");
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
    h.click_on("settings-apply");
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
    h.scroll_into_view("settings-content", "settings-terminal-copy-on-selection");
    h.click_on("settings-terminal-copy-on-selection");
    h.click_on("settings-apply");
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
fn the_general_tab_has_no_open_text_externally_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    assert!(
        drawn(&mut h, "settings-repo-sort-active"),
        "the General tab is drawn"
    );
    assert!(
        !drawn(&mut h, "settings-general-open-text-externally"),
        "the link gestures replaced the row"
    );
}

#[gpui::test]
fn app_title_preview_follows_toggles(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-app-title");
    let preview = |h: &mut Harness<'_>| h.root(|root, _| root.title_preview());
    assert_eq!(preview(&mut h), "Tab name — rustling-tulip");
    assert!(h.bounds("settings-title-preview").origin.x >= px(0.0));

    h.click_on("settings-title-count");
    assert_eq!(
        preview(&mut h),
        "Tab name — rustling-tulip",
        "the count is the badge's, never the title's"
    );
    h.click_on("settings-title-product-suffix");
    assert_eq!(preview(&mut h), "Tab name");
    h.click_on("settings-title-count");
    assert_eq!(preview(&mut h), "Tab name");
    h.click_on("settings-apply");

    let saved = saved_ui(&dir);
    assert_eq!(saved["title"]["show_count"], true);
    assert_eq!(saved["title"]["suffix"], false);
}

/// Puts `s2`, in the active tab `work`, waiting on the owner, and lets the
/// title settle.
fn s2_needs_you(h: &mut Harness<'_>) {
    h.send(DaemonMessage::SessionUpdated {
        session: session("s2").status("awaiting_input").build(),
        request_id: None,
    });
    h.advance(Duration::from_millis(400));
}

/// The text of each overlay put on a taskbar button, oldest first; `None`
/// cleared it.
fn badge_texts(h: &mut Harness<'_>) -> Vec<Option<String>> {
    h.taskbar_badges()
        .into_iter()
        .map(|(_, overlay)| overlay.map(|overlay| overlay.text))
        .collect()
}

/// The overlay the taskbar button shows now.
fn shown_badge(h: &mut Harness<'_>) -> Option<BadgeOverlay> {
    h.taskbar_badges().pop().and_then(|(_, overlay)| overlay)
}

#[gpui::test]
fn the_badge_shows_the_needs_you_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    assert_eq!(
        badge_texts(&mut h),
        Vec::<Option<String>>::new(),
        "nothing needs you"
    );

    s2_needs_you(&mut h);
    let badge = shown_badge(&mut h).expect("a badge once s2 needs you");
    assert_eq!(badge.text, "1");
    assert_eq!(badge.description, "1 needs you");
    assert_eq!(
        badge.colour,
        h.root(|_, cx| rustling_tulip_native::ui_theme::theme(cx).danger)
    );

    h.click_on("tab-t2");
    h.advance(Duration::from_millis(400));
    assert_eq!(
        badge_texts(&mut h),
        vec![Some("1".to_owned())],
        "every tab counts, and an unchanged badge is set once"
    );
}

/// gpui's test platform draws every dirty window on each effect flush
/// (gpui `app.rs`, `flush_effects`), so a minimized window, which draws no
/// frame, cannot be simulated. What this pins instead: the badge waits out
/// the debounce after the session update, though the window drew at once,
/// so it is set from the update's timer and not from a frame.
#[gpui::test]
fn the_badge_updates_while_the_window_is_minimized(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(DaemonMessage::SessionUpdated {
        session: session("s2").status("awaiting_input").build(),
        request_id: None,
    });
    h.advance(Duration::from_millis(300));
    assert_eq!(badge_texts(&mut h), Vec::<Option<String>>::new(), "not yet");
    h.advance(Duration::from_millis(60));
    assert_eq!(badge_texts(&mut h), vec![Some("1".to_owned())]);
}

#[gpui::test]
fn a_session_in_two_tabs_counts_once_on_the_badge(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = two_panes();
    fixture.tabs[1] = named("t2", "other", &pane("p3", Some("s2")));
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.answer_scrollback("s2", b"");
    h.advance(Duration::from_millis(400));
    s2_needs_you(&mut h);
    let badge = shown_badge(&mut h).expect("a badge once s2 needs you");
    assert_eq!(
        (badge.text.as_str(), badge.description.as_str()),
        ("1", "1 needs you"),
        "s2 waits once, though two tabs show it"
    );
}

#[gpui::test]
fn the_badge_clears_at_zero(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    s2_needs_you(&mut h);
    h.send(DaemonMessage::SessionUpdated {
        session: session("s2").build(),
        request_id: None,
    });
    h.advance(Duration::from_millis(400));
    assert_eq!(badge_texts(&mut h), vec![Some("1".to_owned()), None]);
}

#[gpui::test]
fn the_window_title_no_longer_carries_the_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    s2_needs_you(&mut h);
    assert_eq!(title(&mut h), "work — rustling-tulip");
    assert_eq!(badge_texts(&mut h), vec![Some("1".to_owned())]);
}

#[gpui::test]
fn the_badge_follows_the_title_count_choice(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    s2_needs_you(&mut h);
    let pill = |h: &mut Harness<'_>| h.root(|root, _| root.tab_badge("t1"));
    assert_eq!(
        pill(&mut h),
        Some(TabBadge::NeedsYou(1)),
        "Needs you, the default"
    );
    assert_eq!(
        shown_badge(&mut h).map(|badge| badge.text).as_deref(),
        Some("1")
    );

    h.keys("ctrl-,");
    h.click_on("settings-tab-app-title");
    h.click_on("settings-count-busy");
    h.click_on("settings-apply");
    h.advance(Duration::from_millis(400));
    assert_eq!(pill(&mut h), Some(TabBadge::Busy { busy: 2, total: 2 }));
    let busy = shown_badge(&mut h).expect("a busy badge");
    assert_eq!(
        (busy.text.as_str(), busy.description.as_str()),
        ("2", "2 busy")
    );
    assert_eq!(saved_ui(&dir)["title"]["count"], "busy", "saved on Apply");

    h.click_on("settings-count-idle");
    h.click_on("settings-apply");
    h.advance(Duration::from_millis(400));
    assert_eq!(pill(&mut h), Some(TabBadge::Idle { idle: 0, total: 2 }));
    assert_eq!(shown_badge(&mut h), None, "no idle terminal: no badge");
    assert_eq!(saved_ui(&dir)["title"]["count"], "idle");

    h.click_on("settings-count-needs-you");
    h.click_on("settings-apply");
    h.advance(Duration::from_millis(400));
    assert_eq!(pill(&mut h), Some(TabBadge::NeedsYou(1)));
    assert_eq!(
        shown_badge(&mut h).map(|badge| badge.text).as_deref(),
        Some("1")
    );
    assert_eq!(saved_ui(&dir)["title"]["count"], "needs_you");

    h.click_on("settings-title-count");
    h.click_on("settings-apply");
    h.advance(Duration::from_millis(400));
    assert_eq!(
        shown_badge(&mut h),
        None,
        "the count toggle hides the badge"
    );
    assert_eq!(
        pill(&mut h),
        Some(TabBadge::NeedsYou(1)),
        "and leaves the pills"
    );
    assert_eq!(
        title(&mut h),
        "work — rustling-tulip",
        "the title never counts"
    );
}

#[gpui::test]
fn a_popout_carries_no_badge(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    s2_needs_you(&mut h);
    let key = PopoutKey::Pane {
        tab_id: "t1".to_owned(),
        pane_id: "p2".to_owned(),
    };
    let outcome = h.root_update(move |root, window, cx| root.open_popout(key, window, cx));
    assert_eq!(outcome, OpenOutcome::Opened);
    h.advance(Duration::from_millis(400));
    assert_eq!(h.window_count(), 2, "the pop-out has a window of its own");

    let main = h.cx.update(|window, _| window.window_handle().window_id());
    let shown = h.taskbar_badges();
    assert!(
        shown.iter().all(|(window, _)| *window == main),
        "only the main window carries a badge: {shown:?}"
    );
    assert_eq!(
        badge_texts(&mut h),
        vec![Some("1".to_owned())],
        "the popped session still counts on the main window's badge"
    );
}

#[gpui::test]
fn window_title_debounced_350ms(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    assert_eq!(title(&mut h), "work — rustling-tulip");

    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s2")),
    );
    h.send(DaemonMessage::TabUpdated {
        tab: named("t1", "build", &grid),
    });
    h.advance(Duration::from_millis(300));
    assert_eq!(title(&mut h), "work — rustling-tulip", "not yet");
    h.advance(Duration::from_millis(60));
    assert_eq!(title(&mut h), "build — rustling-tulip", "a rename");
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
        "work — rustling-tulip",
        "360 ms after the first change, the second restarted the wait"
    );
    h.advance(Duration::from_millis(180));
    assert_eq!(title(&mut h), "work — rustling-tulip", "not yet");
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
    assert_eq!(title(&mut h), "work — rustling-tulip", "not yet");
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
    h.click_on("settings-apply");
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
    assert!(
        h.root(|root, _| root.settings_have_pending_edits()),
        "the choice pends"
    );
    assert_eq!(
        saved_ui(&dir)["general"]["leaf_density"],
        "compact",
        "nothing saved before Apply"
    );
    assert_eq!(leaf_height(&mut h), px(26.0), "the leaves wait for Apply");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-leaf-density-compact")
    );
    h.click_on("settings-apply");
    assert_eq!(saved_ui(&dir)["general"]["leaf_density"], "comfortable");
    assert!(leaf_height(&mut h) > px(26.0), "two lines again");
}

#[gpui::test]
fn sessions_under_repos_choice_saves_and_is_reached_by_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);

    h.keys("ctrl-,");
    h.click_on("settings-sessions-under-repos-busy");
    h.click_on("settings-apply");
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
    h.click_on("settings-apply");
    assert_eq!(saved_ui(&dir)["general"]["repo_sort"], "active");
    h.click_on("settings-repo-sort-manual");
    h.click_on("settings-apply");
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
    h.click_on("settings-apply");
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
        h.root(|root, _| root.settings_have_pending_edits()),
        "Space presses the keep-awake choice"
    );
    assert!(
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::SetKeepAwakeMode { .. })),
        "the choice waits for Apply"
    );

    h.keys("tab tab tab tab tab tab tab tab tab tab tab tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-terminal-copy-on-selection"),
        "past Always, the two Default view, the two Leaf density, the three Sessions under repos \
         and the three Sort repos choices"
    );
    h.keys("space");
    h.keys("enter");

    h.keys("tab tab tab tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-general-check-updates")
    );
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-general-check-now"));
    for stop in [
        "settings-detailed-logging",
        "settings-open-logs-folder",
        "settings-ok",
        "settings-apply",
        "settings-cancel",
        "settings-search",
    ] {
        h.keys("tab");
        assert_eq!(
            settings_focus(&mut h),
            Some(stop),
            "the ring walks on past Updates"
        );
    }
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("list"), "back to the tab list");
    h.keys(&["shift-tab"; 12].join(" "));
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-terminal-copy-on-selection"),
        "Shift+Tab walks back"
    );
    h.keys(
        "shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab \
         shift-tab shift-tab shift-tab shift-tab",
    );
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-keep-awake-off"),
        "back past every General control to the first keep-awake choice"
    );
    h.keys("shift-tab");
    assert_eq!(settings_focus(&mut h), Some("list"));
    assert_eq!(shown_tab(&mut h), "General");

    h.sent();
    h.click_on("settings-apply");
    assert!(
        h.sent().iter().any(|msg| matches!(
            msg,
            ClientMessage::SetKeepAwakeMode {
                mode: KeepAwakeMode::WhenBusy
            }
        )),
        "Apply sends the keep-awake choice Space pressed"
    );
    assert_eq!(
        saved_ui(&dir)["general"]["copy_on_select"],
        true,
        "Space then Enter pressed copy-on-select off and on again"
    );
}

#[gpui::test]
fn tab_walks_app_title_controls_and_space_toggles_the_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.keys("down down down down down down");
    assert_eq!(shown_tab(&mut h), "App title");
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-count-needs-you"));
    h.keys("tab tab tab");
    assert_eq!(settings_focus(&mut h), Some("settings-title-count"));
    h.keys("space");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-title-product-suffix")
    );
    for stop in [
        "settings-ok",
        "settings-apply",
        "settings-cancel",
        "settings-search",
        "list",
    ] {
        h.keys("tab");
        assert_eq!(settings_focus(&mut h), Some(stop));
    }
    h.click_on("settings-apply");
    assert_eq!(saved_ui(&dir)["title"]["show_count"], false);
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
    h.click_on("settings-apply");
    assert_eq!(saved_ui(&dir)["sidebar_view"], "tabs", "saved on Apply");
    assert_eq!(
        h.root(|root, _| root.sidebar_view()),
        SidebarView::Tabs,
        "the open sidebar follows"
    );
    assert_eq!(tags(&mut h), ["TAB", "TAB"]);

    h.click_on("settings-sidebar-view-repos");
    assert_eq!(
        h.root(|root, _| root.sidebar_view()),
        SidebarView::Tabs,
        "the sidebar waits for Apply"
    );
    h.click_on("settings-apply");
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
    assert!(
        h.root(|root, _| root.settings_have_pending_edits()),
        "Space pends the choice"
    );
    assert_eq!(
        h.root(|root, _| root.sidebar_view()),
        SidebarView::Repos,
        "the sidebar waits for Apply"
    );
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
    assert!(
        !h.root(|root, _| root.settings_have_pending_edits()),
        "Enter presses the saved choice back"
    );
    h.keys("tab space");
    h.click_on("settings-apply");
    assert_eq!(saved_ui(&dir)["sidebar_view"], "tabs");
    assert_eq!(h.root(|root, _| root.sidebar_view()), SidebarView::Tabs);
}

const CHAT_WIDTH: &str = "settings-chat-width-input";
const CHAT_WIDTH_ERROR: &str = "Enter a whole number of pixels, 0 or more.";

/// Clicks the General tab's `selector` once it is scrolled into view.
fn click_general(h: &mut Harness<'_>, selector: &str) {
    h.scroll_into_view("settings-content", selector);
    h.click_on(selector);
}

/// Empties the Maximum chat width field and types `text` into it.
fn type_chat_width(h: &mut Harness<'_>, text: &str) {
    click_general(h, CHAT_WIDTH);
    h.keys("ctrl-a backspace");
    if !text.is_empty() {
        type_text(h, text);
    }
}

fn chat_width_draft(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.chat_width_draft())
}

fn chat_width_error(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.chat_width_error())
}

/// The saved `chat_max_width`, if `native-ui.json` was written and holds
/// one.
fn saved_chat_width(dir: &TestDir) -> Option<serde_json::Value> {
    std::fs::read_to_string(dir.path().join("native-ui.json"))
        .ok()
        .and_then(|text| {
            let saved: serde_json::Value =
                serde_json::from_str(&text).expect("native-ui.json is JSON");
            saved.get("chat_max_width").cloned()
        })
}

#[gpui::test]
fn the_chat_view_choice_pends_until_apply_and_saves(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    assert_eq!(apply_gate(&mut h), (false, false), "nothing is pending");
    click_general(&mut h, "settings-chat-mode-comfortable");
    assert_eq!(apply_gate(&mut h), (true, true), "the choice pends");
    assert_eq!(
        h.root(|root, _| root.chat_mode()),
        ChatMode::Compact,
        "chat panes wait for Apply"
    );
    h.click_on("settings-apply");
    assert_eq!(saved_ui(&dir)["chat_mode"], "comfortable", "saved on Apply");
    assert_eq!(h.root(|root, _| root.chat_mode()), ChatMode::Comfortable);
    assert_eq!(apply_gate(&mut h), (false, false), "Apply took it");

    h.click_on("settings-chat-mode-compact");
    h.click_on("settings-chat-mode-comfortable");
    assert_eq!(
        apply_gate(&mut h),
        (false, false),
        "pressed back to the saved choice"
    );
    h.click_on("settings-chat-mode-compact");
    h.click_on("settings-ok");
    assert_eq!(saved_ui(&dir)["chat_mode"], "compact", "OK saves it too");
    assert_eq!(h.root(|root, _| root.chat_mode()), ChatMode::Compact);
}

#[gpui::test]
fn the_chat_width_pends_until_apply_and_saves(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    assert_eq!(
        chat_width_draft(&mut h),
        Some("960".to_owned()),
        "the field holds the saved width"
    );
    assert_eq!(apply_gate(&mut h), (false, false), "nothing is pending");
    type_chat_width(&mut h, "720");
    assert_eq!(apply_gate(&mut h), (true, true), "the width pends");
    assert_eq!(chat_width_error(&mut h), None);
    assert_ne!(
        saved_chat_width(&dir),
        Some(serde_json::json!(720)),
        "nothing saved before Apply"
    );
    h.click_on("settings-apply");
    assert_eq!(saved_ui(&dir)["chat_max_width"], 720, "saved on Apply");
    assert_eq!(apply_gate(&mut h), (false, false), "Apply took it");

    type_chat_width(&mut h, "0");
    assert_eq!(apply_gate(&mut h), (true, true), "0 is a width: no limit");
    h.click_on("settings-apply");
    assert_eq!(saved_ui(&dir)["chat_max_width"], 0);
}

#[gpui::test]
fn an_invalid_chat_width_shuts_the_apply_gate(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    for text in ["wide", "4294967296", "12.5", ""] {
        type_chat_width(&mut h, text);
        assert_eq!(
            apply_gate(&mut h),
            (true, false),
            "{text:?} pends and fails validation"
        );
        assert_eq!(
            chat_width_error(&mut h),
            Some(CHAT_WIDTH_ERROR.to_owned()),
            "{text:?}"
        );
        assert!(drawn(&mut h, "settings-chat-width-error"), "{text:?}");
    }
    h.click_on("settings-apply");
    assert!(
        saved_chat_width(&dir).is_none_or(|width| width == 960),
        "a shut gate saves nothing"
    );

    type_chat_width(&mut h, "100000");
    assert_eq!(
        apply_gate(&mut h),
        (true, true),
        "a width above 65535 passes"
    );
    assert_eq!(chat_width_error(&mut h), None);
    h.click_on("settings-apply");
    assert_eq!(saved_ui(&dir)["chat_max_width"], 100_000);
}

#[gpui::test]
fn a_saved_width_above_65535_leaves_apply_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "chat_max_width": 100000 }"#,
    )
    .expect("the saved layout is written");
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    assert_eq!(
        chat_width_draft(&mut h),
        Some("100000".to_owned()),
        "the field holds the saved width"
    );
    assert_eq!(chat_width_error(&mut h), None, "the saved width is a width");
    assert_eq!(
        apply_gate(&mut h),
        (false, false),
        "shut only because nothing is pending"
    );
    click_general(&mut h, "settings-terminal-copy-on-selection");
    assert_eq!(apply_gate(&mut h), (true, true), "a toggle edit opens it");
}

#[gpui::test]
fn the_general_ring_reaches_the_chat_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    click_general(&mut h, "settings-terminal-copy-on-selection");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-terminal-copy-on-selection")
    );
    for stop in [
        "settings-chat-mode-compact",
        "settings-chat-mode-comfortable",
        CHAT_WIDTH,
        "settings-general-check-updates",
    ] {
        h.keys("tab");
        assert_eq!(settings_focus(&mut h), Some(stop), "Tab walks on to {stop}");
    }
    for stop in [CHAT_WIDTH, "settings-chat-mode-comfortable"] {
        h.keys("shift-tab");
        assert_eq!(settings_focus(&mut h), Some(stop), "Shift+Tab walks back");
    }
}

#[gpui::test]
fn cancel_discards_a_chat_width_draft(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    for close in ["settings-cancel", "escape"] {
        h.keys("ctrl-,");
        type_chat_width(&mut h, "720");
        assert_eq!(apply_gate(&mut h), (true, true));
        if close == "escape" {
            h.keys("escape");
        } else {
            h.click_on(close);
        }
        assert!(!h.root(|root, _| root.settings_open()), "{close} closes");
        assert_ne!(
            saved_chat_width(&dir),
            Some(serde_json::json!(720)),
            "{close} saves nothing"
        );
        h.keys("ctrl-,");
        assert_eq!(
            chat_width_draft(&mut h),
            Some("960".to_owned()),
            "{close} put the field back to the saved width"
        );
        assert_eq!(apply_gate(&mut h), (false, false), "{close} left nothing");
        h.keys("escape");
    }
}

#[gpui::test]
fn the_claude_page_saves_the_permission_mode(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
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
    h.click_on("settings-apply");
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
}

#[gpui::test]
fn a_saved_trusted_default_becomes_each_hosts_bypass_choice(cx: &mut TestAppContext) {
    use rustling_tulip_native::target::HostKey;
    let dir = TestDir::new();
    support::seed_ui(
        &dir,
        &serde_json::json!({
            "spawn": { "trusted": true },
            "saved_hosts": [{ "name": support::REMOTE, "port": 8787 }],
        }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let bypass = options(&[("permission_mode", text("bypass_permissions"))]);
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(claude(), bypass.clone())],
        "This PC's Claude Settings take the bypass choice"
    );
    assert_eq!(
        saved_ui(&dir)["spawn"]["trusted_moved"],
        serde_json::json!(["@local"])
    );
    broadcast_claude(&mut h, OptionValues::new());
    assert!(
        sent_agent_settings(&mut h).is_empty(),
        "a host takes the choice once"
    );

    let remote = support::saved_remote();
    let key = match HostKey::of(&remote) {
        HostKey::Remote(key) => key,
        HostKey::Local => String::new(),
    };
    let sent = {
        let mut host = h.host(&remote);
        host.connect_keeping_sent();
        host.sent()
    };
    assert_eq!(
        settings_sets(&sent),
        vec![(claude(), bypass)],
        "a host that connects later takes it too"
    );
    assert_eq!(
        saved_ui(&dir)["spawn"]["trusted_moved"],
        serde_json::json!(["@local", key])
    );
}

/// The registry's descriptors as a daemon from before the bypass choice
/// lists them: none names one.
fn descriptors_without_bypass() -> Vec<protocol::AgentDescriptor> {
    agent_registry::descriptors()
        .into_iter()
        .map(|mut descriptor| {
            descriptor.bypass = None;
            descriptor
        })
        .collect()
}

/// The window over a saved Trusted default and [`support::REMOTE`] saved,
/// This PC's move already sent and read; returns the remote target and its
/// `trusted_moved` key.
fn trusted_default_with_a_remote<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
) -> (Harness<'a>, rustling_tulip_native::target::Target, String) {
    use rustling_tulip_native::target::HostKey;
    support::seed_ui(
        dir,
        &serde_json::json!({
            "spawn": { "trusted": true },
            "saved_hosts": [{ "name": support::REMOTE, "port": 8787 }],
        }),
    );
    let mut h = Harness::with(cx, dir, &two_panes());
    h.answer_scrollback("s1", b"");
    h.answer_scrollback("s2", b"");
    h.advance(Duration::from_millis(400));
    h.sent();
    let remote = support::saved_remote();
    let key = match HostKey::of(&remote) {
        HostKey::Remote(key) => key,
        HostKey::Local => String::new(),
    };
    (h, remote, key)
}

/// `remote` connects listing descriptors without a bypass choice: nothing
/// is sent to it and it stays unrecorded.
fn connect_an_older_daemon(
    h: &mut Harness<'_>,
    dir: &TestDir,
    remote: &rustling_tulip_native::target::Target,
) {
    let sent = {
        let mut host = h.host(remote);
        host.connect_listing(descriptors_without_bypass());
        host.sent()
    };
    assert!(
        settings_sets(&sent).is_empty(),
        "an older daemon names no bypass choice: {sent:?}"
    );
    assert_eq!(
        saved_ui(dir)["spawn"]["trusted_moved"],
        serde_json::json!(["@local"]),
        "and is not recorded"
    );
}

#[gpui::test]
fn a_host_without_a_bypass_choice_is_moved_once_it_has_one(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, remote, key) = trusted_default_with_a_remote(cx, &dir);
    connect_an_older_daemon(&mut h, &dir, &remote);

    let sent = {
        let mut host = h.host(&remote);
        host.connect_keeping_sent();
        host.sent()
    };
    assert_eq!(
        settings_sets(&sent),
        vec![(
            claude(),
            options(&[("permission_mode", text("bypass_permissions"))])
        )],
        "the updated daemon takes the choice"
    );
    assert_eq!(
        saved_ui(&dir)["spawn"]["trusted_moved"],
        serde_json::json!(["@local", key])
    );
}

#[gpui::test]
fn a_permission_mode_set_before_the_move_is_never_overwritten(cx: &mut TestAppContext) {
    use rustling_tulip_native::target::HostKey;
    let dir = TestDir::new();
    let (mut h, remote, key) = trusted_default_with_a_remote(cx, &dir);
    connect_an_older_daemon(&mut h, &dir, &remote);

    h.keys("ctrl-,");
    let row = format!(
        "settings-host-{}",
        HostKey::of(&remote)
            .wire()
            .expect("a remote host has a wire key")
    );
    h.click_on("settings-tab-claude");
    h.click_on(&row);
    h.click_on("settings-claude-permission-mode-plan");
    h.click_on("settings-apply");
    assert_eq!(
        settings_sets(&h.host(&remote).sent()),
        vec![(claude(), options(&[("permission_mode", text("plan"))]))],
        "the user's own choice goes to the host"
    );
    assert_eq!(
        saved_ui(&dir)["spawn"]["trusted_moved"],
        serde_json::json!(["@local", key]),
        "a host the user configured is recorded"
    );

    let sent = {
        let mut host = h.host(&remote);
        host.connect_keeping_sent();
        host.sent()
    };
    assert!(
        settings_sets(&sent).is_empty(),
        "the updated daemon keeps the user's Plan: {sent:?}"
    );
}

#[gpui::test]
fn an_untrusted_last_choice_moves_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    support::seed_ui(
        &dir,
        &serde_json::json!({ "spawn": { "trusted": true, "last_trusted": false } }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    assert!(
        sent_agent_settings(&mut h).is_empty(),
        "the last dialog's choice was not trusted"
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
        claude_choice(&mut h, "effort").as_deref(),
        Some("CLI default"),
        "the effort row is on the page, showing its no-value choice"
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
        "settings-claude-effort-cli-default",
        "settings-claude-effort-low",
        "settings-claude-effort-medium",
        "settings-claude-effort-high",
        "settings-claude-effort-xhigh",
        "settings-claude-effort-max",
    ] {
        assert_eq!(
            settings_focus(&mut h),
            Some(stop),
            "permission mode, then provider, then effort, in the descriptor's order"
        );
        h.keys("tab");
    }
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-cancel"),
        "no other field is on the page before the daemon reports its command: the footer's \
         Cancel follows"
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
    h.click_on("settings-apply");
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
    h.click_on("settings-apply");
    let anthropic = options(&[("provider", text("anthropic"))]);
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(claude(), anthropic.clone())]
    );
    broadcast_claude(&mut h, anthropic);
    assert_eq!(provider_choice(&mut h).as_deref(), Some("Anthropic"));
    h.click_on("settings-claude-provider-deepseek");
    h.click_on("settings-apply");
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
    h.click_on("settings-apply");
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
    h.click_on("settings-apply");
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
    h.click_on("settings-apply");
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
    h.click_on("settings-apply");
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
    conn.on_welcome(version, support::daemon_features(), None);
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
    h.click_on("settings-apply");
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

const PROGRAM_REFUSAL: &str =
    "The launch command's Program cannot contain a newline or other control character.";
const UNCLOSED_QUOTE: &str = "The extra arguments have an unclosed quote.";
const RESERVED_FLAG: &str =
    "--settings is set by rustling-tulip; remove it from the extra arguments.";
const ARGS_CONTROL: &str =
    "The launch command's Extra arguments cannot contain a newline or other control character.";
const WORKTREES_ROOT_REFUSAL: &str =
    "creating worktrees root X:\\nope: Access is denied. (os error 5)";

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
                ..
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

/// The error code the daemon sends with each refusal this spec fakes.
fn refusal_code(message: &str) -> &'static str {
    match message {
        PROGRAM_REFUSAL => error_codes::LAUNCH_PROGRAM_REFUSED,
        UNCLOSED_QUOTE | RESERVED_FLAG | ARGS_CONTROL => error_codes::LAUNCH_ARGS_REFUSED,
        WORKTREES_ROOT_REFUSAL => error_codes::WORKTREES_ROOT_REFUSED,
        PROFILE_REFUSAL => error_codes::SETTINGS_REFUSED,
        _ => error_codes::INTERNAL,
    }
}

/// The daemon refuses with `message` and its [`refusal_code`], answering
/// the request `request_id` names, or none.
fn refuse(h: &mut Harness<'_>, request_id: Option<&str>, message: &str) {
    h.send(DaemonMessage::Error {
        message: message.to_owned(),
        request_id: request_id.map(str::to_owned),
        code: Some(refusal_code(message).to_owned()),
    });
}

/// The request id of the last launch command among `sent`.
fn last_command_id(sent: &[ClientMessage]) -> Option<String> {
    sent.iter()
        .rev()
        .find_map(|msg| match msg {
            ClientMessage::SetAgentCommand { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .flatten()
}

/// The request id of the last Settings values among `sent`.
fn last_settings_id(sent: &[ClientMessage]) -> Option<String> {
    sent.iter()
        .rev()
        .find_map(|msg| match msg {
            ClientMessage::SetAgentSettings { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .flatten()
}

/// The daemon refuses the last launch command the client sent with
/// `message`; returns the request id it answered.
fn refuse_last_command(h: &mut Harness<'_>, message: &str) -> Option<String> {
    let id = last_command_id(&h.sent());
    refuse(h, id.as_deref(), message);
    id
}

fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

const USER_HOOKS_PATH: &str = "C:\\Users\\me\\.agent\\settings.json";

/// The agents each user-hooks Get among `sent` asks about, in order.
fn user_hooks_gets(sent: &[ClientMessage]) -> Vec<AgentId> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::GetAgentUserHooks { agent, .. } => Some(agent.clone()),
            _ => None,
        })
        .collect()
}

/// The request id of the last user-hooks Get among `sent`.
fn last_user_hooks_get_id(sent: &[ClientMessage]) -> Option<String> {
    sent.iter()
        .rev()
        .find_map(|msg| match msg {
            ClientMessage::GetAgentUserHooks { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .flatten()
}

/// Each user-hooks Set among `sent`: its agent, value and request id.
fn user_hooks_sets(sent: &[ClientMessage]) -> Vec<(AgentId, bool, Option<String>)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetAgentUserHooks {
                agent,
                enabled,
                request_id,
            } => Some((agent.clone(), *enabled, request_id.clone())),
            _ => None,
        })
        .collect()
}

/// The daemon's answer for Claude's user hooks.
fn user_hooks_reply(
    state: protocol::UserHooksWire,
    error: Option<&str>,
    request_id: Option<String>,
) -> DaemonMessage {
    DaemonMessage::AgentUserHooks {
        agent: claude(),
        state,
        path: Some(USER_HOOKS_PATH.to_owned()),
        error: error.map(str::to_owned),
        request_id,
    }
}

/// [`opened`] on a daemon that lists the user-hooks feature, Claude's Get
/// answered with `state`, and Settings open on Claude's page with the Hooks
/// switch in view.
fn opened_with_user_hooks<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    state: protocol::UserHooksWire,
) -> Harness<'a> {
    let mut h = opened(cx, dir);
    reconnect_with_user_hooks(&mut h, true);
    let get = last_user_hooks_get_id(&h.sent());
    assert!(get.is_some(), "the client asked for Claude's user hooks");
    h.send(user_hooks_reply(state, None, get));
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    h.scroll_into_view("settings-content", "settings-agent-user-hooks");
    h
}

/// The local daemon's connection is reported again listing the current
/// features, and the user-hooks one when `user_hooks`, then the daemon lists
/// its agents; what the client sends is left for `sent`.
fn reconnect_with_user_hooks(h: &mut Harness<'_>, user_hooks: bool) {
    let mut features = support::daemon_features();
    if user_hooks {
        features.push(protocol::FEATURE_AGENT_USER_HOOKS.to_owned());
    }
    let mut conn = Connection::new();
    conn.on_connecting();
    conn.on_socket_open(4242);
    conn.on_welcome(support::PROTOCOL, features, None);
    h.event(NetEvent::State(conn));
    h.send(DaemonMessage::Agents {
        agents: agent_registry::descriptors(),
    });
}

fn user_hooks_row(h: &mut Harness<'_>) -> Option<UserHooksRow> {
    h.root(|root, _| root.settings_user_hooks_row())
}

#[gpui::test]
fn user_hooks_get_sent_when_agents_arrive(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    reconnect_with_user_hooks(&mut h, true);
    assert_eq!(
        user_hooks_gets(&h.sent()),
        vec![claude()],
        "the agent whose descriptor has user_hooks is asked about"
    );
    list_agents(&mut h, test_agent());
    assert_eq!(
        user_hooks_gets(&h.sent()),
        vec![claude()],
        "a second listing asks again, and never about an agent without the flag"
    );
}

#[gpui::test]
fn user_hooks_toggle_sends_set_and_shows_the_reply(cx: &mut TestAppContext) {
    use protocol::UserHooksWire;
    let dir = TestDir::new();
    let mut h = opened_with_user_hooks(cx, &dir, UserHooksWire::Off);
    let row = user_hooks_row(&mut h).expect("the Hooks section is drawn");
    assert!(row.enabled && !row.checked, "off and live: {row:?}");
    assert_eq!(row.path.as_deref(), Some(USER_HOOKS_PATH));
    assert!(drawn(&mut h, "settings-agent-user-hooks-path"));

    h.click_on("settings-agent-user-hooks");
    let sets = user_hooks_sets(&h.sent());
    assert_eq!(sets.len(), 1, "one Set: {sets:?}");
    let (agent, enabled, id) = sets[0].clone();
    assert_eq!((agent, enabled), (claude(), true));
    assert!(id.is_some(), "the Set carries a request id");
    let row = user_hooks_row(&mut h).expect("still drawn");
    assert!(
        row.checked && row.pending,
        "the asked-for value shows: {row:?}"
    );

    h.send(user_hooks_reply(UserHooksWire::Stale, None, id));
    let row = user_hooks_row(&mut h).expect("still drawn");
    assert!(
        row.checked && !row.pending && row.repair,
        "the reply's state shows: {row:?}"
    );
    assert!(drawn(&mut h, "settings-agent-user-hooks-repair"));

    h.click_on("settings-agent-user-hooks");
    let sets = user_hooks_sets(&h.sent());
    assert_eq!(sets.len(), 1, "one Set: {sets:?}");
    assert!(!sets[0].1, "a click on the switch shown on asks for off");
    let row = user_hooks_row(&mut h).expect("still drawn");
    assert!(!row.checked && row.pending, "{row:?}");
    h.send(user_hooks_reply(
        UserHooksWire::Off,
        None,
        sets[0].2.clone(),
    ));
    let row = user_hooks_row(&mut h).expect("still drawn");
    assert!(!row.checked && !row.pending && !row.repair, "{row:?}");
}

#[gpui::test]
fn user_hooks_section_disabled_without_the_feature(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    reconnect_with_user_hooks(&mut h, false);
    assert_eq!(
        user_hooks_gets(&h.sent()),
        Vec::<AgentId>::new(),
        "nothing is asked of a daemon without the feature"
    );
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    h.scroll_into_view("settings-content", "settings-agent-user-hooks");
    let row = user_hooks_row(&mut h).expect("the Hooks section is drawn, dimmed");
    assert!(!row.enabled && !row.checked, "{row:?}");
    assert_eq!(
        h.root(|root, _| root.settings_user_hooks_tip()),
        Some(
            "This host's daemon is too old for this setting. Update rustling-tulip there to use it."
        )
    );
    assert!(drawn(&mut h, "settings-agent-user-hooks"));
    h.click_on("settings-agent-user-hooks");
    assert_eq!(
        user_hooks_sets(&h.sent()),
        Vec::new(),
        "a click on the dimmed switch sends nothing"
    );
}

#[gpui::test]
fn user_hooks_error_shown_under_the_row(cx: &mut TestAppContext) {
    use protocol::UserHooksWire;
    let dir = TestDir::new();
    let mut h = opened_with_user_hooks(cx, &dir, UserHooksWire::Off);
    assert!(!drawn(&mut h, "settings-agent-user-hooks-error"));
    h.click_on("settings-agent-user-hooks");
    let id = user_hooks_sets(&h.sent()).pop().and_then(|(_, _, id)| id);
    let refusal = "writing C:\\Users\\me\\.agent\\settings.json: access denied";
    h.send(user_hooks_reply(UserHooksWire::Off, Some(refusal), id));
    let row = user_hooks_row(&mut h).expect("still drawn");
    assert_eq!(row.error.as_deref(), Some(refusal));
    assert!(
        !row.checked && !row.pending,
        "the file's state shows: {row:?}"
    );
    assert!(drawn(&mut h, "settings-agent-user-hooks-error"));

    h.send(user_hooks_reply(UserHooksWire::Off, None, None));
    assert_eq!(
        user_hooks_row(&mut h).and_then(|row| row.error),
        None,
        "the next reply clears the error"
    );
    assert!(!drawn(&mut h, "settings-agent-user-hooks-error"));
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
        Some("settings-claude-effort-cli-default"),
        "the effort row follows the provider"
    );
    tab_to(&mut h, "settings-claude-effort-max");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-cancel"),
        "the ring holds no launch rows before the daemon reports: the footer's Cancel follows"
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
    h.click_on("settings-apply");
    assert_eq!(
        launch_sets(&h.sent()),
        Vec::<(String, String)>::new(),
        "nothing is sent"
    );
    h.click_on("settings-claude-provider-deepseek");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-claude-effort-cli-default"),
        "the effort row follows the provider"
    );
    tab_to(&mut h, "settings-claude-effort-max");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-ok"),
        "Tab skips the disabled launch rows to the footer, whose OK takes the pending provider"
    );
    h.click_on("settings-apply");
    let sent = h.sent();
    assert_eq!(
        settings_sets(&sent),
        vec![(claude(), options(&[("provider", text("deepseek"))]))],
        "Apply sends the provider"
    );
    assert_eq!(
        launch_sets(&sent),
        Vec::<(String, String)>::new(),
        "and still no launch command"
    );

    h.welcome_with_features();
    assert_eq!(h.root(|root, _| root.settings_launch_command_tip()), None);
}

#[gpui::test]
fn apply_sends_both_launch_drafts_trimmed(cx: &mut TestAppContext) {
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
        Vec::<(String, String)>::new(),
        "Enter sends nothing"
    );
    h.click_on("settings-apply");
    assert_eq!(
        launch_sets(&h.sent()),
        [("wrapper".to_owned(), "verbose".to_owned())]
    );
}

#[gpui::test]
fn apply_sends_the_untyped_program_with_the_args(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("wrapper", "", None));
    h.click_on("settings-agent-args");
    type_line(&mut h, "verbose");
    h.click_on("settings-apply");
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
    h.click_on("settings-apply");
    assert_eq!(
        launch_sets(&h.sent()),
        [(String::new(), String::new())],
        "Apply on empty rows clears the command"
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
    h.click_on("settings-apply");
    assert_eq!(launch_sets(&h.sent()).len(), 1);
    assert!(
        !h.root(|root, _| root.launch_saved()),
        "not Saved before the echo"
    );
    h.send(agent_commands("wrapper", "", None));
    assert!(h.root(|root, _| root.launch_saved()));
    assert!(drawn(&mut h, "settings-agent-saved"));
    h.click_on("settings-agent-program");
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
    h.click_on("settings-apply");
    let id = refuse_last_command(&mut h, PROGRAM_REFUSAL);
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

    refuse(&mut h, id.as_deref(), PROGRAM_REFUSAL);
    assert_eq!(
        toast_titles(&mut h).len(),
        1,
        "the claimed refusal ended the apply, so another for its id toasts"
    );
}

#[gpui::test]
fn args_refusal_shows_under_extra_arguments(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    for text in [UNCLOSED_QUOTE, RESERVED_FLAG, ARGS_CONTROL] {
        h.click_on("settings-agent-args");
        type_line(&mut h, "x");
        h.click_on("settings-apply");
        refuse_last_command(&mut h, text);
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
    h.click_on("settings-apply");
    refuse_last_command(&mut h, UNCLOSED_QUOTE);
    assert!(refusal(&mut h).is_some());
    h.click_on("settings-agent-args");
    type_line(&mut h, "x");
    assert_eq!(refusal(&mut h), None, "the edit clears the refusal");
}

#[gpui::test]
fn unrelated_error_while_pending_still_toasts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-program");
    type_line(&mut h, "wrapper");
    h.click_on("settings-apply");
    let id = last_command_id(&h.sent());
    refuse(&mut h, None, "worktree delete failed");
    assert_eq!(
        refusal(&mut h),
        None,
        "another refusal is not the launch command's"
    );
    assert_eq!(toast_titles(&mut h).len(), 1, "it toasts as before");
    refuse(&mut h, id.as_deref(), PROGRAM_REFUSAL);
    assert_eq!(
        refusal(&mut h),
        Some((LaunchCommandRow::Program, PROGRAM_REFUSAL.to_owned())),
        "the unrouted error answered no apply, so the apply's own refusal still lands"
    );
    assert_eq!(toast_titles(&mut h).len(), 1, "and raises no toast");
}

#[gpui::test]
fn reselecting_the_claude_tab_keeps_drafts_and_pending_apply(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-program");
    type_line(&mut h, "wrapper");
    h.click_on("settings-apply");
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
    refuse_last_command(&mut h, PROGRAM_REFUSAL);
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
    h.click_on("settings-apply");
    h.click_on("settings-tab-general");
    refuse_last_command(&mut h, PROGRAM_REFUSAL);
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
    h.click_on("settings-apply");
    refuse_last_command(&mut h, "worktree delete failed");
    assert_eq!(
        toast_titles(&mut h).len(),
        1,
        "a refusal with no launch-row text toasts"
    );
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
    dropped.on_welcome(version, support::daemon_features(), None);
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
                ..
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
    h.click_on("settings-apply");
    assert_eq!(
        command_sets(&h.sent()),
        [
            ("claude".to_owned(), "wrapper".to_owned(), String::new()),
            ("test-agent".to_owned(), String::new(), "verbose".to_owned())
        ],
        "Apply sends each page's pending command, each to its own agent"
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
    h.click_on("settings-apply");
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
        Some("settings-cancel"),
        "the keyboard moves to the stop after profile in the full order: past the launch rows \
         the daemon has not reported and the disabled OK and Apply, to Cancel"
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-search"),
        "Tab walks on from there"
    );
}

#[gpui::test]
fn a_text_row_edit_sends_nothing_until_apply(cx: &mut TestAppContext) {
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
    assert_eq!(
        sent_agent_settings(&mut h),
        Vec::new(),
        "Enter sends nothing"
    );
    assert!(
        h.root(|root, _| root.settings_open()),
        "Enter in a text row does not close Settings"
    );
    h.keys("tab");
    assert_eq!(
        sent_agent_settings(&mut h),
        Vec::new(),
        "leaving the field sends nothing"
    );
    h.click_on("settings-apply");
    let work = options(&[("profile", text("work"))]);
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(test_agent_id(), work.clone())],
        "Apply sends the field"
    );
    assert!(
        h.root(|root, _| root.settings_open()),
        "Apply keeps Settings open"
    );
    h.send(DaemonMessage::AgentSettings {
        settings: std::collections::BTreeMap::from([(test_agent_id(), work)]),
    });
    h.click_on("settings-apply");
    assert_eq!(
        sent_agent_settings(&mut h),
        Vec::new(),
        "a second Apply has nothing left to send"
    );
}

#[gpui::test]
fn closing_settings_no_longer_applies_edited_agent_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    list_agents(&mut h, test_agent());
    h.keys("ctrl-,");
    h.click_on("settings-tab-test");
    h.click_on("settings-test-profile");
    type_line(&mut h, "work");
    h.sent();
    h.click_on("settings-close");
    assert!(!h.root(|root, _| root.settings_open()));
    assert_eq!(
        sent_agent_settings(&mut h),
        Vec::new(),
        "closing Settings sends nothing"
    );
    h.keys("ctrl-,");
    h.click_on("settings-tab-test");
    h.click_on("settings-apply");
    assert_eq!(
        sent_agent_settings(&mut h),
        Vec::new(),
        "the reopened page holds the saved value, so nothing is pending"
    );
}

/// The harness on [`opened`] with the test agent listed beside Claude and
/// both agents' launch commands empty, Settings open on the test agent's
/// page, nothing sent yet.
fn two_agent_pages<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = opened(cx, dir);
    list_agents(&mut h, test_agent());
    h.send(DaemonMessage::AgentCommands {
        commands: vec![
            command_entry("claude", "", ""),
            command_entry("test-agent", "", ""),
        ],
    });
    h.keys("ctrl-,");
    h.click_on("settings-tab-test");
    h.sent();
    h
}

#[gpui::test]
fn apply_sends_every_pending_edit_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_agent_pages(cx, &dir);
    h.click_on("settings-test-profile");
    type_line(&mut h, "work");
    h.click_on("settings-agent-args");
    type_line(&mut h, "verbose");
    h.click_on("settings-tab-claude");
    h.click_on("settings-agent-program");
    type_line(&mut h, "wrapper");
    no_setting_sent(&mut h, "editing sends nothing");

    h.click_on("settings-apply");
    let sent = h.sent();
    assert_eq!(
        settings_sets(&sent),
        vec![(test_agent_id(), options(&[("profile", text("work"))]))],
        "the text row goes once"
    );
    assert_eq!(
        command_sets(&sent),
        [
            ("claude".to_owned(), "wrapper".to_owned(), String::new()),
            ("test-agent".to_owned(), String::new(), "verbose".to_owned())
        ],
        "each launch command goes once"
    );
    h.click_on("settings-apply");
    no_setting_sent(
        &mut h,
        "a second Apply before the echoes sends nothing again",
    );
}

#[gpui::test]
fn apply_stays_disabled_with_nothing_pending(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_agent_pages(cx, &dir);
    h.click_on("settings-apply");
    h.click_on("settings-ok");
    assert!(
        h.root(|root, _| root.settings_open()),
        "a disabled OK does not close"
    );
    no_setting_sent(&mut h, "a disabled Apply sends nothing");

    h.click_on("settings-agent-args");
    type_line(&mut h, "x");
    h.keys("backspace");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-cancel"),
        "Tab passes over the disabled OK and Apply"
    );
    h.click_on("settings-apply");
    no_setting_sent(
        &mut h,
        "an edit typed back to the saved value is not pending",
    );
}

/// Asserts the client sent no agent's Settings values and no launch
/// command since the last look.
fn no_setting_sent(h: &mut Harness<'_>, why: &str) {
    let sent = h.sent();
    assert_eq!(settings_sets(&sent), Vec::new(), "{why}");
    assert_eq!(
        command_sets(&sent),
        Vec::<(String, String, String)>::new(),
        "{why}"
    );
}

#[gpui::test]
fn ok_applies_then_closes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_agent_pages(cx, &dir);
    h.click_on("settings-test-profile");
    type_line(&mut h, "work");
    h.click_on("settings-agent-args");
    type_line(&mut h, "verbose");
    h.click_on("settings-ok");
    let sent = h.sent();
    assert_eq!(
        settings_sets(&sent),
        vec![(test_agent_id(), options(&[("profile", text("work"))]))]
    );
    assert_eq!(
        command_sets(&sent),
        [("test-agent".to_owned(), String::new(), "verbose".to_owned())]
    );
    assert!(
        h.root(|root, _| root.settings_open()),
        "OK waits for the daemon's echoes"
    );
    h.send(DaemonMessage::AgentSettings {
        settings: [(test_agent_id(), options(&[("profile", text("work"))]))]
            .into_iter()
            .collect(),
    });
    h.send(DaemonMessage::AgentCommands {
        commands: vec![AgentCommandEntry {
            agent: test_agent_id(),
            program: String::new(),
            args: "verbose".to_owned(),
            program_env: None,
        }],
    });
    assert!(!h.root(|root, _| root.settings_open()), "OK closes");
}

#[gpui::test]
fn cancel_discards_pending_edits_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_agent_pages(cx, &dir);
    for close in ["settings-cancel", "settings-close"] {
        h.click_on("settings-test-profile");
        type_line(&mut h, "work");
        h.click_on("settings-agent-args");
        type_line(&mut h, "verbose");
        h.click_on(close);
        assert!(!h.root(|root, _| root.settings_open()), "{close} closes");
        no_setting_sent(&mut h, &format!("{close} sends nothing"));

        h.keys("ctrl-,");
        h.click_on("settings-tab-test");
        assert_eq!(
            launch_fields(&mut h),
            (Some(String::new()), Some(String::new())),
            "{close} put the launch rows back to the saved command"
        );
        h.click_on("settings-apply");
        no_setting_sent(&mut h, &format!("{close} left nothing pending"));
    }
}

#[gpui::test]
fn escape_closes_and_discards_pending_edits(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_agent_pages(cx, &dir);
    h.click_on("settings-agent-args");
    type_line(&mut h, "verbose");
    h.click_on("settings-test-profile");
    type_line(&mut h, "work");
    h.keys("escape");
    assert!(
        !h.root(|root, _| root.settings_open()),
        "Esc in a text row closes"
    );
    no_setting_sent(&mut h, "Esc sends nothing");

    h.keys("ctrl-,");
    h.click_on("settings-tab-test");
    h.click_on("settings-agent-args");
    type_line(&mut h, "verbose");
    h.click_on("settings-tab-general");
    h.keys("escape");
    assert!(
        !h.root(|root, _| root.settings_open()),
        "Esc off a text row closes too"
    );
    no_setting_sent(&mut h, "nothing is sent");
    h.keys("ctrl-,");
    h.click_on("settings-tab-test");
    assert_eq!(
        launch_fields(&mut h),
        (Some(String::new()), Some(String::new())),
        "the drafts went with the Esc"
    );
}

#[gpui::test]
fn tab_ring_ends_on_ok_apply_cancel(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_agent_pages(cx, &dir);
    tab_to(&mut h, "settings-agent-args");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-cancel"),
        "with nothing pending the ring ends on Cancel"
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-search"),
        "the search box comes before the tab list"
    );
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("list"));

    h.keys("shift-tab shift-tab shift-tab");
    assert_eq!(settings_focus(&mut h), Some("settings-agent-args"));
    type_line(&mut h, "verbose");
    for stop in [
        "settings-ok",
        "settings-apply",
        "settings-cancel",
        "settings-search",
        "list",
    ] {
        h.keys("tab");
        assert_eq!(settings_focus(&mut h), Some(stop));
    }
    tab_to(&mut h, "settings-apply");
    h.keys("enter");
    assert_eq!(
        command_sets(&h.sent()),
        [("test-agent".to_owned(), String::new(), "verbose".to_owned())],
        "Enter on Apply applies"
    );
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-cancel"),
        "Apply, disabled once nothing is pending, hands the keyboard on to Cancel"
    );
}

/// A daemon refusal of the test agent's profile.
const PROFILE_REFUSAL: &str = "Test agent refuses the profile `work`.";

#[gpui::test]
fn a_refused_setting_does_not_consume_the_launch_command_apply(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-program");
    type_line(&mut h, "wrapper");
    h.click_on("settings-apply");
    assert_eq!(
        launch_sets(&h.sent()),
        [("wrapper".to_owned(), String::new())]
    );
    h.click_on("settings-claude-permission-mode-plan");
    let settings_id = last_settings_id(&h.sent());
    refuse(&mut h, settings_id.as_deref(), PROFILE_REFUSAL);
    assert_eq!(
        toast_titles(&mut h).len(),
        1,
        "the setting's refusal toasts"
    );
    assert_eq!(refusal(&mut h), None, "it is no launch-command refusal");
    h.send(agent_commands("wrapper", "", None));
    assert!(
        h.root(|root, _| root.launch_saved()),
        "the launch command's echo still answers its own apply"
    );
}

#[gpui::test]
fn a_refused_setting_shows_unsaved_and_is_not_resent(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    list_agents(&mut h, test_agent());
    h.keys("ctrl-,");
    h.click_on("settings-tab-test");
    h.click_on("settings-test-profile");
    type_line(&mut h, "work");
    h.sent();
    h.click_on("settings-apply");
    let first = last_settings_id(&h.sent());
    refuse(&mut h, first.as_deref(), PROFILE_REFUSAL);
    assert_eq!(
        toast_titles(&mut h).len(),
        1,
        "the refusal shows where Settings errors show"
    );

    h.click_on("settings-apply");
    let sent = h.sent();
    assert_eq!(
        settings_sets(&sent),
        vec![(test_agent_id(), options(&[("profile", text("work"))]))],
        "the field still holds the refused text, unsaved, so Apply sends it again"
    );
    refuse(&mut h, last_settings_id(&sent).as_deref(), PROFILE_REFUSAL);

    h.click_on("settings-test-profile");
    h.keys("ctrl-a backspace");
    h.click_on("settings-test-sandbox");
    h.click_on("settings-apply");
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(
            test_agent_id(),
            options(&[("sandbox", OptionValue::Bool(true))])
        )],
        "once the refused text is cleared, a later press does not carry the refused value"
    );
}

#[gpui::test]
fn a_refusal_after_an_unrelated_broadcast_still_reaches_its_field(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    list_agents(&mut h, test_agent());
    h.keys("ctrl-,");
    h.click_on("settings-tab-test");
    h.click_on("settings-test-profile");
    type_line(&mut h, "work");
    h.sent();
    h.click_on("settings-apply");
    let refused = last_settings_id(&h.sent());
    let slow = options(&[("mode", text("slow"))]);
    h.send(DaemonMessage::AgentSettings {
        settings: std::collections::BTreeMap::from([(test_agent_id(), slow)]),
    });
    refuse(&mut h, refused.as_deref(), PROFILE_REFUSAL);

    h.click_on("settings-apply");
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(
            test_agent_id(),
            options(&[("mode", text("slow")), ("profile", text("work"))])
        )],
        "the refusal still finds its field, which holds the user's text, unsaved"
    );
}

#[gpui::test]
fn a_refusal_keeps_an_earlier_applys_pending_values(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    list_agents(&mut h, test_agent());
    h.keys("ctrl-,");
    h.click_on("settings-tab-test");
    h.sent();
    h.click_on("settings-test-mode-slow");
    h.click_on("settings-test-sandbox");
    h.click_on("settings-apply");
    let sent = h.sent();
    assert_eq!(
        settings_sets(&sent),
        vec![
            (test_agent_id(), options(&[("mode", text("slow"))])),
            (
                test_agent_id(),
                options(&[("mode", text("slow")), ("sandbox", OptionValue::Bool(true))])
            ),
        ]
    );
    refuse(&mut h, last_settings_id(&sent).as_deref(), PROFILE_REFUSAL);

    h.click_on("settings-test-profile");
    type_line(&mut h, "work");
    h.click_on("settings-apply");
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(
            test_agent_id(),
            options(&[("mode", text("slow")), ("profile", text("work"))])
        )],
        "the earlier apply's mode, still waiting, rides along; the refused sandbox does not"
    );
}

#[gpui::test]
fn refusals_route_to_the_right_agents_page(cx: &mut TestAppContext) {
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
    h.click_on("settings-tab-test");
    h.click_on("settings-agent-args");
    type_line(&mut h, "verbose");
    h.click_on("settings-apply");
    h.click_on("settings-tab-claude");
    h.click_on("settings-agent-program");
    type_line(&mut h, "wrapper");
    h.sent();
    h.click_on("settings-apply");
    let claude_apply = last_command_id(&h.sent());

    refuse(&mut h, claude_apply.as_deref(), PROGRAM_REFUSAL);
    assert_eq!(
        refusal(&mut h),
        Some((LaunchCommandRow::Program, PROGRAM_REFUSAL.to_owned())),
        "Claude's page shows the refusal of its own apply"
    );
    h.send(DaemonMessage::AgentCommands {
        commands: vec![
            command_entry("claude", "", ""),
            command_entry("test-agent", "", "verbose"),
        ],
    });
    h.click_on("settings-tab-test");
    assert_eq!(refusal(&mut h), None, "the test agent's page shows none");
    assert!(
        h.root(|root, _| root.launch_saved()),
        "the echo answers the test agent's apply, which the refusal left waiting"
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
    assert_eq!(sent_agent_settings(&mut h), Vec::new(), "Tab sends nothing");
    h.click_on("settings-apply");
    assert_eq!(
        sent_agent_settings(&mut h),
        vec![(
            test_agent_id(),
            options(&[("sandbox", OptionValue::Bool(true)), ("second", text("x"))])
        )],
        "Apply sends the field's own value"
    );
}

#[gpui::test]
fn typing_after_apply_survives_the_echo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = claude_page(cx, &dir, agent_commands("", "", None));
    h.click_on("settings-agent-program");
    type_line(&mut h, "wrapper");
    h.click_on("settings-apply");
    h.click_on("settings-agent-program");
    h.keys("end");
    type_line(&mut h, "x");
    h.send(agent_commands("wrapper", "", None));
    assert_eq!(
        launch_fields(&mut h).0.as_deref(),
        Some("wrapperx"),
        "the echo keeps what was typed after Apply"
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
        Some("settings-claude-effort-cli-default"),
        "the effort row follows the provider"
    );
    tab_to(&mut h, "settings-claude-effort-max");
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-agent-args"),
        "Tab skips the read-only Program row"
    );
    type_line(&mut h, "verbose");
    h.click_on("settings-apply");
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
fn tab_walks_the_appearance_stops_and_shift_tab_walks_back(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    let ring = [
        "appearance-theme",
        "appearance-frame-width-down",
        "appearance-frame-width-up",
        "appearance-ui-family-filter",
        "appearance-ui-family-list",
        "appearance-ui-text-size-down",
        "appearance-ui-text-size-up",
        "appearance-ui-weight-regular",
        "appearance-ui-weight-medium",
        "appearance-ui-weight-semibold",
        "appearance-family-filter",
        "appearance-family-list",
        "appearance-size-down",
        "appearance-size-up",
        "appearance-term-weight-regular",
        "appearance-term-weight-medium",
        "appearance-term-weight-semibold",
        "appearance-term-weight-bold",
    ];
    for stop in ring {
        h.keys("tab");
        assert_eq!(settings_focus(&mut h), Some(stop));
    }
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-cancel"),
        "the footer's Cancel ends the ring"
    );
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-search"));
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("list"), "back to the tab list");
    h.keys("shift-tab");
    assert_eq!(settings_focus(&mut h), Some("settings-search"));
    h.keys("shift-tab");
    assert_eq!(settings_focus(&mut h), Some("settings-cancel"));
    for stop in ring.iter().rev() {
        h.keys("shift-tab");
        assert_eq!(settings_focus(&mut h), Some(*stop), "Shift+Tab walks back");
    }
    h.keys("shift-tab");
    assert_eq!(settings_focus(&mut h), Some("list"));
}

#[gpui::test]
fn settings_appearance_draws_no_accent_or_background_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let h = appearance_tab(cx, &dir);
    for row in [
        "appearance-accent-presets",
        "appearance-accent-hex",
        "appearance-background-presets",
        "appearance-background-hex",
    ] {
        assert!(
            h.cx.debug_bounds(row).is_none(),
            "{row}: a custom colour lives in a theme"
        );
    }
    assert!(h.cx.debug_bounds("appearance-theme").is_some());
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
    let close = h.bounds("settings-cancel");
    assert!(
        close.bottom() <= px(height),
        "Cancel ends at {:?}, below the {height}px window",
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
    assert_eq!(ui_family(&mut h), "Inter", "nothing picked: the default");

    h.click_on("appearance-ui-family-option-Geist");
    assert_eq!(
        saved_ui(&dir)["ui_font_family"],
        "Geist",
        "saved on this machine"
    );
    assert_eq!(ui_family(&mut h), "Geist", "the pick draws at once");

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
        "Inter",
        "Use built-in restores the default"
    );
    assert_eq!(saved_ui(&dir)["ui_font_family"], serde_json::Value::Null);
}

/// The offset every UI text size is drawn at.
fn ui_text_size(h: &mut Harness<'_>) -> i8 {
    h.cx.update(|_, cx| fonts::ui_text_size(cx))
}

/// What the UI text size row shows between its buttons.
fn ui_text_size_label(h: &mut Harness<'_>) -> String {
    h.root(|root, _| root.appearance_ui_text_size_label())
        .expect("Settings › Appearance is open")
}

/// Clicks the UI text size row's `up` or `down` button, scrolled into view.
fn press_ui_text_size(h: &mut Harness<'_>, which: &str) {
    let selector = format!("appearance-ui-text-size-{which}");
    h.scroll_into_view("settings-content", &selector);
    h.click_on(&selector);
}

/// The Appearance stops Tab walks, from the tab list round to it.
fn appearance_ring(h: &mut Harness<'_>) -> Vec<&'static str> {
    h.click_on("settings-tab-appearance");
    let mut ring = Vec::new();
    for _ in 0..40 {
        h.keys("tab");
        match settings_focus(h) {
            Some("list") | None => return ring,
            Some(stop) => ring.push(stop),
        }
    }
    assert_eq!(
        settings_focus(h),
        Some("list"),
        "Tab never came back to the tab list: {ring:?}"
    );
    ring
}

/// The weight inherited body text draws at.
fn body_weight(h: &mut Harness<'_>) -> FontWeight {
    h.cx.update(|_, cx| fonts::ui_weight(cx, FontWeight::NORMAL))
}

/// Clicks the UI weight row's `regular`, `medium` or `semibold` button,
/// scrolled into view.
fn press_ui_weight(h: &mut Harness<'_>, which: &str) {
    let selector = format!("appearance-ui-weight-{which}");
    h.scroll_into_view("settings-content", &selector);
    h.click_on(&selector);
}

#[gpui::test]
fn ui_weight_buttons_set_and_save_the_weight(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    assert_eq!(body_weight(&mut h), FontWeight::NORMAL, "nothing picked");

    press_ui_weight(&mut h, "semibold");
    assert_eq!(
        body_weight(&mut h),
        FontWeight::SEMIBOLD,
        "the press applies at once"
    );
    assert_eq!(
        saved_ui(&dir)["ui_font_weight"],
        600,
        "saved on this machine"
    );

    press_ui_weight(&mut h, "medium");
    assert_eq!(body_weight(&mut h), FontWeight::MEDIUM);
    assert_eq!(saved_ui(&dir)["ui_font_weight"], 500);
    assert_eq!(
        h.cx.update(|_, cx| fonts::ui_text_size(cx)),
        0,
        "the weight leaves the text size alone"
    );

    press_ui_weight(&mut h, "regular");
    assert_eq!(body_weight(&mut h), FontWeight::NORMAL);
    assert_eq!(saved_ui(&dir)["ui_font_weight"], 400);
}

#[gpui::test]
fn the_ui_weight_buttons_follow_the_ui_text_size_in_the_ring(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    let ring = appearance_ring(&mut h);
    let up = ring
        .iter()
        .position(|stop| *stop == "appearance-ui-text-size-up")
        .expect("the UI text size + is in the ring");
    assert_eq!(
        ring[up + 1..up + 5],
        [
            "appearance-ui-weight-regular",
            "appearance-ui-weight-medium",
            "appearance-ui-weight-semibold",
            "appearance-family-filter",
        ],
        "after the UI text size, before the terminal font"
    );
}

#[gpui::test]
fn ui_text_size_plus_raises_every_ui_text_and_saves(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    assert_eq!(ui_text_size(&mut h), 0, "nothing picked: no offset");
    assert_eq!(ui_text_size_label(&mut h), "Default");

    press_ui_text_size(&mut h, "up");
    assert_eq!(ui_text_size(&mut h), 1, "the press applies at once");
    assert_eq!(
        h.cx.update(|_, cx| fonts::ui_px(cx, 12.0)),
        px(13.0),
        "every UI text size reads one pixel larger"
    );
    assert_eq!(ui_text_size_label(&mut h), "+1 px");
    assert_eq!(saved_ui(&dir)["ui_text_size"], 1, "saved on this machine");

    press_ui_text_size(&mut h, "down");
    press_ui_text_size(&mut h, "down");
    assert_eq!(ui_text_size(&mut h), -1);
    assert_eq!(ui_text_size_label(&mut h), "−1 px");
    assert_eq!(saved_ui(&dir)["ui_text_size"], -1);
}

#[gpui::test]
fn a_dialog_button_grows_with_the_ui_text_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    let base = h.bounds("settings-cancel").size.height;

    h.root_update(|root, _, cx| root.set_ui_text_size(3, cx));
    assert_eq!(
        h.bounds("settings-cancel").size.height,
        base + px(3.0),
        "the Cancel button is three pixels taller at +3"
    );

    h.root_update(|root, _, cx| root.set_ui_text_size(-2, cx));
    assert_eq!(h.bounds("settings-cancel").size.height, base - px(2.0));
}

#[gpui::test]
fn the_settings_tabs_grow_with_the_ui_text_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    let base = h.bounds("settings-tab-general").size.height;

    h.root_update(|root, _, cx| root.set_ui_text_size(3, cx));
    let first = h.bounds("settings-tab-general");
    assert_eq!(
        first.size.height,
        base + px(3.0),
        "three pixels taller at +3"
    );
    assert!(
        first.bottom() <= h.bounds("settings-tab-notifications").top(),
        "the tabs do not overlap"
    );
}

#[gpui::test]
fn ui_text_size_buttons_stop_at_the_ends_of_the_range(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    for _ in 0..6 {
        press_ui_text_size(&mut h, "up");
    }
    assert_eq!(ui_text_size(&mut h), 4, "+ stops at +4");
    assert_eq!(ui_text_size_label(&mut h), "+4 px");
    let ring = appearance_ring(&mut h);
    assert!(
        ring.contains(&"appearance-ui-text-size-down")
            && !ring.contains(&"appearance-ui-text-size-up"),
        "+ is disabled at the top of the range: {ring:?}"
    );

    for _ in 0..8 {
        press_ui_text_size(&mut h, "down");
    }
    assert_eq!(ui_text_size(&mut h), -2, "− stops at −2");
    assert_eq!(ui_text_size_label(&mut h), "−2 px");
    assert_eq!(saved_ui(&dir)["ui_text_size"], -2);
    let ring = appearance_ring(&mut h);
    assert!(
        ring.contains(&"appearance-ui-text-size-up")
            && !ring.contains(&"appearance-ui-text-size-down"),
        "− is disabled at the bottom of the range: {ring:?}"
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
fn appearance_term_weight_picks_with_space(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    tab_to(&mut h, "appearance-term-weight-semibold");
    h.keys("space");
    assert_eq!(
        appearance_hint(&mut h, "weight"),
        "Resolved: SemiBold from app"
    );
    assert_eq!(saved_ui(&dir)["terminal_font"]["weight"], 600);
    assert_eq!(
        settings_focus(&mut h),
        Some("appearance-term-weight-semibold"),
        "the pick keeps the keyboard on the button"
    );
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("appearance-term-weight-bold"));
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("appearance-term-weight-reset"),
        "the reset link is a stop while the level sets the weight"
    );
    h.keys("enter");
    assert_eq!(
        appearance_hint(&mut h, "weight"),
        "Resolved: Regular from built-in"
    );
}

#[gpui::test]
fn appearance_hex_and_filter_fields_draw_the_focus_ring(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    for field in ["appearance-ui-family-filter", "appearance-family-filter"] {
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
        Some("appearance-term-weight-regular"),
        "the reset link is off now, so the keyboard moved on to the weight row"
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("appearance-term-weight-medium"),
        "the ring walks on along the weight row"
    );
}

#[gpui::test]
fn a_click_on_an_appearance_control_moves_the_keyboard_there(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    tab_to(&mut h, "appearance-term-weight-bold");
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
fn window_title_without_suffix(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-app-title");
    h.click_on("settings-count-busy");
    h.click_on("settings-title-product-suffix");
    h.click_on("settings-apply");
    h.advance(Duration::from_millis(360));
    assert_eq!(title(&mut h), "work");
    h.click_on("settings-title-count");
    h.click_on("settings-apply");
    h.advance(Duration::from_millis(360));
    assert_eq!(title(&mut h), "work");
    h.click_on("settings-title-product-suffix");
    h.click_on("settings-apply");
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
    h.click_on("settings-apply");
    assert_eq!(worktrees_root_sets(&h.sent()), [Some("X:/nope".to_owned())]);
    refuse(&mut h, None, WORKTREES_ROOT_REFUSAL);
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
    assert!(
        h.root(|root, _| root.settings_apply_enabled()),
        "the path the daemon refused is pending again"
    );
    assert_eq!(
        h.root(|root, _| root.worktrees_active_line()),
        "Active: D:/other — user override",
        "another client's root still lands"
    );
}

#[gpui::test]
fn holding_enter_on_choose_opens_nothing_twice(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = appearance_tab(cx, &dir);
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("appearance-theme"));

    h.held("enter");
    assert!(
        !h.root(|root, _| root.themes_modal_open()),
        "a held key opens nothing"
    );

    h.keys("enter");
    assert!(h.root(|root, _| root.themes_modal_open()));
    h.keys("escape");
    h.held("enter");
    assert!(!h.root(|root, _| root.themes_modal_open()));
}

/// The privacy lines of the two providers the Dashboard specs offer.
const CLI_PRIVACY: &str = "Runs through your agent's CLI, on your account.";
const KEYED_PRIVACY: &str = "Sends session text to the provider's API.";

/// A status offering a CLI provider (`cli`, no key) and a keyed one
/// (`keyed`), with `chosen` chosen.
fn dashboard_status(chosen: &str) -> support::SummarizerStatusBuilder {
    support::summarizer_status()
        .offers("cli", "CLI model", false, CLI_PRIVACY)
        .offers("keyed", "Keyed model", true, KEYED_PRIVACY)
        .chosen(chosen)
}

/// Settings open on its Dashboard tab after the daemon sent `status`,
/// nothing sent yet.
fn dashboard_tab<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    status: support::SummarizerStatusBuilder,
) -> Harness<'a> {
    dashboard_tab_after(cx, dir, status.build())
}

/// Settings open on its Dashboard tab after the daemon sent `status`, a
/// `SummarizerStatus`, nothing sent yet.
fn dashboard_tab_after<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    status: DaemonMessage,
) -> Harness<'a> {
    let mut h = opened(cx, dir);
    h.send(status);
    h.keys("ctrl-,");
    h.click_on("settings-tab-dashboard");
    h.sent();
    h
}

/// The summarizer settings the client sent, oldest first.
fn summarizer_sets(sent: &[ClientMessage]) -> Vec<protocol::SummarizerSettings> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetSummarizerSettings { settings, .. } => Some(settings.clone()),
            _ => None,
        })
        .collect()
}

/// The `(provider, key)` of every `SetSummarizerKey` the client sent.
fn key_sets(sent: &[ClientMessage]) -> Vec<(String, Option<String>)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetSummarizerKey { provider, key, .. } => {
                Some((provider.clone(), key.as_ref().map(|key| key.0.clone())))
            }
            _ => None,
        })
        .collect()
}

/// The request id of the last `SetSummarizerSettings` among `sent`.
fn last_summarizer_id(sent: &[ClientMessage]) -> Option<String> {
    sent.iter().rev().find_map(|msg| match msg {
        ClientMessage::SetSummarizerSettings { request_id, .. } => request_id.clone(),
        _ => None,
    })
}

/// The request ids of the `TestSummary` messages among `sent`.
fn test_ids(sent: &[ClientMessage]) -> Vec<Option<String>> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::TestSummary { request_id } => Some(request_id.clone()),
            _ => None,
        })
        .collect()
}

fn key_state_line(h: &mut Harness<'_>) -> Option<&'static str> {
    h.root(|root, _| root.dashboard_key_state())
}

fn spend_line(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.dashboard_spend_line())
}

fn test_result(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.dashboard_test_result())
}

#[gpui::test]
fn dashboard_tab_follows_the_notifications_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    let rows = h.root(|root, _| root.settings_tab_rows());
    let at = rows
        .iter()
        .position(|row| *row == "Notifications")
        .expect("Notifications is listed");
    assert_eq!(rows.get(at + 1).copied(), Some("Dashboard"));
    h.keys("down down");
    assert_eq!(shown_tab(&mut h), "Dashboard", "Down walks to it");
    assert!(
        !drawn(&mut h, "settings-dashboard-enabled"),
        "no controls before the daemon reports its summarizer"
    );
    h.send(dashboard_status("cli").build());
    assert!(drawn(&mut h, "settings-dashboard-enabled"));
    assert!(drawn(&mut h, "settings-dashboard-test"));
}

#[gpui::test]
fn dashboard_tab_lists_the_providers_the_daemon_offers(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("cli"));
    assert!(drawn(&mut h, "settings-dashboard-provider-cli"));
    assert!(drawn(&mut h, "settings-dashboard-provider-keyed"));
    assert!(!drawn(&mut h, "settings-dashboard-provider-p1"));
    assert!(
        !drawn(&mut h, "settings-dashboard-key-source-stored"),
        "a provider that takes no key shows no key rows"
    );
}

#[gpui::test]
fn summaries_toggle_sends_set_summarizer_settings(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("cli"));
    h.click_on("settings-dashboard-enabled");
    h.click_on("settings-apply");
    let sets = summarizer_sets(&h.sent());
    assert_eq!(sets.len(), 1, "{sets:?}");
    assert!(!sets[0].enabled, "the toggle turns summaries off");
    assert_eq!(sets[0].provider, "cli", "the rest is as the daemon has it");
}

/// Whether Settings holds a pending edit, and whether OK and Apply can be
/// pressed.
fn apply_gate(h: &mut Harness<'_>) -> (bool, bool) {
    h.root(|root, _| {
        (
            root.settings_have_pending_edits(),
            root.settings_apply_enabled(),
        )
    })
}

#[gpui::test]
fn a_pending_toggle_opens_the_apply_gate(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-notifications");
    assert_eq!(apply_gate(&mut h), (false, false), "nothing is pending");
    h.click_on("settings-notify-finished");
    assert_eq!(apply_gate(&mut h), (true, true), "the toggle pends");
    h.click_on("settings-apply");
    assert_eq!(saved_ui(&dir)["notifications"]["finished"], false);
    assert_eq!(apply_gate(&mut h), (false, false), "Apply took it");
}

#[gpui::test]
fn a_pending_worktrees_path_opens_the_apply_gate(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(DaemonMessage::WorktreesRootChanged {
        root: "C:/default".to_owned(),
        is_override: false,
    });
    h.keys("ctrl-,");
    h.click_on("settings-tab-worktrees");
    assert_eq!(apply_gate(&mut h), (false, false), "nothing is pending");
    h.click_on("settings-worktrees-root-input");
    type_text(&mut h, "D:/wt");
    assert_eq!(apply_gate(&mut h), (true, true), "the path pends");
    h.sent();
    h.click_on("settings-apply");
    assert_eq!(worktrees_root_sets(&h.sent()), [Some("D:/wt".to_owned())]);
}

#[gpui::test]
fn a_pending_dashboard_row_opens_the_apply_gate(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("cli"));
    assert_eq!(apply_gate(&mut h), (false, false), "nothing is pending");
    h.click_on("settings-dashboard-cap");
    h.keys("ctrl-a");
    type_text(&mut h, "2.5");
    assert_eq!(apply_gate(&mut h), (true, true), "the cap pends");
    h.click_on("settings-apply");
    let caps: Vec<f64> = summarizer_sets(&h.sent())
        .iter()
        .map(|settings| settings.daily_cap_usd)
        .collect();
    assert_eq!(caps, [2.5]);
}

#[gpui::test]
fn provider_choice_sends_set_summarizer_settings_and_shows_its_privacy_line(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("cli"));
    assert_eq!(
        h.root(|root, _| root.dashboard_privacy()).as_deref(),
        Some(CLI_PRIVACY)
    );
    h.click_on("settings-dashboard-provider-keyed");
    h.click_on("settings-apply");
    let sets = summarizer_sets(&h.sent());
    assert_eq!(
        sets.iter().map(|s| s.provider.as_str()).collect::<Vec<_>>(),
        ["keyed"]
    );
    h.send(dashboard_status("keyed").build());
    assert_eq!(
        h.root(|root, _| root.dashboard_privacy()).as_deref(),
        Some(KEYED_PRIVACY),
        "the privacy line follows the chosen provider"
    );
    assert!(drawn(&mut h, "settings-dashboard-privacy"));
}

#[gpui::test]
fn deepseek_save_sends_set_summarizer_key_and_clears_the_field(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let status = dashboard_status("keyed").key(protocol::KeyState::Missing);
    let mut h = dashboard_tab(cx, &dir, status);
    h.click_on("settings-dashboard-key-field");
    type_text(&mut h, "sk-123");
    h.click_on("settings-apply");
    assert_eq!(
        key_sets(&h.sent()),
        [("keyed".to_owned(), Some("sk-123".to_owned()))]
    );
    assert_eq!(
        h.root(rustling_tulip_native::RootView::dashboard_key_draft)
            .as_deref(),
        Some(""),
        "Save clears the field"
    );
}

#[gpui::test]
fn masked_key_field_never_draws_the_typed_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let status = dashboard_status("keyed").key(protocol::KeyState::Missing);
    let mut h = dashboard_tab(cx, &dir, status);
    h.click_on("settings-dashboard-key-field");
    type_text(&mut h, "secret");
    assert_eq!(
        h.root(rustling_tulip_native::RootView::dashboard_key_masked),
        Some(true),
        "the key field draws dots"
    );
}

#[gpui::test]
fn clear_sends_set_summarizer_key_none(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("keyed"));
    h.click_on("settings-dashboard-key-clear");
    assert_eq!(key_sets(&h.sent()), [("keyed".to_owned(), None)]);
}

#[gpui::test]
fn key_state_line_follows_summarizer_status(cx: &mut TestAppContext) {
    use protocol::KeyState;
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("keyed"));
    assert!(drawn(&mut h, "settings-dashboard-key-state"));
    for (state, words) in [
        (KeyState::Missing, "No key"),
        (KeyState::Stored, "Key stored"),
        (KeyState::EnvFound, "Variable found"),
        (KeyState::EnvMissing, "Variable not set"),
        (KeyState::Rejected, "Key rejected"),
        (
            KeyState::Unreachable,
            "Credential Manager unavailable on this host",
        ),
    ] {
        h.send(dashboard_status("keyed").key(state).build());
        assert_eq!(key_state_line(&mut h), Some(words), "{state:?}");
    }
}

#[gpui::test]
fn env_source_sends_the_variable_name(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("keyed"));
    assert!(drawn(&mut h, "settings-dashboard-key-field"));
    h.click_on("settings-dashboard-key-source-env");
    assert_eq!(
        summarizer_sets(&h.sent()),
        [],
        "no variable is named yet, so nothing is sent"
    );
    assert!(
        drawn(&mut h, "settings-dashboard-env-variable"),
        "the variable field takes the key field's place"
    );
    h.click_on("settings-dashboard-env-variable");
    type_text(&mut h, "MY_KEY");
    h.keys("enter");
    h.click_on("settings-apply");
    let sets = summarizer_sets(&h.sent());
    assert_eq!(
        sets.iter()
            .map(|s| s.key_source.clone())
            .collect::<Vec<_>>(),
        [protocol::SummarizerKeySource::Env {
            variable: "MY_KEY".to_owned()
        }]
    );
    h.send(dashboard_status("keyed").env("MY_KEY").build());
    assert!(
        drawn(&mut h, "settings-dashboard-env-variable"),
        "the daemon reads the variable"
    );
    h.click_on("settings-dashboard-key-source-stored");
    h.click_on("settings-apply");
    assert_eq!(
        summarizer_sets(&h.sent())
            .iter()
            .map(|s| s.key_source.clone())
            .collect::<Vec<_>>(),
        [protocol::SummarizerKeySource::Stored],
        "Stored key goes back to the stored key"
    );
}

#[gpui::test]
fn cap_field_sends_a_valid_cap_and_refuses_a_bad_one(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("cli"));
    h.click_on("settings-dashboard-cap");
    h.keys("ctrl-a");
    type_text(&mut h, "two");
    h.keys("enter");
    assert_eq!(summarizer_sets(&h.sent()), [], "a bad cap is not sent");
    assert!(
        h.root(|root, _| root.dashboard_cap_error()).is_some(),
        "and says why"
    );

    h.keys("ctrl-a");
    type_text(&mut h, "2.5");
    h.keys("enter");
    h.click_on("settings-apply");
    let sent = h.sent();
    let caps: Vec<f64> = summarizer_sets(&sent)
        .iter()
        .map(|s| s.daily_cap_usd)
        .collect();
    assert_eq!(caps, [2.5]);
    assert_eq!(h.root(|root, _| root.dashboard_cap_error()), None);

    let id = last_summarizer_id(&sent).expect("the cap change carries a request id");
    h.send(DaemonMessage::Error {
        message: "The daily cap must be a number of dollars, zero or more.".to_owned(),
        request_id: Some(id),
        code: Some(error_codes::SUMMARIZER_REFUSED.to_owned()),
    });
    assert_eq!(
        h.root(|root, _| root.dashboard_cap_error()).as_deref(),
        Some("The daily cap must be a number of dollars, zero or more."),
        "the daemon's refusal shows under the field"
    );
    assert!(toast_titles(&mut h).is_empty(), "and raises no toast");
}

#[gpui::test]
fn spend_today_and_cap_reached_follow_the_status(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("cli").spent(0.42));
    assert!(drawn(&mut h, "settings-dashboard-spend"));
    assert_eq!(
        spend_line(&mut h).as_deref(),
        Some("Spent today: $0.42 of $1.00")
    );
    h.send(dashboard_status("cli").spent(1.0).cap_reached(true).build());
    assert_eq!(
        spend_line(&mut h).as_deref(),
        Some("Spent today: $1.00 of $1.00 — cap reached, summaries paused until tomorrow")
    );
}

#[gpui::test]
fn test_button_sends_test_summary_and_shows_the_reply_source_latency_cost(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("cli"));
    assert_eq!(test_result(&mut h), None, "nothing before a Test");
    h.click_on("settings-dashboard-test");
    let ids = test_ids(&h.sent());
    assert_eq!(ids.len(), 1, "{ids:?}");
    let id = ids[0].clone().expect("the Test carries a request id");
    assert_eq!(test_result(&mut h).as_deref(), Some("Testing…"));
    h.send(DaemonMessage::SummaryTest {
        result: protocol::SummaryTestResult {
            kind: None,
            headline: "Tests pass".to_owned(),
            notification: "All 12 tests pass.".to_owned(),
            source: protocol::SummarySource::Model,
        },
        latency_ms: 840,
        cost_usd: 0.0012,
        request_id: Some(id),
    });
    assert_eq!(
        test_result(&mut h).as_deref(),
        Some("Tests pass — All 12 tests pass.\nModel · 840 ms · $0.0012")
    );
    assert!(drawn(&mut h, "settings-dashboard-test-result"));
}

#[gpui::test]
fn test_shows_an_error_reply_at_the_cap(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("cli").cap_reached(true));
    h.click_on("settings-dashboard-test");
    let id = test_ids(&h.sent())
        .pop()
        .flatten()
        .expect("the Test carries a request id");
    h.send(DaemonMessage::Error {
        message: "Today's summary cap is reached.".to_owned(),
        request_id: Some(id),
        code: Some(error_codes::SUMMARIZER_REFUSED.to_owned()),
    });
    assert_eq!(
        test_result(&mut h).as_deref(),
        Some("Today's summary cap is reached.")
    );
    assert!(
        toast_titles(&mut h).is_empty(),
        "the slot says it, not a toast"
    );
}

#[gpui::test]
fn status_is_kept_per_host_and_forgotten_on_welcome(cx: &mut TestAppContext) {
    use rustling_tulip_native::target::{HostKey, Target};
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("cli").spent(0.1));
    let remote = Target::tailnet("box.tail1234.ts.net", None);
    let row = format!(
        "settings-host-{}",
        HostKey::of(&remote)
            .wire()
            .expect("a remote host has a wire key")
    );
    {
        let mut host = h.host(&remote);
        host.connect();
        host.send(dashboard_status("cli").spent(0.2).build());
    }
    assert_eq!(
        spend_line(&mut h).as_deref(),
        Some("Spent today: $0.10 of $1.00")
    );
    h.click_on(&row);
    assert_eq!(
        spend_line(&mut h).as_deref(),
        Some("Spent today: $0.20 of $1.00"),
        "the selector's host shows its own"
    );
    h.click_on("settings-dashboard-test");
    assert_eq!(
        test_ids(&h.host(&remote).sent()).len(),
        1,
        "the Test goes to that host"
    );
    assert!(test_ids(&h.sent()).is_empty(), "and not to This PC");

    h.host(&remote).send(support::welcome(support::PROTOCOL));
    assert_eq!(
        spend_line(&mut h),
        None,
        "a Welcome forgets the host's status"
    );
    assert_eq!(test_result(&mut h), None, "and its Test");
    h.click_on("settings-host-local");
    assert_eq!(
        spend_line(&mut h).as_deref(),
        Some("Spent today: $0.10 of $1.00"),
        "This PC's stays"
    );
}

/// `status`, a `SummarizerStatus`, with its settings edited by `edit`.
fn with_settings(
    mut status: DaemonMessage,
    edit: impl FnOnce(&mut protocol::SummarizerSettings),
) -> DaemonMessage {
    if let DaemonMessage::SummarizerStatus { settings, .. } = &mut status {
        edit(settings);
    }
    status
}

fn key_draft(h: &mut Harness<'_>) -> Option<String> {
    h.root(rustling_tulip_native::RootView::dashboard_key_draft)
}

fn cap_draft(h: &mut Harness<'_>) -> Option<String> {
    h.root(rustling_tulip_native::RootView::dashboard_cap_draft)
}

#[gpui::test]
fn an_unsaved_key_is_dropped_when_settings_closes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let status = dashboard_status("keyed").key(protocol::KeyState::Missing);
    let mut h = dashboard_tab(cx, &dir, status);
    h.click_on("settings-dashboard-key-field");
    type_text(&mut h, "sk-unsaved");
    h.click_on("settings-tab-general");
    h.click_on("settings-tab-dashboard");
    assert_eq!(
        key_draft(&mut h).as_deref(),
        Some("sk-unsaved"),
        "a tab switch keeps the typed key, pending until Apply or OK"
    );

    h.click_on("settings-dashboard-key-field");
    h.keys("escape");
    assert!(
        !h.root(|root, _| root.settings_open()),
        "Escape closes Settings"
    );
    assert_eq!(
        key_draft(&mut h),
        None,
        "closing Settings drops the typed key"
    );
    h.keys("ctrl-,");
    h.click_on("settings-tab-dashboard");
    assert_eq!(
        key_draft(&mut h).as_deref(),
        Some(""),
        "reopened, the field is empty"
    );
    assert_eq!(key_sets(&h.sent()), [], "the key was never sent");
}

#[gpui::test]
fn a_toggle_right_after_a_cap_change_keeps_the_new_cap(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("cli"));
    h.click_on("settings-dashboard-cap");
    h.keys("ctrl-a");
    type_text(&mut h, "2.50");
    h.keys("enter");
    h.click_on("settings-apply");
    h.click_on("settings-dashboard-enabled");
    h.click_on("settings-apply");
    let sets: Vec<(bool, f64)> = summarizer_sets(&h.sent())
        .iter()
        .map(|s| (s.enabled, s.daily_cap_usd))
        .collect();
    assert_eq!(
        sets,
        [(true, 2.5), (false, 2.5)],
        "the toggle builds on the cap just sent, not on the daemon's last status"
    );
}

#[gpui::test]
fn an_unknown_key_source_locks_the_dashboard_settings(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let status = with_settings(dashboard_status("keyed").build(), |settings| {
        settings.key_source = protocol::SummarizerKeySource::Unknown;
    });
    let mut h = dashboard_tab_after(cx, &dir, status);
    assert!(
        drawn(&mut h, "settings-dashboard-newer-daemon"),
        "the tab says the daemon is newer than this app"
    );
    h.click_on("settings-dashboard-enabled");
    h.click_on("settings-dashboard-provider-cli");
    h.click_on("settings-dashboard-key-source-env");
    assert_eq!(
        summarizer_sets(&h.sent()),
        [],
        "a key source this app cannot name is never sent back"
    );
}

#[gpui::test]
fn the_key_source_follows_the_daemon_after_env_applies(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("keyed"));
    h.click_on("settings-dashboard-key-source-env");
    h.click_on("settings-dashboard-env-variable");
    type_text(&mut h, "MY_KEY");
    h.keys("enter");
    h.send(dashboard_status("keyed").env("MY_KEY").build());
    let shown_source = |h: &mut Harness<'_>| h.root(|root, _| root.dashboard_key_source());
    assert_eq!(shown_source(&mut h), "Environment variable");

    h.send(dashboard_status("keyed").build());
    h.click_on("settings-apply");
    assert_eq!(
        shown_source(&mut h),
        "Stored key",
        "the stored key set elsewhere shows"
    );
}

#[gpui::test]
fn the_cap_field_follows_a_cap_set_elsewhere_after_a_local_edit(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("cli"));
    let capped = |cap: f64| {
        with_settings(dashboard_status("cli").build(), |settings| {
            settings.daily_cap_usd = cap;
        })
    };
    h.click_on("settings-dashboard-cap");
    h.keys("ctrl-a");
    type_text(&mut h, "2.5");
    h.keys("enter");
    h.send(capped(2.5));
    assert_eq!(
        cap_draft(&mut h).as_deref(),
        Some("2.5"),
        "the typed cap stays"
    );

    h.send(capped(3.0));
    assert_eq!(
        cap_draft(&mut h).as_deref(),
        Some("3.00"),
        "a cap set elsewhere replaces the one this field sent"
    );
}

const COPY: &str = "settings-terminal-copy-on-selection";

/// Settings open on General, This PC's keep-awake reported as Off.
fn general_with_keep_awake<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = opened(cx, dir);
    h.send(keep_awake_status(KeepAwakeMode::Off, false));
    h.keys("ctrl-,");
    h.sent();
    h
}

/// Copy on selection as `native-ui.json` holds it; `None` before any save.
fn copy_saved(dir: &TestDir) -> Option<bool> {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).ok()?;
    let saved: serde_json::Value = serde_json::from_str(&text).expect("native-ui.json is JSON");
    saved["general"]["copy_on_select"].as_bool()
}

fn keep_awake_sends(sent: &[ClientMessage]) -> Vec<KeepAwakeMode> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetKeepAwakeMode { mode } => Some(*mode),
            _ => None,
        })
        .collect()
}

fn applies(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.settings_apply_enabled())
}

fn press_copy(h: &mut Harness<'_>) {
    h.scroll_into_view("settings-content", COPY);
    h.click_on(COPY);
}

const DETAILED: &str = "settings-detailed-logging";
const OPEN_LOGS: &str = "settings-open-logs-folder";

/// Whether the Detailed logging switch draws ticked.
fn detailed_shown(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.detailed_logging_shown())
}

/// Detailed logging as `native-ui.json` holds it; `None` before any save.
fn detailed_saved(dir: &TestDir) -> Option<bool> {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).ok()?;
    let saved: serde_json::Value = serde_json::from_str(&text).expect("native-ui.json is JSON");
    saved["general"]["detailed_logging"].as_bool()
}

/// The `enabled` of every `SetDetailedLogging` among `sent`.
fn detailed_sends(sent: &[ClientMessage]) -> Vec<bool> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetDetailedLogging { enabled } => Some(*enabled),
            _ => None,
        })
        .collect()
}

fn press_detailed(h: &mut Harness<'_>) {
    h.scroll_into_view("settings-content", DETAILED);
    h.click_on(DETAILED);
}

#[gpui::test]
fn detailed_logging_row_pends_until_apply(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = general_with_keep_awake(cx, &dir);
    let before = detailed_saved(&dir);
    press_detailed(&mut h);
    assert_eq!(detailed_saved(&dir), before, "no native-ui.json write");
    assert!(
        detailed_sends(&h.sent()).is_empty(),
        "nothing is sent until Apply"
    );
    assert!(detailed_shown(&mut h), "the pending switch draws ticked");
    assert!(applies(&mut h), "Apply and OK open");
}

#[gpui::test]
fn apply_saves_detailed_logging_and_sends_set_detailed_logging_to_this_pc(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = general_with_keep_awake(cx, &dir);
    press_detailed(&mut h);
    h.click_on("settings-apply");
    assert_eq!(saved_ui(&dir)["general"]["detailed_logging"], true);
    assert_eq!(
        detailed_sends(&h.sent()),
        [true],
        "Apply sends the switch to This PC's daemon"
    );
}

#[gpui::test]
fn cancel_discards_a_pending_detailed_logging_switch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = general_with_keep_awake(cx, &dir);
    let before = detailed_saved(&dir);
    press_detailed(&mut h);
    h.click_on("settings-cancel");
    assert!(
        !h.root(|root, _| root.settings_open()),
        "Cancel closed Settings"
    );
    assert_eq!(detailed_saved(&dir), before, "nothing was saved");
    assert!(detailed_sends(&h.sent()).is_empty(), "nothing was sent");
    h.keys("ctrl-,");
    assert!(!detailed_shown(&mut h), "the switch draws the saved value");
}

#[gpui::test]
fn ok_waits_for_the_detailed_logging_echo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = general_with_keep_awake(cx, &dir);
    press_detailed(&mut h);
    h.click_on("settings-ok");
    assert!(
        h.root(|root, _| root.settings_ok_waiting()),
        "OK waits for the daemon's echo"
    );
    assert!(
        h.root(|root, _| root.settings_open()),
        "Settings stays open"
    );
    h.send(DaemonMessage::DetailedLogging { enabled: true });
    assert!(
        !h.root(|root, _| root.settings_open()),
        "the echo closed Settings"
    );
}

#[gpui::test]
fn detailed_logging_is_not_sent_to_a_daemon_without_the_feature(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = general_with_keep_awake(cx, &dir);
    h.welcome_without_features();
    press_detailed(&mut h);
    h.click_on("settings-ok");
    assert_eq!(saved_ui(&dir)["general"]["detailed_logging"], true);
    assert!(
        detailed_sends(&h.sent()).is_empty(),
        "a daemon without the feature hears nothing"
    );
    assert!(
        !h.root(|root, _| root.settings_open()),
        "OK closes without an echo to wait for"
    );
}

#[gpui::test]
fn the_open_logs_folder_row_opens_the_logs_folder_at_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with_config_dir(cx, &dir, &two_panes());
    h.keys("ctrl-,");
    h.sent();
    let folder = dir.path().join("logs");
    h.scroll_into_view("settings-content", OPEN_LOGS);
    h.click_on(OPEN_LOGS);
    assert_eq!(
        h.opened(),
        [Opened::DefaultApp(folder)],
        "the press opens the logs folder at once"
    );
    assert!(!applies(&mut h), "an immediate action holds nothing");
    assert!(
        h.root(|root, _| root.settings_open()),
        "Settings stays open"
    );
}

#[gpui::test]
fn the_detailed_logging_hint_names_the_5_mib_log_files(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = general_with_keep_awake(cx, &dir);
    h.scroll_into_view("settings-content", "settings-detailed-logging-hint");
    assert!(
        h.bounds("settings-detailed-logging-hint").origin.x >= px(0.0),
        "the switch's hint is drawn"
    );
    assert_eq!(
        h.root(|root, _| root.detailed_logging_hint()),
        "Records why each session changes status. Fills the 5 MiB log files faster."
    );
}

#[gpui::test]
fn a_toggle_is_pending_until_apply(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = general_with_keep_awake(cx, &dir);
    let before = copy_saved(&dir);
    h.click_on("settings-keep-awake-always");
    press_copy(&mut h);
    assert_eq!(copy_saved(&dir), before, "no native-ui.json write");
    assert!(keep_awake_sends(&h.sent()).is_empty(), "no message");
    assert_eq!(
        h.root(|root, _| root.keep_awake_mode()),
        Some(KeepAwakeMode::Always),
        "the pending choice is drawn"
    );
    assert!(applies(&mut h), "Apply and OK open");
}

#[gpui::test]
fn a_toggle_pressed_back_is_not_pending(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = general_with_keep_awake(cx, &dir);
    press_copy(&mut h);
    press_copy(&mut h);
    assert!(!applies(&mut h), "the toggle is back to its saved value");
    h.scroll_into_view("settings-content", "settings-keep-awake-always");
    h.click_on("settings-keep-awake-always");
    h.click_on("settings-keep-awake-off");
    assert!(!applies(&mut h), "the choice is back to the daemon's");
}

#[gpui::test]
fn apply_writes_a_pending_toggle_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = general_with_keep_awake(cx, &dir);
    let before = copy_saved(&dir).unwrap_or(false);
    h.click_on("settings-keep-awake-always");
    press_copy(&mut h);
    h.click_on("settings-apply");
    assert_eq!(copy_saved(&dir), Some(!before), "written on Apply");
    assert_eq!(keep_awake_sends(&h.sent()), [KeepAwakeMode::Always]);
    assert!(!applies(&mut h), "nothing is pending after Apply");
    h.click_on("settings-apply");
    assert!(keep_awake_sends(&h.sent()).is_empty(), "sent once");
    assert!(
        h.root(|root, _| root.settings_open()),
        "Apply keeps Settings open"
    );
}

#[gpui::test]
fn ok_with_local_edits_closes_at_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = general_with_keep_awake(cx, &dir);
    let before = copy_saved(&dir).unwrap_or(false);
    press_copy(&mut h);
    h.click_on("settings-ok");
    assert!(
        !h.root(|root, _| root.settings_open()),
        "OK closed Settings"
    );
    assert_eq!(copy_saved(&dir), Some(!before));
}

#[gpui::test]
fn ok_closes_after_every_daemon_echo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = general_with_keep_awake(cx, &dir);
    let before = copy_saved(&dir).unwrap_or(false);
    h.click_on("settings-keep-awake-always");
    press_copy(&mut h);
    h.click_on("settings-ok");
    assert_eq!(keep_awake_sends(&h.sent()), [KeepAwakeMode::Always]);
    assert_eq!(copy_saved(&dir), Some(!before), "the local write is made");
    assert!(
        h.root(|root, _| root.settings_open()),
        "OK waits for the echo"
    );
    assert!(h.root(|root, _| root.settings_ok_waiting()));
    h.send(keep_awake_status(KeepAwakeMode::Always, false));
    assert!(
        !h.root(|root, _| root.settings_open()),
        "the echo closes it"
    );
}

#[gpui::test]
fn a_daemon_refusal_on_apply_keeps_the_dialog_open_and_marks_the_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = general_with_keep_awake(cx, &dir);
    let before = copy_saved(&dir).unwrap_or(false);
    press_copy(&mut h);
    h.click_on("settings-tab-worktrees");
    h.click_on("settings-worktrees-root-input");
    type_text(&mut h, "X:/nope");
    h.click_on("settings-ok");
    assert_eq!(worktrees_root_sets(&h.sent()), [Some("X:/nope".to_owned())]);
    refuse(&mut h, None, WORKTREES_ROOT_REFUSAL);
    assert!(
        h.root(|root, _| root.settings_open()),
        "the refusal keeps it open"
    );
    assert!(
        !h.root(|root, _| root.settings_ok_waiting()),
        "OK no longer waits"
    );
    assert_eq!(toast_titles(&mut h).len(), 1, "the refusal is shown");
    assert_eq!(
        h.root(|root, _| root.worktrees_path()).as_deref(),
        Some("X:/nope"),
        "the refused row keeps its draft"
    );
    assert!(applies(&mut h), "and is pending again");
    assert_eq!(copy_saved(&dir), Some(!before), "the other row was applied");
}

#[gpui::test]
fn ok_stays_open_when_the_host_drops_before_the_echo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = general_with_keep_awake(cx, &dir);
    h.click_on("settings-keep-awake-always");
    h.click_on("settings-tab-worktrees");
    h.click_on("settings-worktrees-root-input");
    type_text(&mut h, "X:/kept");
    h.click_on("settings-ok");
    let mut dropped = welcomed();
    dropped.on_closed("the daemon went away".to_owned());
    h.event(NetEvent::State(dropped));
    assert!(
        h.root(|root, _| root.settings_open()),
        "Settings stays open"
    );
    assert!(!h.root(|root, _| root.settings_ok_waiting()));
    assert_eq!(
        h.root(|root, _| root.worktrees_path()).as_deref(),
        Some("X:/kept"),
        "the drafts are kept"
    );
}

#[gpui::test]
fn an_edit_made_while_ok_waits_is_kept(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = general_with_keep_awake(cx, &dir);
    let before = copy_saved(&dir);
    h.click_on("settings-keep-awake-always");
    h.click_on("settings-ok");
    assert!(h.root(|root, _| root.settings_ok_waiting()));
    press_copy(&mut h);
    h.send(keep_awake_status(KeepAwakeMode::Always, false));
    assert!(
        h.root(|root, _| root.settings_open()),
        "the edit made while OK waited keeps Settings open"
    );
    assert!(
        !h.root(|root, _| root.settings_ok_waiting()),
        "the wait ends"
    );
    assert!(applies(&mut h), "the new edit stays pending");
    assert_eq!(copy_saved(&dir), before, "and unsaved");
}

#[gpui::test]
fn a_dashboard_draft_survives_a_tab_switch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = dashboard_tab(cx, &dir, dashboard_status("cli"));
    h.click_on("settings-dashboard-cap");
    h.keys("ctrl-a");
    type_text(&mut h, "3");
    h.click_on("settings-tab-general");
    h.click_on("settings-tab-dashboard");
    assert_eq!(
        h.root(rustling_tulip_native::RootView::dashboard_cap_draft)
            .as_deref(),
        Some("3"),
        "the typed cap is still there"
    );
    assert!(applies(&mut h), "and pending");
}

#[gpui::test]
fn a_worktrees_draft_survives_a_tab_switch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(DaemonMessage::WorktreesRootChanged {
        root: "C:/default".to_owned(),
        is_override: false,
    });
    h.keys("ctrl-,");
    h.click_on("settings-tab-worktrees");
    h.click_on("settings-worktrees-root-input");
    type_text(&mut h, "X:/kept");
    h.click_on("settings-tab-general");
    h.click_on("settings-tab-worktrees");
    assert_eq!(
        h.root(|root, _| root.worktrees_path()).as_deref(),
        Some("X:/kept"),
        "the typed path is still there"
    );
    assert!(applies(&mut h), "and pending");
}

/// One of a host's default-shell choices.
fn shell(id: &str, label: &str, available: bool, hosts_agents: bool) -> protocol::ShellChoice {
    protocol::ShellChoice {
        id: id.to_owned(),
        label: label.to_owned(),
        available,
        path: None,
        hosts_agents,
    }
}

/// The daemon's shell choices: Automatic, PowerShell 7, WSL (agents start
/// on their own), fish (not installed) and Custom, whose `hosts_agents` is
/// `custom_hosts_agents`.
fn shell_choices(custom_hosts_agents: bool) -> DaemonMessage {
    DaemonMessage::ShellChoices {
        choices: vec![
            shell("auto", "Automatic", true, true),
            shell("pwsh", "PowerShell 7", true, true),
            shell("wsl", "WSL", true, false),
            shell("fish", "fish", false, false),
            shell("custom", "Custom", true, custom_hosts_agents),
        ],
    }
}

/// The daemon's saved default shell.
fn default_shell(id: &str, command: Option<&str>) -> DaemonMessage {
    DaemonMessage::DefaultShell {
        id: id.to_owned(),
        command: command.map(str::to_owned),
    }
}

/// How many times the client asked for the shell choices among `sent`.
fn shell_lists(sent: &[ClientMessage]) -> usize {
    sent.iter()
        .filter(|msg| matches!(msg, ClientMessage::ListShellChoices))
        .count()
}

/// The `(id, command)` of every default shell set among `sent`.
fn shell_sets(sent: &[ClientMessage]) -> Vec<(String, Option<String>)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetDefaultShell { id, command, .. } => {
                Some((id.clone(), command.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The harness on This PC's daemon welcomed with the `default_shell`
/// feature beside the usual ones, PowerShell 7 saved, nothing sent yet.
fn shell_host<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = opened(cx, dir);
    h.event(NetEvent::State(shell_connection()));
    h.send(default_shell("pwsh", None));
    h.sent();
    h
}

/// This PC's connection open and welcomed with the `default_shell`
/// feature beside the usual ones.
fn shell_connection() -> Connection {
    let mut features = support::daemon_features();
    features.push(protocol::FEATURE_DEFAULT_SHELL.to_owned());
    let mut conn = Connection::for_target(rustling_tulip_native::target::Target::Local);
    conn.on_connecting();
    conn.on_socket_open(4242);
    conn.on_welcome(support::PROTOCOL, features, None);
    conn
}

const SHELL_REFUSAL: &str = "The custom shell command has an unclosed quote.";

/// Picks Custom, types `command` into its field and presses Apply; returns
/// the request id the set carried.
fn apply_custom(h: &mut Harness<'_>, command: &str) -> Option<String> {
    pick_shell(h, "custom");
    h.scroll_into_view("settings-content", "settings-default-shell-command");
    h.click_on("settings-default-shell-command");
    h.keys("end");
    type_line(h, command);
    h.click_on("settings-apply");
    h.sent().iter().rev().find_map(|msg| match msg {
        ClientMessage::SetDefaultShell { request_id, .. } => request_id.clone(),
        _ => None,
    })
}

/// Settings open on General with the Default shell row in view, the
/// choices asked for once and answered with [`shell_choices`].
fn shell_row_open(h: &mut Harness<'_>, custom_hosts_agents: bool) {
    h.keys("ctrl-,");
    assert_eq!(shell_lists(&h.sent()), 1, "opening Settings asks once");
    h.send(shell_choices(custom_hosts_agents));
    h.scroll_into_view("settings-content", "settings-default-shell-field");
}

/// Opens the Default shell list and picks `id`'s entry.
fn pick_shell(h: &mut Harness<'_>, id: &str) {
    h.click_on("settings-default-shell-field");
    assert!(h.root(|root, _| root.shell_list_open()), "the list opens");
    h.click_on(&format!("settings-shell-option-{id}"));
}

fn shell_text(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.default_shell_text())
}

fn shell_note(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.default_shell_note())
}

#[gpui::test]
fn default_shell_row_lists_host_choices_with_unavailable_dimmed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shell_host(cx, &dir);
    h.keys("ctrl-,");
    assert_eq!(shell_lists(&h.sent()), 1, "opening Settings asks once");
    assert_eq!(shell_text(&mut h).as_deref(), Some("Loading shells…"));
    assert!(!h.root(|root, _| root.default_shell_live()));
    h.scroll_into_view("settings-content", "settings-default-shell-field");
    h.click_on("settings-default-shell-field");
    assert!(
        !h.root(|root, _| root.shell_list_open()),
        "the list does not open before the choices arrive"
    );
    assert_eq!(
        shell_note(&mut h),
        None,
        "no note before the choices arrive"
    );

    h.send(shell_choices(true));
    assert_eq!(shell_text(&mut h).as_deref(), Some("PowerShell 7"));
    h.click_on("settings-default-shell-field");
    assert!(h.root(|root, _| root.shell_list_open()));
    let entries = h.root(|root, _| root.default_shell_entries());
    let ids: Vec<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
    assert_eq!(
        ids,
        ["auto", "pwsh", "wsl", "fish", "custom"],
        "the daemon's order"
    );
    let labels: Vec<&str> = entries.iter().map(|entry| entry.label.as_str()).collect();
    assert_eq!(
        labels,
        ["Automatic", "PowerShell 7", "WSL", "fish", "Custom"]
    );
    let fish = entries
        .iter()
        .find(|entry| entry.id == "fish")
        .expect("listed");
    assert!(!fish.pickable, "a shell the host lacks cannot be picked");
    assert_eq!(fish.tip.as_deref(), Some("Not installed on This PC"));
    assert!(
        entries
            .iter()
            .filter(|entry| entry.id != "fish")
            .all(|entry| entry.pickable && entry.tip.is_none())
    );
    assert!(drawn(&mut h, "settings-shell-option-fish"), "drawn, dimmed");
    h.click_on("settings-shell-option-fish");
    assert_eq!(
        shell_text(&mut h).as_deref(),
        Some("PowerShell 7"),
        "a press on fish picks nothing"
    );
    assert!(!h.root(|root, _| root.settings_apply_enabled()));
}

#[gpui::test]
fn default_shell_pick_applies_on_ok_and_closes_on_echo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shell_host(cx, &dir);
    shell_row_open(&mut h, true);
    pick_shell(&mut h, "auto");
    assert_eq!(shell_text(&mut h).as_deref(), Some("Automatic"));
    assert_eq!(shell_sets(&h.sent()), Vec::new(), "a pick sends nothing");

    h.click_on("settings-ok");
    let sent = h.sent();
    assert_eq!(shell_sets(&sent), [("auto".to_owned(), None)]);
    let request_id = sent.iter().find_map(|msg| match msg {
        ClientMessage::SetDefaultShell { request_id, .. } => request_id.clone(),
        _ => None,
    });
    assert!(request_id.is_some(), "the set carries a request id");
    assert!(h.root(|root, _| root.settings_ok_waiting()));
    assert!(
        h.root(|root, _| root.settings_open()),
        "OK waits for the echo"
    );

    h.send(default_shell("auto", None));
    assert!(
        !h.root(|root, _| root.settings_open()),
        "the echo closes Settings"
    );
}

#[gpui::test]
fn default_shell_custom_shows_command_field_and_sends_command(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shell_host(cx, &dir);
    shell_row_open(&mut h, true);
    assert!(
        h.root(|root, _| root.default_shell_command()).is_none(),
        "no command field while PowerShell 7 is chosen"
    );
    pick_shell(&mut h, "custom");
    assert_eq!(
        h.root(|root, _| root.default_shell_command()).as_deref(),
        Some("")
    );
    assert!(drawn(&mut h, "settings-default-shell-command"));
    h.scroll_into_view("settings-content", "settings-default-shell-command");
    h.click_on("settings-default-shell-command");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-default-shell-command")
    );
    type_line(&mut h, "nu -l");
    assert_eq!(
        h.root(|root, _| root.default_shell_command()).as_deref(),
        Some("nu -l")
    );
    h.keys("enter");
    assert_eq!(shell_sets(&h.sent()), Vec::new(), "Enter applies nothing");

    h.click_on("settings-apply");
    assert_eq!(
        shell_sets(&h.sent()),
        [("custom".to_owned(), Some("nu -l".to_owned()))]
    );
}

#[gpui::test]
fn default_shell_refetches_choices_after_echo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shell_host(cx, &dir);
    shell_row_open(&mut h, true);
    pick_shell(&mut h, "custom");
    h.scroll_into_view("settings-content", "settings-default-shell-command");
    h.click_on("settings-default-shell-command");
    type_line(&mut h, "nu");
    h.click_on("settings-apply");
    let sent = h.sent();
    assert_eq!(
        shell_sets(&sent),
        [("custom".to_owned(), Some("nu".to_owned()))]
    );
    assert_eq!(shell_lists(&sent), 0, "nothing asked before the echo");

    h.send(default_shell("custom", Some("nu")));
    assert_eq!(
        shell_lists(&h.sent()),
        1,
        "the echo asks for the choices again"
    );
    assert_eq!(shell_note(&mut h), None);
    h.send(shell_choices(false));
    assert_eq!(
        shell_note(&mut h).as_deref(),
        Some("Agents start on their own, not inside Custom."),
        "the fresh choices say Custom does not host agents"
    );

    h.send(default_shell("pwsh", None));
    assert_eq!(
        shell_lists(&h.sent()),
        1,
        "another client's set asks again too"
    );
}

#[gpui::test]
fn default_shell_note_shows_for_shell_that_does_not_host_agents(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shell_host(cx, &dir);
    shell_row_open(&mut h, true);
    assert_eq!(shell_note(&mut h), None, "PowerShell 7 hosts agents");
    assert!(!drawn(&mut h, "settings-default-shell-note"));

    pick_shell(&mut h, "wsl");
    assert_eq!(
        shell_note(&mut h).as_deref(),
        Some("Agents start on their own, not inside WSL.")
    );
    assert!(drawn(&mut h, "settings-default-shell-note"));

    pick_shell(&mut h, "pwsh");
    assert_eq!(shell_note(&mut h), None, "back on PowerShell 7");
}

#[gpui::test]
fn default_shell_row_disabled_for_older_daemon(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    assert_eq!(
        shell_lists(&h.sent()),
        0,
        "nothing is sent to an older daemon"
    );
    assert_eq!(
        h.root(|root, _| root.default_shell_tip()),
        Some("Update this host's daemon to choose a default shell")
    );
    assert!(!h.root(|root, _| root.default_shell_live()));
    h.scroll_into_view("settings-content", "settings-default-shell-field");
    assert!(
        drawn(&mut h, "settings-default-shell-field"),
        "drawn disabled"
    );
    h.click_on("settings-default-shell-field");
    assert!(
        !h.root(|root, _| root.shell_list_open()),
        "it does not open"
    );
    assert_eq!(shell_note(&mut h), None);
    assert_eq!(shell_lists(&h.sent()), 0);
}

#[gpui::test]
fn default_shell_pending_not_sent_to_older_or_closed_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shell_host(cx, &dir);
    shell_row_open(&mut h, true);
    pick_shell(&mut h, "wsl");
    h.welcome_without_features();
    h.click_on("settings-ok");
    assert_eq!(
        shell_sets(&h.sent()),
        Vec::new(),
        "an older daemon is sent nothing"
    );
    assert!(
        !h.root(|root, _| root.settings_open()),
        "no echo is owed, so OK closes"
    );

    h.event(NetEvent::State(shell_connection()));
    h.send(default_shell("pwsh", None));
    shell_row_open(&mut h, true);
    pick_shell(&mut h, "wsl");
    let mut closed = Connection::for_target(rustling_tulip_native::target::Target::Local);
    closed.on_connecting();
    h.event(NetEvent::State(closed));
    h.click_on("settings-ok");
    assert_eq!(
        shell_sets(&h.sent()),
        Vec::new(),
        "a closed connection is sent nothing"
    );
    assert!(
        !h.root(|root, _| root.settings_open()),
        "no echo is owed, so OK closes"
    );
}

#[gpui::test]
fn default_shell_field_follows_saved_command_after_echo_and_refusal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shell_host(cx, &dir);
    h.send(default_shell("custom", Some("nu")));
    shell_row_open(&mut h, true);
    assert_eq!(
        h.root(|root, _| root.default_shell_command()).as_deref(),
        Some("nu"),
        "the field holds the saved command"
    );
    h.scroll_into_view("settings-content", "settings-default-shell-command");
    h.click_on("settings-default-shell-command");
    h.keys("end");
    type_line(&mut h, " -l");
    assert_eq!(
        h.root(|root, _| root.default_shell_command()).as_deref(),
        Some("nu -l")
    );
    h.click_on("settings-apply");
    let id = h.sent().iter().find_map(|msg| match msg {
        ClientMessage::SetDefaultShell { request_id, .. } => request_id.clone(),
        _ => None,
    });
    refuse(&mut h, id.as_deref(), SHELL_REFUSAL);
    assert_eq!(
        h.root(|root, _| root.default_shell_refusal()).as_deref(),
        Some(SHELL_REFUSAL)
    );
    assert_eq!(
        h.root(|root, _| root.default_shell_command()).as_deref(),
        Some("nu"),
        "a refusal puts the saved command back"
    );

    h.send(default_shell("custom", Some("fish")));
    assert_eq!(
        h.root(|root, _| root.default_shell_command()).as_deref(),
        Some("fish"),
        "another client's set shows its command"
    );
}

#[gpui::test]
fn default_shell_refusal_clears_when_settings_close(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shell_host(cx, &dir);
    shell_row_open(&mut h, true);
    let id = apply_custom(&mut h, "nu");
    assert!(id.is_some(), "Apply sent the set");
    refuse(&mut h, id.as_deref(), SHELL_REFUSAL);
    assert_eq!(
        h.root(|root, _| root.default_shell_refusal()).as_deref(),
        Some(SHELL_REFUSAL)
    );

    h.click_on("settings-cancel");
    assert!(!h.root(|root, _| root.settings_open()));
    h.keys("ctrl-,");
    assert_eq!(
        h.root(|root, _| root.default_shell_refusal()),
        None,
        "closing Settings drops the refusal"
    );
}
