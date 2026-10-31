//! Settings host selector specs: with two hosts held, the agent pages show
//! the chosen host's agents and values, their edits go to that host only,
//! and each host keeps its own drafts.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{TestAppContext, px};
use protocol::{
    AgentCommandEntry, AgentId, ClientMessage, DaemonMessage, KeepAwakeMode, OptionValue,
};
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, NetEvent};
use support::{Fixture, Harness, PROTOCOL, TestDir, daemon_features, session};

/// The tailnet host the specs hold beside This PC.
const REMOTE: &str = "box.tail1234.ts.net";

fn remote() -> Target {
    Target::tailnet(REMOTE, None)
}

fn remote_key() -> HostKey {
    HostKey::of(&remote())
}

/// The selector row of the remote host.
fn remote_row() -> String {
    let key = remote_key();
    format!(
        "settings-host-{}",
        key.wire().expect("a remote host has a wire key")
    )
}

const LOCAL_ROW: &str = "settings-host-local";

/// The daemon's launch commands: Claude's `program`, no extra arguments.
fn claude_command(program: &str) -> DaemonMessage {
    DaemonMessage::AgentCommands {
        commands: vec![AgentCommandEntry {
            agent: AgentId("claude".to_owned()),
            program: program.to_owned(),
            args: String::new(),
            program_env: None,
        }],
    }
}

/// A window on This PC with the tailnet host connected beside it, each
/// daemon reporting Claude's launch command, nothing sent yet.
fn two_hosts<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &Fixture::single(session("a1").build()));
    h.send(claude_command("local-claude"));
    let mut host = h.host(&remote());
    host.connect();
    host.send(claude_command("box-claude"));
    host.sent();
    h.sent();
    h
}

fn shown_host(h: &mut Harness<'_>) -> HostKey {
    h.root(|root, _| root.settings_host().clone())
}

fn shown_tab(h: &mut Harness<'_>) -> &'static str {
    h.root(|root, _| root.settings_tab())
}

fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

fn launch_program(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.launch_program())
}

fn permission_mode(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.settings_field_choice("claude", "permission_mode"))
}

fn type_line(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
}

/// The `(agent, permission_mode)` of every `SetAgentSettings` among `sent`.
fn settings_sets(sent: &[ClientMessage]) -> Vec<(String, Option<OptionValue>)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetAgentSettings { agent, options, .. } => {
                Some((agent.0.clone(), options.get("permission_mode").cloned()))
            }
            _ => None,
        })
        .collect()
}

/// The `(agent, program)` of every `SetAgentCommand` among `sent`.
fn command_sets(sent: &[ClientMessage]) -> Vec<(String, String)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetAgentCommand { agent, program, .. } => {
                Some((agent.0.clone(), program.clone()))
            }
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn an_option_edit_on_the_chosen_host_goes_to_that_host_only(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    let local_before = permission_mode(&mut h);

    h.click_on(&remote_row());
    assert_eq!(shown_host(&mut h), remote_key(), "the selector shows box");
    assert_eq!(
        shown_tab(&mut h),
        "Claude",
        "box's Claude page shows in place of This PC's"
    );
    h.click_on("settings-claude-permission-mode-accept-edits");

    assert_eq!(
        settings_sets(&h.host(&remote()).sent()),
        [(
            "claude".to_owned(),
            Some(OptionValue::Text("accept_edits".to_owned()))
        )],
        "the edit goes to box"
    );
    let local_sent = settings_sets(&h.sent());
    assert!(
        local_sent.is_empty(),
        "nothing goes to This PC: {local_sent:?}"
    );

    h.click_on(LOCAL_ROW);
    assert_eq!(shown_host(&mut h), HostKey::Local);
    assert_eq!(
        permission_mode(&mut h),
        local_before,
        "This PC's page keeps its own value"
    );
    h.host(&remote()).send(DaemonMessage::AgentSettings {
        settings: std::collections::BTreeMap::from([(
            AgentId("claude".to_owned()),
            std::collections::BTreeMap::from([(
                "permission_mode".to_owned(),
                OptionValue::Text("accept_edits".to_owned()),
            )]),
        )]),
    });
    assert_eq!(
        permission_mode(&mut h),
        local_before,
        "box's echo leaves This PC's page alone"
    );
    h.click_on(&remote_row());
    assert_eq!(
        permission_mode(&mut h).as_deref(),
        Some("Accept edits"),
        "box's page shows box's echo"
    );
}

