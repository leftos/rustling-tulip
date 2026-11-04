//! Settings search specs: the search box over the tab list filters the list
//! to the tabs holding a match, each with its count; picking a tab
//! highlights its first match and scrolls to it; Enter, Esc, Ctrl+F and Tab
//! treat the box as one more field of the frame.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::time::Duration;

use gpui::{TestAppContext, px};
use protocol::{AgentCommandEntry, AgentId, DaemonMessage, KeepAwakeMode, OptionValue};
use rustling_tulip_native::RootView;
use rustling_tulip_native::target::Target;
use support::{Fixture, Harness, TestDir, session};

/// The tailnet host the remote spec holds beside This PC.
const REMOTE: &str = "box.tail1234.ts.net";

/// The harness on one session, its scrollback answered, the title settled
/// and what it sent drained.
fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    h.advance(Duration::from_millis(400));
    h.sent();
    h
}

/// Settings open with the keyboard in the search box.
fn searching<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = opened(cx, dir);
    h.keys("ctrl-,");
    h.click_on("settings-search");
    assert_eq!(settings_focus(&mut h), Some("settings-search"));
    h
}

/// Which part of the Settings frame has the keyboard.
fn settings_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).settings_focus(window, cx))
}

/// Types `text` into whatever has the keyboard.
fn type_text(h: &mut Harness<'_>, text: &str) {
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

/// Empties the search box, which has the keyboard, and types `text`.
fn search(h: &mut Harness<'_>, text: &str) {
    h.keys("ctrl-a backspace");
    type_text(h, text);
}

/// The rows the tab list draws, a listed tab's count after it.
fn rows(h: &mut Harness<'_>) -> Vec<String> {
    h.root(RootView::settings_search_rows)
}

/// The selector of the highlighted setting, while one is drawn.
fn hit(h: &mut Harness<'_>) -> Option<&'static str> {
    h.root(RootView::settings_search_hit)
}

fn shown_tab(h: &mut Harness<'_>) -> &'static str {
    h.root(|root, _| root.settings_tab())
}

fn search_text(h: &mut Harness<'_>) -> String {
    h.root(RootView::settings_search_text)
}

fn saved_ui(dir: &TestDir) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("a saved layout");
    serde_json::from_str(&text).expect("native-ui.json is JSON")
}

/// Whether the element tagged `selector` lies wholly inside the element
/// tagged `container`.
fn inside(h: &mut Harness<'_>, container: &str, selector: &str) -> bool {
    let outer = h.bounds(container);
    let inner = h.bounds(selector);
    inner.top() >= outer.top() && inner.bottom() <= outer.bottom()
}

/// Whether the highlight's outline is drawn round the control tagged
/// `selector`.
fn outlined(h: &mut Harness<'_>, selector: &str) -> bool {
    let outline = h.bounds("settings-search-hit");
    let control = h.bounds(selector);
    outline.size.width > px(0.0)
        && outline.left() <= control.left()
        && outline.right() >= control.right()
        && outline.top() <= control.top()
        && outline.bottom() >= control.bottom()
}

#[gpui::test]
fn typing_filters_the_tab_list_with_counts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = searching(cx, &dir);
    type_text(&mut h, "logs");
    assert_eq!(rows(&mut h), ["General  2"], "both Diagnostics rows");
    assert!(
        h.bounds("settings-tab-general-count").origin.x >= px(0.0),
        "the count is drawn beside the label"
    );
    search(&mut h, "width");
    assert_eq!(rows(&mut h), ["General  1", "Appearance  1"]);
}

#[gpui::test]
fn a_blank_query_shows_every_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = searching(cx, &dir);
    let every: Vec<String> = h
        .root(|root, _| root.settings_tab_rows())
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(rows(&mut h), every, "an empty box lists every tab");
    type_text(&mut h, "   ");
    assert_eq!(rows(&mut h), every, "a box of spaces lists every tab");
    assert_eq!(hit(&mut h), None, "a blank query highlights nothing");
}

#[gpui::test]
fn a_query_with_no_match_shows_the_no_match_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = searching(cx, &dir);
    type_text(&mut h, "zzqx");
    assert_eq!(rows(&mut h), ["No settings match"]);
    assert!(
        h.bounds("settings-search-no-match").origin.x >= px(0.0),
        "the line is drawn"
    );
    assert_eq!(hit(&mut h), None);
}

