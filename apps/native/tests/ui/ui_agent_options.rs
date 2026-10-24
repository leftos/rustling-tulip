//! Agent options specs: the daemon's agent descriptors and each agent's
//! Settings values, as the client keeps them, and the spawn dialog's agent
//! rows drawn from them: Runtime lists the daemon's agents then Plain shell,
//! an agent's Main fields sit under Runtime and its Advanced fields in
//! Advanced, and each field's value reaches the spawn.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::collections::BTreeMap;

use gpui::{TestAppContext, px};
use protocol::{
    AgentDescriptor, AgentId, ClientMessage, DaemonMessage, FieldKind, OptionField, OptionValue,
    OptionValues, SessionMode, SpawnRequest, SuggestTarget,
};
use support::{Fixture, Harness, TestDir, repo, session};

#[gpui::test]
fn the_client_keeps_agents_and_settings_until_the_connection_drops(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    assert_eq!(
        h.root(|root, _| root.agents().to_vec()),
        agent_registry::descriptors(),
        "the descriptors the daemon sends after Welcome"
    );
    assert!(
        h.root(|root, _| root.agent_settings().is_empty()),
        "no agent has Settings values yet"
    );

    let settings = BTreeMap::from([(
        AgentId("claude".to_owned()),
        OptionValues::from([(
            "provider".to_owned(),
            OptionValue::Text("deepseek".to_owned()),
        )]),
    )]);
    h.send(DaemonMessage::AgentSettings {
        settings: settings.clone(),
    });
    assert_eq!(
        h.root(|root, _| root.agent_settings().clone()),
        settings,
        "a Settings broadcast replaces the values"
    );

    h.lose_connection();
    assert!(
        h.root(|root, _| root.agents().is_empty()),
        "a dropped connection forgets the agents"
    );
    assert!(
        h.root(|root, _| root.agent_settings().is_empty()),
        "a dropped connection forgets the Settings values"
    );
}

fn claude() -> AgentDescriptor {
    agent_registry::descriptors()
        .into_iter()
        .find(|descriptor| descriptor.id == AgentId("claude".to_owned()))
        .expect("the registry has Claude")
}

fn test_agent() -> AgentDescriptor {
    agent_api::test_agent::DESCRIPTOR.clone()
}

/// An agent whose one Main field is of a kind this client does not know.
fn future_agent() -> AgentDescriptor {
    let mut descriptor = test_agent();
    descriptor.id = AgentId("future-agent".to_owned());
    "future".clone_into(&mut descriptor.tag);
    let template = descriptor.fields.first().cloned().expect("a first field");
    descriptor.fields = vec![OptionField {
        id: "future".to_owned(),
        label: "Future".to_owned(),
        kind: FieldKind::Unknown,
        default: OptionValue::Null,
        when: Vec::new(),
        ..template
    }];
    descriptor
}

/// Repo `r1` with session `s1` in the focused pane, the daemon listing
/// `agents`, and the window tall enough to show the whole dialog.
fn harness<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    agents: Vec<AgentDescriptor>,
) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "C:/r1")];
    let mut h = Harness::with(cx, dir, &fixture);
    h.send(DaemonMessage::Agents { agents });
    h.cx.simulate_resize(gpui::size(px(1000.0), px(1400.0)));
    h
}

/// Opens the dialog with "+ Session" and gives it a branch name, so Spawn
/// can go.
fn open(h: &mut Harness<'_>) {
    h.click_on("sidebar-add-session");
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the dialog opened"
    );
    h.sent();
    h.send(DaemonMessage::BranchNameSuggestion {
        target: SuggestTarget::Repo {
            repo_id: "r1".to_owned(),
        },
        name: "wt/brave-fox".to_owned(),
        request_id: None,
    });
}

fn selected(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.spawn_dialog_selected())
}

fn focus(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.spawn_dialog_focus())
}

fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.height > px(0.0)
}

/// Every control Tab reaches, once round the ring from the focused one.
fn ring(h: &mut Harness<'_>) -> Vec<String> {
    let first = focus(h).expect("a focus");
    let mut seen = vec![first.clone()];
    for _ in 0..60 {
        h.keys("tab");
        let now = focus(h).expect("a focus");
        if now == first {
            return seen;
        }
        seen.push(now);
    }
    panic!("Tab never came back to {first}: {seen:?}");
}

/// Tabs until `selector` has the focus.
fn tab_to(h: &mut Harness<'_>, selector: &str) {
    for _ in 0..60 {
        if focus(h).as_deref() == Some(selector) {
            return;
        }
        h.keys("tab");
    }
    panic!("Tab never reached {selector}");
}

/// Submits the dialog and returns the one spawn it sent.
fn submit(h: &mut Harness<'_>) -> SpawnRequest {
    h.click_on("spawn-submit");
    let spawns: Vec<SpawnRequest> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::SpawnSession(request) => Some(request),
            _ => None,
        })
        .collect();
    match spawns.as_slice() {
        [request] => request.clone(),
        other => panic!("expected one SpawnSession, sent {other:?}"),
    }
}