#[gpui::test]
fn a_launch_command_edit_on_the_chosen_host_goes_to_that_host_only(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir);
    h.keys("ctrl-,");
    h.click_on(&remote_row());
    h.click_on("settings-tab-claude");
    assert_eq!(
        launch_program(&mut h).as_deref(),
        Some("box-claude"),
        "box's page holds box's launch command"
    );

    h.click_on("settings-agent-program");
    h.keys("ctrl-a");
    type_line(&mut h, "wrapper");
    h.keys("enter");

    assert_eq!(
        command_sets(&h.host(&remote()).sent()),
        [("claude".to_owned(), "wrapper".to_owned())],
        "the command goes to box"
    );
    let local_sent = command_sets(&h.sent());
    assert!(
        local_sent.is_empty(),
        "nothing goes to This PC: {local_sent:?}"
    );
}

#[gpui::test]
fn a_draft_on_this_pcs_page_survives_choosing_another_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");
    h.click_on("settings-agent-program");
    h.keys("ctrl-a");
    type_line(&mut h, "mine");

    h.click_on(&remote_row());
    assert_eq!(
        launch_program(&mut h).as_deref(),
        Some("box-claude"),
        "box's page has its own draft"
    );
    h.click_on(LOCAL_ROW);
    assert_eq!(
        launch_program(&mut h).as_deref(),
        Some("mine"),
        "This PC's draft is still there"
    );
    assert!(
        command_sets(&h.sent()).is_empty() && command_sets(&h.host(&remote()).sent()).is_empty(),
        "moving the selector sends no draft"
    );
}

#[gpui::test]
fn settings_reopens_on_this_pc(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir);
    h.keys("ctrl-,");
    h.click_on(&remote_row());
    assert_eq!(shown_host(&mut h), remote_key());
    h.click_on("settings-close");
    h.keys("ctrl-,");
    assert_eq!(shown_host(&mut h), HostKey::Local);
}

#[gpui::test]
fn the_chosen_host_disconnecting_returns_the_selector_to_this_pc(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir);
    h.keys("ctrl-,");
    h.click_on(&remote_row());
    h.click_on("settings-tab-claude");
    h.click_on("settings-agent-program");
    type_line(&mut h, "draft");

    h.host(&remote())
        .event(NetEvent::State(Connection::for_target(remote())));

    assert_eq!(
        shown_host(&mut h),
        HostKey::Local,
        "the selector falls back"
    );
    assert_eq!(shown_tab(&mut h), "Claude", "This PC lists Claude too");
    assert!(
        drawn(&mut h, "settings-agent-program") && drawn(&mut h, "settings-agent-args"),
        "This PC's Claude page draws its launch rows"
    );
    assert_eq!(
        launch_program(&mut h).as_deref(),
        Some("local-claude"),
        "This PC's page holds This PC's launch command"
    );
    assert_tab_reaches_only_drawn_controls(&mut h);
    assert_eq!(
        h.root(|root, _| root.settings_host_choices()),
        [(HostKey::Local, true), (remote_key(), false)],
        "the offline host is listed, not choosable"
    );
    h.click_on(&remote_row());
    assert_eq!(
        shown_host(&mut h),
        HostKey::Local,
        "a click on the offline host changes nothing"
    );

    let mut open = Connection::for_target(remote());
    open.on_connecting();
    open.on_socket_open(8787);
    open.on_welcome(PROTOCOL, daemon_features());
    h.host(&remote()).event(NetEvent::State(open));
    h.click_on(&remote_row());
    assert_eq!(shown_host(&mut h), remote_key());
    assert_eq!(
        launch_program(&mut h),
        None,
        "box's drafts went with its connection"
    );
}

/// Walks Tab from where the keyboard is round the shown page's ring to the
/// tab list, checking every control it reaches is drawn.
fn assert_tab_reaches_only_drawn_controls(h: &mut Harness<'_>) {
    for selector in tab_ring(h) {
        assert!(drawn(h, selector), "Tab reached undrawn {selector}");
    }
}

