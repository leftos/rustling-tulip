//! Gates specs: the rail item that shows only while This PC's daemon pushes
//! a gate state, its badge counting the busy and held slots, the panel its
//! click shows, the Sessions panel a stored Gates falls back to with no
//! state, the persisted choice, and a remote host's state left out.

use crate::support;

use gpui::TestAppContext;
use rustling_tulip_native::Activity;
use serde_json::json;
use support::{
    Fixture, Harness, TestDir, gate_state, gate_state_msg, saved_remote, saved_remote_over,
    saved_ui,
};

fn activity(h: &mut Harness<'_>) -> Activity {
    h.root(|root, _| root.activity())
}

fn shown(h: &mut Harness<'_>) -> Activity {
    h.root(|root, _| root.shown_activity())
}

fn badge(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.gates_badge())
}

/// This PC reconnected to a daemon listing the gate feature, which pushed
/// [`gate_state`] for `BOX`.
fn pushed(h: &mut Harness<'_>) {
    h.reconnect_with_features(&[protocol::FEATURE_GATE_STATE]);
    h.send(gate_state_msg(Some(gate_state("BOX"))));
}

#[gpui::test]
fn the_item_shows_only_while_this_pc_has_a_gate_state(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    assert!(h.in_model("activity-rail"));
    assert!(
        !h.in_model("activity-gates"),
        "no item before any GateState"
    );

    pushed(&mut h);
    assert!(h.in_model("activity-gates"), "a state shows the item");

    h.send(gate_state_msg(None));
    assert!(!h.in_model("activity-gates"), "no state, no item");
    assert_eq!(badge(&mut h), None);
}

#[gpui::test]
fn the_badge_counts_busy_and_held_slots(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    pushed(&mut h);
    assert_eq!(badge(&mut h).as_deref(), Some("3"), "two heavy, one light");
    assert!(h.in_model("gates-badge"));

    let mut idle = gate_state("BOX");
    for pool in &mut idle.pools {
        for slot in &mut pool.slots {
            slot.state = protocol::GateSlotState::Free;
            slot.claim = None;
        }
    }
    h.send(gate_state_msg(Some(idle)));
    assert!(h.in_model("activity-gates"), "the item stays");
    assert_eq!(badge(&mut h), None, "hidden at 0");
    assert!(!h.in_model("gates-badge"));
}

#[gpui::test]
fn a_click_shows_the_panel_under_the_machines_header(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    pushed(&mut h);
    h.click_on("activity-gates");
    assert_eq!(activity(&mut h), Activity::Gates);
    assert_eq!(shown(&mut h), Activity::Gates);
    assert!(h.in_model("gates-panel"));
    assert!(h.in_model("gates-header"));
    assert!(!h.in_model("sidebar-panel"), "the sessions panel gave way");
    assert_eq!(
        h.root(|root, _| root.gates_header()).as_deref(),
        Some("GATES · BOX")
    );
    assert_eq!(
        h.root(|root, _| root.gates_pool_lines()),
        ["HEAVY · 2 of 3 busy", "LIGHT · 1 of 2 busy"]
    );
    let rail = h.bounds("activity-rail");
    assert_eq!(h.bounds("gates-panel").origin.x, rail.right());
}

#[gpui::test]
fn a_stored_gates_with_the_state_gone_shows_sessions_and_keeps_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    pushed(&mut h);
    h.click_on("activity-gates");
    assert!(h.in_model("gates-panel"));

    h.send(gate_state_msg(None));
    assert_eq!(shown(&mut h), Activity::Sessions);
    assert!(h.in_model("sidebar-panel"), "the sessions panel shows");
    assert!(!h.in_model("gates-panel"));
    assert_eq!(activity(&mut h), Activity::Gates, "the stored value stays");
    assert_eq!(saved_ui(&dir)["activity"], json!("gates"));

    h.send(gate_state_msg(Some(gate_state("BOX"))));
    assert!(h.in_model("gates-panel"), "the state back, the panel back");
}

#[gpui::test]
fn activity_gates_persists_across_a_reopen(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::default();
    let mut h = Harness::with(cx, &dir, &fixture);
    pushed(&mut h);
    h.click_on("activity-gates");
    assert_eq!(saved_ui(&dir)["activity"], json!("gates"));
    drop(h);

    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(activity(&mut h), Activity::Gates, "restored");
    assert!(
        h.in_model("sidebar-panel"),
        "Sessions until a state arrives"
    );
    pushed(&mut h);
    assert!(h.in_model("gates-panel"));
    assert_eq!(saved_ui(&dir)["activity"], json!("gates"));
}

#[gpui::test]
fn a_remote_hosts_gate_state_shows_no_item(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &Fixture::default(), &[]);
    h.host(&saved_remote())
        .send(gate_state_msg(Some(gate_state("REMOTE-BOX"))));
    assert!(!h.in_model("activity-gates"));
    assert_eq!(badge(&mut h), None);
    assert_eq!(h.root(|root, _| root.gates_header()), None);
}