fn options_of(request: &SpawnRequest) -> OptionValues {
    request
        .agent
        .as_ref()
        .map(|selection| selection.options.clone())
        .expect("an agent spawn")
}

fn values(pairs: &[(&str, OptionValue)]) -> OptionValues {
    pairs
        .iter()
        .map(|(id, value)| ((*id).to_owned(), value.clone()))
        .collect()
}

fn text(value: &str) -> OptionValue {
    OptionValue::Text(value.to_owned())
}

#[gpui::test]
fn the_runtime_row_lists_the_daemons_agents_then_plain_shell(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, vec![claude(), test_agent()]);
    open(&mut h);
    let left = |h: &mut Harness<'_>, selector: &str| h.bounds(selector).left();
    let claude_at = left(&mut h, "spawn-agent-claude");
    let test_at = left(&mut h, "spawn-agent-test-agent");
    let shell_at = left(&mut h, "spawn-agent-plain-shell");
    assert!(
        claude_at < test_at && test_at < shell_at,
        "the daemon's order, then Plain shell: {claude_at:?} {test_at:?} {shell_at:?}"
    );
    assert!(
        selected(&mut h).contains(&"spawn-agent-claude".to_owned()),
        "the first agent is the default"
    );
}

#[gpui::test]
fn main_fields_draw_under_runtime_and_advanced_fields_in_advanced(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, vec![claude(), test_agent()]);
    open(&mut h);
    let top = |h: &mut Harness<'_>, selector: &str| h.bounds(selector).top();
    assert!(drawn(&mut h, "spawn-opt-provider-anthropic"));
    let provider = top(&mut h, "spawn-opt-provider-anthropic");
    assert!(top(&mut h, "spawn-agent-claude") < provider);
    assert!(provider < top(&mut h, "spawn-placement-current-tab"));
    assert!(
        !drawn(&mut h, "spawn-opt-permission-mode-plan") && !drawn(&mut h, "spawn-opt-model"),
        "collapsed Advanced keeps its fields hidden"
    );

    h.click_on("spawn-advanced");
    for selector in [
        "spawn-opt-permission-mode-cli-default",
        "spawn-opt-permission-mode-plan",
        "spawn-opt-model",
        "spawn-opt-model-sonnet",
    ] {
        assert!(drawn(&mut h, selector), "{selector} in Advanced");
        assert!(top(&mut h, "spawn-advanced") < top(&mut h, selector));
        assert!(
            top(&mut h, selector) < top(&mut h, "spawn-env-add"),
            "{selector} above the env rows"
        );
    }
    assert!(
        top(&mut h, "spawn-opt-permission-mode-plan") < top(&mut h, "spawn-opt-model"),
        "Advanced fields in descriptor order"
    );
    assert!(
        selected(&mut h).contains(&"spawn-opt-permission-mode-cli-default".to_owned()),
        "a choice with no default starts on CLI default"
    );
}

#[gpui::test]
fn a_toggle_choice_and_text_field_send_their_values(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, vec![claude(), test_agent()]);
    open(&mut h);
    h.click_on("spawn-agent-test-agent");
    assert!(selected(&mut h).contains(&"spawn-opt-mode-fast".to_owned()));
    h.click_on("spawn-opt-mode-slow");
    h.click_on("spawn-opt-sandbox");
    assert!(
        selected(&mut h).contains(&"spawn-opt-sandbox".to_owned()),
        "the toggle is on"
    );
    h.click_on("spawn-opt-sandbox");
    assert!(!selected(&mut h).contains(&"spawn-opt-sandbox".to_owned()));
    tab_to(&mut h, "spawn-opt-profile");
    h.keys("space w o r k space");
    assert_eq!(
        h.root(|root, cx| root.spawn_dialog_option_text("profile", cx))
            .as_deref(),
        Some(" work "),
        "the field keeps the typed text"
    );
    let request = submit(&mut h);
    assert_eq!(
        request
            .agent
            .as_ref()
            .map(|selection| selection.agent.clone()),
        Some(AgentId("test-agent".to_owned()))
    );
    assert_eq!(
        options_of(&request),
        values(&[("mode", text("slow")), ("profile", text("work"))]),
        "the choice and the trimmed text; the toggle at its default is not sent"
    );
}

#[gpui::test]
fn a_when_rule_hides_a_field_and_drops_its_value(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, vec![test_agent()]);
    open(&mut h);
    tab_to(&mut h, "spawn-opt-profile");
    h.keys("w o r k");
    h.click_on("spawn-opt-sandbox");
    assert!(
        !ring(&mut h).contains(&"spawn-opt-profile".to_owned()),
        "the sandbox rule hides profile"
    );
    let request = submit(&mut h);
    assert_eq!(
        options_of(&request),
        values(&[("sandbox", OptionValue::Bool(true))]),
        "a hidden field's value is not sent"
    );
}