#[gpui::test]
fn the_selected_tab_stays_when_it_matches_else_the_first_match_opens(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-appearance");
    h.click_on("settings-search");
    type_text(&mut h, "width");
    assert_eq!(
        shown_tab(&mut h),
        "Appearance",
        "the shown tab is listed, so it stays"
    );
    assert_eq!(hit(&mut h), Some("appearance-frame-width-down"));
    search(&mut h, "logs");
    assert_eq!(
        shown_tab(&mut h),
        "General",
        "Appearance is not listed, so the first listed tab opens"
    );
    assert_eq!(hit(&mut h), Some("settings-detailed-logging"));
}

#[gpui::test]
fn picking_a_filtered_tab_highlights_its_first_match(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = searching(cx, &dir);
    type_text(&mut h, "width");
    assert_eq!(shown_tab(&mut h), "General");
    assert_eq!(hit(&mut h), Some("settings-chat-width-input"));
    assert!(
        outlined(&mut h, "settings-chat-width-input"),
        "the hit row is outlined"
    );

    h.click_on("settings-tab-appearance");
    assert_eq!(shown_tab(&mut h), "Appearance");
    assert_eq!(hit(&mut h), Some("appearance-frame-width-down"));
    assert!(
        outlined(&mut h, "appearance-frame-width-down"),
        "an Appearance row is outlined too"
    );

    h.click_on("settings-tab-general");
    h.keys("down");
    assert_eq!(
        shown_tab(&mut h),
        "Appearance",
        "Down walks the filtered list"
    );
    assert_eq!(hit(&mut h), Some("appearance-frame-width-down"));
}

#[gpui::test]
fn picking_a_tab_scrolls_the_first_match_into_view(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.cx.simulate_resize(gpui::size(px(1200.0), px(600.0)));
    h.keys("ctrl-,");
    assert!(
        !inside(&mut h, "settings-content", "settings-detailed-logging"),
        "the Diagnostics row starts below the fold"
    );
    h.click_on("settings-tab-appearance");
    h.click_on("settings-search");
    type_text(&mut h, "logs");
    h.click_on("settings-tab-general");
    assert_eq!(hit(&mut h), Some("settings-detailed-logging"));
    assert!(
        inside(&mut h, "settings-content", "settings-detailed-logging"),
        "picking General scrolled its first match into view"
    );
}

#[gpui::test]
fn search_finds_an_agent_page_setting(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = searching(cx, &dir);
    type_text(&mut h, "extra arguments");
    assert_eq!(
        rows(&mut h),
        ["Agents", "Claude  1"],
        "the Agents group row stays over its listed page"
    );
    assert_eq!(shown_tab(&mut h), "Claude");
    assert_eq!(hit(&mut h), Some("settings-agent-args"));
}

#[gpui::test]
fn search_finds_the_chat_and_logging_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = searching(cx, &dir);
    type_text(&mut h, "chat view");
    assert_eq!(rows(&mut h).first().map(String::as_str), Some("General  1"));
    assert_eq!(hit(&mut h), Some("settings-chat-mode-compact"));
    search(&mut h, "px");
    assert_eq!(hit(&mut h), Some("settings-chat-width-input"));
    search(&mut h, "detailed logging");
    assert_eq!(rows(&mut h), ["General  1"]);
    assert_eq!(hit(&mut h), Some("settings-detailed-logging"));
    search(&mut h, "open logs");
    assert_eq!(hit(&mut h), Some("settings-open-logs-folder"));
}

#[gpui::test]
fn searching_keeps_pending_edits(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.scroll_into_view("settings-content", "settings-terminal-copy-on-selection");
    h.click_on("settings-terminal-copy-on-selection");
    assert!(
        h.root(|root, _| root.settings_have_pending_edits()),
        "the click pended the toggle"
    );
    h.click_on("settings-search");
    type_text(&mut h, "width");
    h.click_on("settings-tab-appearance");
    assert_eq!(shown_tab(&mut h), "Appearance");
    assert!(
        h.root(|root, _| root.settings_have_pending_edits()),
        "searching and picking a tab keep the pending toggle"
    );
    h.click_on("settings-apply");
    assert_eq!(
        saved_ui(&dir)["general"]["copy_on_select"],
        false,
        "Apply sends the toggle pended before the search"
    );
}