/// The controls Tab reaches from where the keyboard is round the shown
/// page's ring, until it returns to the tab list.
fn tab_ring(h: &mut Harness<'_>) -> Vec<&'static str> {
    let mut reached = Vec::new();
    for _ in 0..40 {
        h.keys("tab");
        let root = h.root.clone();
        let focus =
            h.cx.update(|window, cx| root.read(cx).settings_focus(window, cx));
        assert!(
            focus.is_some(),
            "Tab left the Settings frame after {reached:?}"
        );
        if focus == Some("list") {
            break;
        }
        reached.extend(focus);
    }
    assert!(
        reached.len() < 40,
        "Tab never returned to the tab list: {reached:?}"
    );
    reached
}

#[gpui::test]
fn a_remote_agent_list_keeps_this_pcs_agent_page_while_this_pc_is_down(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-claude");

    h.event(NetEvent::State(Connection::for_target(Target::Local)));
    assert!(
        shown_tab(&mut h).eq_ignore_ascii_case("claude"),
        "a lost connection keeps the page"
    );
    h.host(&remote()).send(DaemonMessage::Agents {
        agents: agent_registry::descriptors(),
    });

    assert!(
        shown_tab(&mut h).eq_ignore_ascii_case("claude"),
        "box's agent list says nothing about This PC's pages"
    );
}

/// The worktrees root each host's daemon reports: This PC's and box's
/// overrides.
fn two_roots(h: &mut Harness<'_>) {
    h.send(root_changed(LOCAL_ROOT));
    h.host(&remote()).send(root_changed(BOX_ROOT));
}

const LOCAL_ROOT: &str = "C:\\local-wt";
const BOX_ROOT: &str = "/home/me/wt";

fn root_changed(root: &str) -> DaemonMessage {
    DaemonMessage::WorktreesRootChanged {
        root: root.to_owned(),
        is_override: true,
    }
}

fn worktrees_path(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.worktrees_path())
}

/// The `path` of every `SetWorktreesRoot` among `sent`.
fn root_sets(sent: &[ClientMessage]) -> Vec<Option<String>> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetWorktreesRoot { path } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

/// Settings open on the Worktrees page, both roots reported.
fn worktrees_page<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = two_hosts(cx, dir);
    two_roots(&mut h);
    h.keys("ctrl-,");
    h.click_on("settings-tab-worktrees");
    h
}

#[gpui::test]
fn the_worktrees_page_shows_and_saves_the_chosen_hosts_root(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = worktrees_page(cx, &dir);
    assert_eq!(worktrees_path(&mut h).as_deref(), Some(LOCAL_ROOT));

    h.click_on(&remote_row());
    assert_eq!(
        worktrees_path(&mut h).as_deref(),
        Some(BOX_ROOT),
        "box's page holds box's root"
    );
    assert!(
        h.root(|root, _| root.worktrees_active_line())
            .contains(BOX_ROOT),
        "the root in use is box's"
    );
    h.click_on("settings-worktrees-root-input");
    h.keys("ctrl-a");
    type_line(&mut h, "boxroot");
    h.click_on("settings-worktrees-root-save");

    assert_eq!(
        root_sets(&h.host(&remote()).sent()),
        [Some("boxroot".to_owned())],
        "Save goes to box"
    );
    let local_sent = root_sets(&h.sent());
    assert!(
        local_sent.is_empty(),
        "nothing goes to This PC: {local_sent:?}"
    );

    h.send(root_changed("C:\\other"));
    assert_eq!(
        h.root(|root, _| root.worktrees_save_button()),
        ("Save", true),
        "This PC's echo does not answer box's save"
    );
    h.host(&remote()).send(root_changed("boxroot"));
    assert_eq!(
        h.root(|root, _| root.worktrees_save_button()),
        ("Saved", false),
        "box's echo answers box's save"
    );
}

#[gpui::test]
fn a_worktrees_draft_on_this_pc_survives_choosing_another_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = worktrees_page(cx, &dir);
    h.click_on("settings-worktrees-root-input");
    h.keys("ctrl-a");
    type_line(&mut h, "mine");

    h.click_on(&remote_row());
    assert_eq!(worktrees_path(&mut h).as_deref(), Some(BOX_ROOT));
    h.click_on(LOCAL_ROW);
    assert_eq!(
        worktrees_path(&mut h).as_deref(),
        Some("mine"),
        "This PC's draft is still there"
    );
    assert!(
        root_sets(&h.sent()).is_empty() && root_sets(&h.host(&remote()).sent()).is_empty(),
        "moving the selector sends no draft"
    );
}