#[gpui::test]
fn trusted_launch_locks_the_locked_fields_and_sends_none(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, vec![claude()]);
    open(&mut h);
    h.click_on("spawn-advanced");
    h.click_on("spawn-opt-permission-mode-plan");
    assert!(selected(&mut h).contains(&"spawn-opt-permission-mode-plan".to_owned()));
    h.click_on("spawn-skip-perms");
    assert!(
        !ring(&mut h)
            .iter()
            .any(|control| control.starts_with("spawn-opt-permission-mode-")),
        "a locked field leaves the focus ring"
    );
    h.click_on("spawn-opt-permission-mode-default");
    let request = submit(&mut h);
    assert!(request.dangerously_skip_permissions);
    assert_eq!(
        options_of(&request),
        OptionValues::new(),
        "a trusted launch sends no permission mode"
    );
}

#[gpui::test]
fn an_unknown_field_kind_is_a_disabled_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, vec![future_agent()]);
    open(&mut h);
    assert!(selected(&mut h).contains(&"spawn-agent-future-agent".to_owned()));
    assert!(drawn(&mut h, "spawn-opt-future"), "the row is drawn");
    assert!(
        !ring(&mut h).contains(&"spawn-opt-future".to_owned()),
        "a field this client cannot edit takes no focus"
    );
    h.click_on("spawn-opt-future");
    let request = submit(&mut h);
    assert_eq!(options_of(&request), OptionValues::new());
}

#[gpui::test]
fn switching_runtime_keeps_each_agents_values(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, vec![claude(), test_agent()]);
    open(&mut h);
    h.click_on("spawn-opt-provider-deepseek");
    h.click_on("spawn-agent-test-agent");
    assert!(
        !selected(&mut h)
            .iter()
            .any(|chosen| chosen.starts_with("spawn-opt-provider-")),
        "the test agent has no provider"
    );
    h.click_on("spawn-opt-mode-slow");
    h.click_on("spawn-agent-plain-shell");
    h.click_on("spawn-agent-claude");
    assert!(selected(&mut h).contains(&"spawn-opt-provider-deepseek".to_owned()));
    h.click_on("spawn-agent-test-agent");
    assert!(selected(&mut h).contains(&"spawn-opt-mode-slow".to_owned()));
    let request = submit(&mut h);
    assert_eq!(options_of(&request), values(&[("mode", text("slow"))]));
}

#[gpui::test]
fn the_trust_note_is_the_descriptors(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, vec![claude(), test_agent()]);
    open(&mut h);
    assert_eq!(
        h.root(|root, _| root.spawn_dialog_trust_note()),
        claude().trust_note
    );
    h.click_on("spawn-agent-test-agent");
    assert_eq!(
        h.root(|root, _| root.spawn_dialog_trust_note()),
        None,
        "an agent with no trust note shows no detail"
    );
}

#[gpui::test]
fn a_locked_text_field_takes_no_typing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut agent = test_agent();
    for field in &mut agent.fields {
        if field.id == "profile" {
            field.locked_when_trusted = true;
        }
    }
    let mut h = harness(cx, &dir, vec![agent]);
    open(&mut h);
    tab_to(&mut h, "spawn-opt-profile");
    h.keys("w o r k");
    h.click_on("spawn-skip-perms");
    assert!(
        !ring(&mut h).contains(&"spawn-opt-profile".to_owned()),
        "a locked field takes no focus"
    );
    h.click_on("spawn-opt-profile");
    h.keys("x y");
    assert_eq!(
        h.root(|root, cx| root.spawn_dialog_option_text("profile", cx))
            .as_deref(),
        Some("work"),
        "a locked field takes no typing"
    );
    h.click_on("spawn-skip-perms");
    let request = submit(&mut h);
    assert_eq!(options_of(&request), values(&[("profile", text("work"))]));
}

#[gpui::test]
fn a_last_spawn_advanced_value_opens_advanced(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    let mut r1 = repo("r1", "C:/r1");
    r1.last_spawn_config = Some(
        serde_json::from_value(serde_json::json!({
            "target": { "kind": "single", "repo_id": "r1", "branch_name": "wt/old",
                "base_branch": null, "use_worktree": true },
            "mode": "interactive",
            "dangerously_skip_permissions": false,
            "agent": { "agent": "claude", "options": { "permission_mode": "bypass_permissions" } },
        }))
        .expect("spawn config fixture"),
    );
    fixture.repos = vec![r1];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.cx.simulate_resize(gpui::size(px(1000.0), px(1400.0)));
    open(&mut h);
    assert!(
        drawn(&mut h, "spawn-opt-permission-mode-bypass-permissions"),
        "Advanced opens on the last spawn's permission mode"
    );
    assert!(selected(&mut h).contains(&"spawn-opt-permission-mode-bypass-permissions".to_owned()));
}

#[gpui::test]
fn no_agents_offers_plain_shell_only(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = harness(cx, &dir, Vec::new());
    open(&mut h);
    assert!(selected(&mut h).contains(&"spawn-agent-plain-shell".to_owned()));
    assert!(drawn(&mut h, "spawn-agent-plain-shell"));
    assert!(!drawn(&mut h, "spawn-agent-claude"), "no hardcoded Claude");
    let request = submit(&mut h);
    assert_eq!(request.mode, SessionMode::PlainShell);
    assert_eq!(request.agent, None);
}