#[gpui::test]
fn enter_in_the_search_box_focuses_the_highlighted_setting(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = searching(cx, &dir);
    type_text(&mut h, "zzqx");
    h.keys("enter");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-search"),
        "with no hit Enter does nothing"
    );
    search(&mut h, "copy selection");
    h.keys("enter");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-terminal-copy-on-selection"),
        "Enter moves the keyboard to the highlighted control"
    );
    assert!(
        !h.root(|root, _| root.settings_have_pending_edits()),
        "Enter does not press it"
    );
    h.keys("tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-chat-mode-compact"),
        "the ring goes on from there"
    );
}

#[gpui::test]
fn escape_clears_a_non_empty_search_then_closes_settings(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = searching(cx, &dir);
    let every: Vec<String> = rows(&mut h);
    type_text(&mut h, "logs");
    h.keys("escape");
    assert!(
        h.root(|root, _| root.settings_open()),
        "Settings stays open"
    );
    assert_eq!(search_text(&mut h), "", "Esc cleared the box");
    assert_eq!(rows(&mut h), every, "the full list is back");
    assert_eq!(hit(&mut h), None, "the highlight went with the query");
    h.keys("escape");
    assert!(
        !h.root(|root, _| root.settings_open()),
        "Esc in the emptied box closes Settings"
    );
}

#[gpui::test]
fn escape_in_an_empty_search_closes_settings(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = searching(cx, &dir);
    h.keys("escape");
    assert!(!h.root(|root, _| root.settings_open()));
}

#[gpui::test]
fn ctrl_f_focuses_the_search_box(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    assert_eq!(
        settings_focus(&mut h),
        Some("list"),
        "Settings opens on the tab list, not the search box"
    );
    h.keys("ctrl-f");
    assert_eq!(settings_focus(&mut h), Some("settings-search"));
    type_text(&mut h, "font");
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("list"));
    h.keys("ctrl-f");
    assert_eq!(settings_focus(&mut h), Some("settings-search"));
    type_text(&mut h, "logs");
    assert_eq!(
        search_text(&mut h),
        "logs",
        "Ctrl+F selected the text, so typing replaced it"
    );
}

#[gpui::test]
fn the_search_box_is_a_tab_stop_before_the_tab_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.keys("shift-tab");
    assert_eq!(settings_focus(&mut h), Some("settings-search"));
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("list"));
    h.keys("shift-tab shift-tab");
    assert_eq!(
        settings_focus(&mut h),
        Some("settings-cancel"),
        "before the search box comes the footer's last stop"
    );
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some("settings-search"));
    type_text(&mut h, "x");
    assert_eq!(search_text(&mut h), "x", "the box takes typed keys");
}

#[gpui::test]
fn the_search_clears_when_settings_closes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = searching(cx, &dir);
    let every: Vec<String> = rows(&mut h);
    type_text(&mut h, "logs");
    h.click_on("settings-close");
    assert!(!h.root(|root, _| root.settings_open()));
    assert_eq!(hit(&mut h), None, "no highlight outlives Settings");
    h.keys("ctrl-,");
    assert_eq!(search_text(&mut h), "", "the query is not kept");
    assert_eq!(rows(&mut h), every);
    assert_eq!(hit(&mut h), None);
}

#[gpui::test]
fn remote_access_results_hide_while_its_page_is_hidden(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let remote = Target::tailnet(REMOTE, None);
    let host_row = format!(
        "settings-host-{}",
        rustling_tulip_native::target::HostKey::of(&remote)
            .wire()
            .expect("a remote host has a wire key")
    );
    let mut host = h.host(&remote);
    host.connect();
    host.sent();
    h.sent();
    h.keys("ctrl-,");
    h.click_on("settings-search");
    type_text(&mut h, "tailnet");
    assert_eq!(rows(&mut h), ["Remote access  1"], "This PC lists the page");

    h.click_on(&host_row);
    assert!(
        !h.root(|root, _| root.settings_tab_rows())
            .contains(&"Remote access"),
        "box's list leaves the page out"
    );
    assert_eq!(
        rows(&mut h),
        ["No settings match"],
        "the hidden page's rows are not found"
    );
}