#[gpui::test]
fn the_worktrees_page_on_a_remote_host_takes_a_typed_path(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = worktrees_page(cx, &dir);
    assert!(
        drawn(&mut h, "settings-worktrees-root-browse")
            && tab_ring(&mut h).contains(&"settings-worktrees-root-browse"),
        "This PC's page browses"
    );
    assert_eq!(h.root(|root, _| root.worktrees_host_note()), None);

    h.click_on(&remote_row());
    let ring = tab_ring(&mut h);
    assert!(
        !ring.contains(&"settings-worktrees-root-browse"),
        "box's page offers no Browse: {ring:?}"
    );
    assert_eq!(
        h.root(|root, _| root.worktrees_host_note()).as_deref(),
        Some("Created on box if it does not exist.")
    );
    assert!(drawn(
        &mut h,
        "settings-worktrees-host-hint=Created on box if it does not exist."
    ));
    assert_tab_reaches_only_drawn_controls(&mut h);
}

#[gpui::test]
fn the_chosen_host_disconnecting_drops_its_worktrees_draft(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = worktrees_page(cx, &dir);
    h.click_on(&remote_row());
    h.click_on("settings-worktrees-root-input");
    h.keys("ctrl-a");
    type_line(&mut h, "draft");

    h.host(&remote())
        .event(NetEvent::State(Connection::for_target(remote())));
    assert_eq!(shown_host(&mut h), HostKey::Local);
    assert_eq!(
        worktrees_path(&mut h).as_deref(),
        Some(LOCAL_ROOT),
        "This PC's page shows"
    );

    let mut open = Connection::for_target(remote());
    open.on_connecting();
    open.on_socket_open(8787);
    open.on_welcome(PROTOCOL, daemon_features());
    h.host(&remote()).event(NetEvent::State(open));
    h.host(&remote()).send(root_changed(BOX_ROOT));
    h.click_on(&remote_row());
    assert_eq!(
        worktrees_path(&mut h).as_deref(),
        Some(BOX_ROOT),
        "box's draft went with its connection"
    );
}

#[gpui::test]
fn remote_access_is_not_offered_on_a_remote_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir);
    h.keys("ctrl-,");
    h.click_on("settings-tab-remote-access");
    assert_eq!(shown_tab(&mut h), "Remote access");

    h.click_on(&remote_row());
    assert_eq!(
        shown_tab(&mut h),
        "General",
        "the page gives way to General"
    );
    assert!(
        !h.root(|root, _| root.settings_tab_rows())
            .contains(&"Remote access"),
        "box lists no Remote access"
    );
    h.click_on(LOCAL_ROW);
    assert!(
        h.root(|root, _| root.settings_tab_rows())
            .contains(&"Remote access"),
        "This PC lists it again"
    );
}

fn keep_awake(mode: KeepAwakeMode) -> DaemonMessage {
    DaemonMessage::KeepAwakeStatus {
        enabled: mode != KeepAwakeMode::Off,
        mode,
        active: false,
    }
}

/// The mode of every `SetKeepAwakeMode` among `sent`.
fn keep_awake_sets(sent: &[ClientMessage]) -> Vec<KeepAwakeMode> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetKeepAwakeMode { mode } => Some(*mode),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn keep_awake_follows_the_chosen_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir);
    h.send(keep_awake(KeepAwakeMode::Always));
    h.host(&remote()).send(keep_awake(KeepAwakeMode::Off));
    h.keys("ctrl-,");
    let mode = |h: &mut Harness<'_>| h.root(|root, _| root.keep_awake_mode());
    assert_eq!(mode(&mut h), Some(KeepAwakeMode::Always));

    h.click_on(&remote_row());
    assert_eq!(mode(&mut h), Some(KeepAwakeMode::Off), "box's mode shows");
    h.click_on("settings-keep-awake-when-busy");
    assert_eq!(
        keep_awake_sets(&h.host(&remote()).sent()),
        [KeepAwakeMode::WhenBusy],
        "the choice goes to box"
    );
    let local_sent = keep_awake_sets(&h.sent());
    assert!(
        local_sent.is_empty(),
        "nothing goes to This PC: {local_sent:?}"
    );

    h.click_on(LOCAL_ROW);
    assert_eq!(
        mode(&mut h),
        Some(KeepAwakeMode::Always),
        "This PC's mode is untouched"
    );
}

#[gpui::test]
fn one_host_held_draws_no_selector(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    h.keys("ctrl-,");
    assert_eq!(
        h.root(|root, _| root.settings_host_choices()),
        Vec::new(),
        "one host held offers no choice"
    );
    assert!(!drawn(&mut h, "settings-host-row"), "no selector row");

    let mut host = h.host(&remote());
    host.connect();
    assert!(drawn(&mut h, "settings-host-row"), "a second host adds it");
}