/// The settings-host row of `target` in the Settings host selector.
fn host_row(target: &Target) -> String {
    format!(
        "settings-host-{}",
        rustling_tulip_native::target::HostKey::of(target)
            .wire()
            .expect("a remote host has a wire key")
    )
}

/// Whether the tab list lists a tab whose label starts with `label`.
fn listed(h: &mut Harness<'_>, label: &str) -> bool {
    rows(h).iter().any(|row| row.starts_with(label))
}

/// The daemon lists its registered agents, then `second`.
fn list_agents(h: &mut Harness<'_>, second: protocol::AgentDescriptor) {
    let mut agents = agent_registry::descriptors();
    agents.push(second);
    h.send(DaemonMessage::Agents { agents });
}

#[gpui::test]
fn a_dashboard_row_the_key_source_hides_is_not_a_result(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(
        support::summarizer_status()
            .offers("keyed", "Keyed model", true, "Sends session text.")
            .chosen("keyed")
            .build(),
    );
    h.keys("ctrl-,");
    h.click_on("settings-search");
    type_text(&mut h, "key");
    assert!(
        listed(&mut h, "Dashboard"),
        "a keyed provider draws the Key rows, so they are found"
    );
    search(&mut h, "variable");
    assert!(
        !listed(&mut h, "Dashboard"),
        "with a stored key the Variable row is not drawn, so it is not found: {:?}",
        rows(&mut h)
    );
}

#[gpui::test]
fn manage_worktrees_is_not_a_result_where_it_is_not_drawn(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let remote = Target::tailnet(REMOTE, None);
    let mut host = h.host(&remote);
    host.connect();
    host.sent();
    h.sent();
    h.keys("ctrl-,");
    h.click_on("settings-search");
    type_text(&mut h, "manage");
    assert!(
        listed(&mut h, "Worktrees"),
        "This PC draws Manage worktrees…, so it is found"
    );
    h.click_on(&host_row(&remote));
    assert!(
        !listed(&mut h, "Worktrees"),
        "a remote host draws no Manage worktrees…, so it is not found: {:?}",
        rows(&mut h)
    );
}

#[gpui::test]
fn an_agent_field_a_when_rule_hides_is_not_a_result(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    list_agents(&mut h, agent_api::test_agent::DESCRIPTOR.clone());
    h.keys("ctrl-,");
    h.click_on("settings-search");
    type_text(&mut h, "profile");
    assert!(
        listed(&mut h, "Test agent"),
        "the profile field is drawn while sandbox is off"
    );
    h.send(DaemonMessage::AgentSettings {
        settings: std::collections::BTreeMap::from([(
            AgentId("test-agent".to_owned()),
            [("sandbox".to_owned(), OptionValue::Bool(true))]
                .into_iter()
                .collect(),
        )]),
    });
    assert!(
        !listed(&mut h, "Test agent"),
        "the sandbox rule hides profile, so it is not found: {:?}",
        rows(&mut h)
    );
}

#[gpui::test]
fn an_agent_field_this_client_cannot_edit_is_outlined_or_not_listed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let mut agent = agent_api::test_agent::DESCRIPTOR.clone();
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
    h.click_on("settings-search");
    type_text(&mut h, "future");
    assert_eq!(shown_tab(&mut h), "Test agent");
    assert_eq!(hit(&mut h), Some("settings-test-future"));
    assert!(
        outlined(&mut h, "settings-test-future"),
        "the listed row draws the outline like every other row"
    );
}

#[gpui::test]
fn switching_hosts_while_searching_refilters_and_keeps_a_listed_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let remote = Target::tailnet(REMOTE, None);
    let mut host = h.host(&remote);
    host.connect();
    host.send(DaemonMessage::Agents {
        agents: vec![agent_api::test_agent::DESCRIPTOR.clone()],
    });
    host.sent();
    h.sent();
    h.keys("ctrl-,");
    h.click_on("settings-search");
    type_text(&mut h, "logs");
    assert_eq!(rows(&mut h), ["General  2"]);
    h.click_on(&host_row(&remote));
    assert_eq!(
        rows(&mut h),
        ["No settings match"],
        "the Diagnostics rows are This PC's only"
    );
    assert_eq!(hit(&mut h), None);

    h.click_on("settings-search");
    search(&mut h, "extra arguments");
    assert_eq!(shown_tab(&mut h), "Test agent");
    h.click_on("settings-host-local");
    assert_eq!(
        rows(&mut h),
        ["Agents", "Claude  1"],
        "This PC lists its own agent page"
    );
    assert_eq!(
        shown_tab(&mut h),
        "Claude",
        "the shown tab is a listed one, not the General page the switch fell back to"
    );
    assert_eq!(hit(&mut h), Some("settings-agent-args"));
}

#[gpui::test]
fn ctrl_f_closes_an_open_sound_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-notifications");
    h.click_on("settings-needs-you-sound-field");
    assert!(
        h.root(|root, _| root.open_sound_list()).is_some(),
        "the click opened the sound list"
    );
    h.keys("ctrl-f");
    assert!(
        h.root(|root, _| root.open_sound_list()).is_none(),
        "Ctrl+F closed the sound list"
    );
    assert_eq!(settings_focus(&mut h), Some("settings-search"));
}

/// The selectors the Tab ring reaches on the shown page, from the tab list
/// round to it, less the frame's own stops: the search box and the footer.
fn page_ring(h: &mut Harness<'_>) -> Vec<&'static str> {
    let frame = [
        "settings-search",
        "settings-ok",
        "settings-apply",
        "settings-cancel",
    ];
    let mut reached = Vec::new();
    let mut back = false;
    for _ in 0..80 {
        h.keys("tab");
        let focus = settings_focus(h).expect("Tab stays in the Settings frame");
        if focus == "list" {
            back = true;
            break;
        }
        if !frame.contains(&focus) {
            reached.push(focus);
        }
    }
    assert!(back, "Tab never returned to the tab list: {reached:?}");
    reached
}

#[gpui::test]
fn every_ring_stop_on_every_tab_has_a_catalogue_entry(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let mut agents = agent_registry::descriptors();
    agents.push(agent_api::test_agent::DESCRIPTOR.clone());
    let commands = agents
        .iter()
        .map(|descriptor| AgentCommandEntry {
            agent: AgentId(descriptor.id.0.clone()),
            program: String::new(),
            args: String::new(),
            program_env: None,
        })
        .collect();
    h.send(DaemonMessage::Agents { agents });
    h.send(DaemonMessage::AgentCommands { commands });
    h.send(DaemonMessage::KeepAwakeStatus {
        enabled: false,
        mode: KeepAwakeMode::Off,
        active: false,
    });
    // A keyed provider with a stored key draws every Dashboard stop the
    // ring can hold, so the walk reaches them.
    h.send(
        support::summarizer_status()
            .offers("cli", "CLI model", false, "Runs through your agent's CLI.")
            .offers("keyed", "Keyed model", true, "Sends session text.")
            .chosen("keyed")
            .build(),
    );
    h.keys("ctrl-,");
    let tabs = [
        "settings-tab-general",
        "settings-tab-notifications",
        "settings-tab-dashboard",
        "settings-tab-spawn-defaults",
        "settings-tab-worktrees",
        "settings-tab-remote-access",
        "settings-tab-appearance",
        "settings-tab-app-title",
        "settings-tab-test",
    ];
    let mut missing = Vec::new();
    for tab in tabs {
        h.click_on(tab);
        assert_eq!(settings_focus(&mut h), Some("list"), "{tab} took the click");
        let ring = page_ring(&mut h);
        if tab == "settings-tab-dashboard" {
            assert!(
                !ring.is_empty(),
                "the Dashboard's summarizer status gives its ring stops"
            );
        }
        for stop in ring {
            if !h.root(|root, _| root.settings_search_names(stop)) {
                missing.push(format!("{tab}: {stop}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "ring stops no catalogue entry or exemption names: {missing:#?}"
    );
}
