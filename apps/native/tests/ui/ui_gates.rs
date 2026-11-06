//! Gates specs: the rail item that shows only while This PC's daemon pushes
//! a gate state, its badge counting the busy and held slots, the panel its
//! click shows, the Sessions panel a stored Gates falls back to with no
//! state, the persisted choice, and a remote host's state left out. Then
//! the Gates view: its slot, waiting, critical and queue rows, the count
//! stepper and its reply and refusal, the row menu and the log a click
//! opens, and the footer strip.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use std::path::PathBuf;
use std::time::Duration;

use crate::support;

use gpui::{TestAppContext, px};
use protocol::{ClientMessage, DaemonMessage, GateSlotKind};
use rustling_tulip_native::Activity;
use serde_json::json;
use support::{
    Fixture, Harness, Opened, TestDir, gate_history_msg, gate_history_runs, gate_history_summary,
    gate_state, gate_state_full, gate_state_msg, saved_remote, saved_remote_over, saved_ui,
};

fn activity(h: &mut Harness<'_>) -> Activity {
    h.root(|root, _| root.activity())
}

fn shown(h: &mut Harness<'_>) -> Activity {
    h.root(|root, _| root.shown_activity())
}

/// Whether the element tagged `selector` was drawn at some point: gpui
/// keeps an element's bounds after it leaves, so only a selector never
/// drawn reads as absent.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
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

/// This PC pushed [`gate_state_full`], its busy heavy slot logging to a
/// file in `dir`, and the Gates panel shows; returns the log's path.
fn full(h: &mut Harness<'_>, dir: &TestDir) -> PathBuf {
    let log = dir.path().join("gate.log");
    std::fs::write(&log, "done\n").expect("write the gate's log");
    h.reconnect_with_features(&[protocol::FEATURE_GATE_STATE]);
    h.send(gate_state_msg(Some(gate_state_full("BOX", &log))));
    h.click_on("activity-gates");
    log
}

/// The `SetGateSlotCount` messages the client sent since the last read.
fn steps_sent(h: &mut Harness<'_>) -> Vec<(GateSlotKind, Option<u32>, Option<String>)> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetGateSlotCount {
                kind,
                count,
                request_id,
            } => Some((kind, count, request_id)),
            _ => None,
        })
        .collect()
}

fn pending(h: &mut Harness<'_>, kind: GateSlotKind) -> bool {
    h.root(|root, _| root.gates_step_pending(kind))
}

/// The request id of the one `SetGateSlotCount` in `sent`, for `kind` and
/// `count`.
fn one_step(
    sent: &[(GateSlotKind, Option<u32>, Option<String>)],
    kind: GateSlotKind,
    count: u32,
) -> String {
    assert!(
        matches!(sent, [(k, Some(c), Some(_))] if *k == kind && *c == count),
        "one SetGateSlotCount for {kind:?} {count} with an id: {sent:?}"
    );
    sent.first()
        .and_then(|(_, _, id)| id.clone())
        .expect("the step carries a request id")
}

#[gpui::test]
fn a_busy_row_shows_its_slot_command_time_folder_and_last_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    full(&mut h, &dir);
    assert!(drawn(&mut h, "gates-cores=16 cores"));
    assert!(drawn(&mut h, "gates-slot-heavy-0-name=heavy 0"));
    assert!(drawn(
        &mut h,
        "gates-slot-heavy-0-command=cargo nextest run"
    ));
    assert!(drawn(&mut h, "gates-slot-heavy-0-time=5m"));
    assert!(drawn(
        &mut h,
        "gates-slot-heavy-0-detail=repo.wt/rt-7 · Compiling foo"
    ));
    assert!(drawn(&mut h, "gates-pool-heavy-count=2 of 4 busy"));
    assert_eq!(
        h.root(|root, _| root.gates_badge()).as_deref(),
        Some("3"),
        "busy plus held in heavy and light; the remote slot is left out"
    );
}

#[gpui::test]
fn a_busy_rows_second_line_starts_under_its_command(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    full(&mut h, &dir);
    let command = h.bounds("gates-slot-heavy-0-command=cargo nextest run");
    let detail = h.bounds("gates-slot-heavy-0-detail=repo.wt/rt-7 · Compiling foo");
    assert!(command.origin.x >= px(0.0) && detail.origin.x >= px(0.0));
    assert!(
        detail.origin.x >= command.origin.x - px(1.0)
            && detail.origin.x <= command.origin.x + px(1.0),
        "line two at {detail:?}, the command at {command:?}"
    );
}

#[gpui::test]
fn a_pool_headers_heading_count_and_waiting_sit_7_px_apart(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    full(&mut h, &dir);
    let parts = [
        "gates-pool-heavy-title",
        "gates-pool-heavy-count=2 of 4 busy",
        "gates-pool-heavy-dot",
        "gates-pool-heavy-waiting",
    ]
    .map(|selector| h.bounds(selector));
    for pair in parts.windows(2) {
        let (left, right) = (pair[0], pair[1]);
        assert!(left.origin.x >= px(0.0) && right.origin.x >= px(0.0));
        assert!(
            right.left() - left.right() >= px(6.99),
            "{right:?} sits less than 7 px right of {left:?}; parts {parts:?}"
        );
    }
}

#[gpui::test]
fn held_and_free_rows_render_with_a_stale_claims_pid(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    full(&mut h, &dir);
    assert!(drawn(&mut h, "gates-slot-heavy-1-state=held"));
    assert!(drawn(
        &mut h,
        "gates-slot-heavy-2-state=free · stale pid 4040"
    ));
    assert!(drawn(&mut h, "gates-slot-heavy-3-state=free"));
    assert!(drawn(&mut h, "gates-slot-remote-0-name=remote 0"));
}

#[gpui::test]
fn waiting_gates_show_under_their_pool_and_count_in_its_header(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    full(&mut h, &dir);
    assert!(drawn(&mut h, "gates-pool-heavy-waiting=1 waiting"));
    assert!(drawn(&mut h, "gates-wait-heavy-0-name=1st"));
    assert!(drawn(&mut h, "gates-wait-heavy-0-command=cargo clippy"));
    assert!(drawn(&mut h, "gates-wait-heavy-0-folder=dev/other"));
    assert!(drawn(&mut h, "gates-wait-heavy-0-time=2m"));
    assert!(
        !drawn(&mut h, "gates-pool-light-waiting"),
        "no waiting text on a pool nothing waits for"
    );
}

#[gpui::test]
fn the_queue_and_critical_blocks_show_only_with_something_in_them(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    pushed(&mut h);
    h.click_on("activity-gates");
    assert!(drawn(&mut h, "gates-pool-heavy"));
    assert!(!drawn(&mut h, "gates-queue"), "no tickets, no queue block");
    assert!(
        !drawn(&mut h, "gates-critical"),
        "no holder or waiter, no block"
    );

    let log = dir.path().join("gate.log");
    h.send(gate_state_msg(Some(gate_state_full("BOX", &log))));
    assert!(drawn(&mut h, "gates-queue"));
    assert!(drawn(&mut h, "gates-queue-0-command=cargo clippy"));
    assert!(drawn(&mut h, "gates-critical"));
    assert!(drawn(&mut h, "gates-critical-note=1 waiting"));
}

#[gpui::test]
fn plus_sends_the_next_count_and_waits_for_the_reply(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    pushed(&mut h);
    h.click_on("activity-gates");
    h.sent();
    h.click_on("gates-step-up-heavy");
    let id = one_step(&steps_sent(&mut h), GateSlotKind::Heavy, 4);
    assert!(pending(&mut h, GateSlotKind::Heavy));
    h.click_on("gates-step-up-heavy");
    assert_eq!(steps_sent(&mut h), [], "the stepper waits for the reply");

    h.send(DaemonMessage::GateSlotCountSet {
        kind: GateSlotKind::Heavy,
        count: Some(4),
        request_id: Some(id),
    });
    assert!(!pending(&mut h, GateSlotKind::Heavy));
    h.click_on("gates-step-up-heavy");
    assert_eq!(steps_sent(&mut h).len(), 1, "a press sends again");
}

#[gpui::test]
fn a_dropped_connection_ends_a_pending_step(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    pushed(&mut h);
    h.click_on("activity-gates");
    h.sent();
    h.click_on("gates-step-up-heavy");
    one_step(&steps_sent(&mut h), GateSlotKind::Heavy, 4);
    assert!(pending(&mut h, GateSlotKind::Heavy));

    h.lose_connection();
    pushed(&mut h);
    assert!(
        !pending(&mut h, GateSlotKind::Heavy),
        "the write went with the connection"
    );
    h.sent();
    h.click_on("gates-step-up-heavy");
    one_step(&steps_sent(&mut h), GateSlotKind::Heavy, 4);
}

#[gpui::test]
fn an_error_echoing_the_id_shows_under_the_pool_header(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    pushed(&mut h);
    h.click_on("activity-gates");
    h.sent();
    h.click_on("gates-step-down-light");
    let id = one_step(&steps_sent(&mut h), GateSlotKind::Light, 1);
    h.send(DaemonMessage::Error {
        code: Some(protocol::error_codes::GATE_WRITE_FAILED.to_owned()),
        message: "slot-counts.json is locked".to_owned(),
        request_id: Some(id),
    });
    assert!(drawn(
        &mut h,
        "gates-error-light=slot-counts.json is locked"
    ));
    assert!(!pending(&mut h, GateSlotKind::Light));
    assert!(
        h.root(|root, _| root.toasts().is_empty()),
        "shown under the pool, not as a toast"
    );
}

#[gpui::test]
fn the_stepper_sends_nothing_past_64_or_below_1(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let mut state = gate_state("BOX");
    state.pools[0].count = 64;
    state.pools[1].count = 1;
    h.reconnect_with_features(&[protocol::FEATURE_GATE_STATE]);
    h.send(gate_state_msg(Some(state)));
    h.click_on("activity-gates");
    h.sent();
    h.click_on("gates-step-up-heavy");
    h.click_on("gates-step-down-light");
    assert_eq!(steps_sent(&mut h), []);
    assert!(!pending(&mut h, GateSlotKind::Heavy));
}

#[gpui::test]
fn the_row_menu_copies_the_command_folder_and_log_path(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let log = full(&mut h, &dir);
    for (row, text) in [
        (
            "gates-menu-copy-command",
            r"C:\tools\cargo.exe nextest run".to_owned(),
        ),
        ("gates-menu-copy-folder", r"X:\dev\repo.wt\rt-7".to_owned()),
        ("gates-menu-copy-log", log.display().to_string()),
    ] {
        h.right_click_on("gates-slot-heavy-0");
        assert!(h.in_model("gates-menu"), "the busy row's menu opens");
        h.click_on(row);
        assert_eq!(h.clipboard(), Some(text), "{row}");
        assert!(!h.in_model("gates-menu"), "a copy closes the menu");
    }

    h.set_clipboard("before");
    h.right_click_on("gates-slot-light-0");
    h.click_on("gates-menu-copy-log");
    assert_eq!(
        h.clipboard().as_deref(),
        Some("before"),
        "no log path, the row is disabled"
    );

    h.click_on("gates-menu-backdrop");
    assert!(!h.in_model("gates-menu"), "a click outside closes it");
    h.right_click_on("gates-slot-heavy-3");
    assert!(!h.in_model("gates-menu"), "a free row has no menu");
}

#[gpui::test]
fn a_click_on_a_busy_row_opens_its_log(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let log = full(&mut h, &dir);
    h.click_on("gates-slot-heavy-0");
    assert_eq!(h.opened(), [Opened::DefaultApp(log.clone())]);
    h.click_on("gates-slot-light-0");
    assert_eq!(
        h.opened(),
        [Opened::DefaultApp(log)],
        "no log path, nothing more opens"
    );
}

#[gpui::test]
fn the_footer_strip_counts_slots_and_shows_the_gates_view(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    assert!(!drawn(&mut h, "footer-gates"), "no state, no strip");
    pushed(&mut h);
    assert!(drawn(&mut h, "footer-gates-text=Gates 3/5"));
    assert_eq!(shown(&mut h), Activity::Sessions);
    h.click_on("footer-gates");
    assert_eq!(shown(&mut h), Activity::Gates);
    assert!(h.in_model("gates-panel"));

    h.send(gate_state_msg(None));
    assert!(!h.in_model("footer-gates"));
}

/// The request ids of the `GateHistory` messages the client sent since the
/// last read.
fn history_sent(h: &mut Harness<'_>) -> Vec<Option<String>> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::GateHistory { request_id } => Some(request_id),
            _ => None,
        })
        .collect()
}

/// The id of the one `GateHistory` the client sent since the last read.
fn one_history(h: &mut Harness<'_>) -> String {
    let sent = history_sent(h);
    assert!(
        matches!(sent.as_slice(), [Some(_)]),
        "one GateHistory with an id: {sent:?}"
    );
    sent.into_iter()
        .flatten()
        .next()
        .expect("the request carries an id")
}

/// This PC pushed [`gate_state`] and the Gates view opened; what the client
/// sent before the open is drained.
fn opened(h: &mut Harness<'_>) {
    pushed(h);
    h.sent();
    h.click_on("activity-gates");
}

/// The reply to `id` with [`gate_history_runs`] and
/// [`gate_history_summary`].
fn answer(h: &mut Harness<'_>, id: String) {
    h.send(gate_history_msg(
        Some(id),
        gate_history_runs(),
        Vec::new(),
        gate_history_summary(),
    ));
}

fn recent_count(h: &mut Harness<'_>) -> Option<usize> {
    h.root(|root, _| root.gates_recent_count())
}

#[gpui::test]
fn opening_the_view_asks_for_the_history_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    pushed(&mut h);
    assert_eq!(history_sent(&mut h), [], "not while the view is hidden");
    h.click_on("activity-gates");
    one_history(&mut h);
}

#[gpui::test]
fn the_history_is_asked_again_every_15_s_while_the_view_shows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    opened(&mut h);
    let id = one_history(&mut h);
    answer(&mut h, id);
    h.advance(Duration::from_secs(14));
    assert_eq!(history_sent(&mut h), [], "not before 15 s");
    h.advance(Duration::from_secs(1));
    let id = one_history(&mut h);
    answer(&mut h, id);

    h.click_on("activity-gates");
    assert!(!h.in_model("gates-panel"), "the panel folded");
    h.advance(Duration::from_secs(30));
    assert_eq!(history_sent(&mut h), [], "folded, no request");

    h.click_on("activity-gates");
    let id = one_history(&mut h);
    answer(&mut h, id);
    h.click_on("activity-sessions");
    h.advance(Duration::from_secs(30));
    assert_eq!(history_sent(&mut h), [], "another panel, no request");
}

#[gpui::test]
fn no_second_request_goes_while_one_is_unanswered(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    opened(&mut h);
    one_history(&mut h);
    h.advance(Duration::from_secs(30));
    assert_eq!(history_sent(&mut h), []);
}

#[gpui::test]
fn a_reply_draws_the_recent_runs_and_the_week_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    opened(&mut h);
    let id = one_history(&mut h);
    assert!(!h.in_model("gates-recent"), "no block before an answer");
    answer(&mut h, id);
    assert!(h.in_model("gates-recent"));
    assert_eq!(recent_count(&mut h), Some(3), "the 3 h old run is out");
    assert!(drawn(
        &mut h,
        "gates-recent-0-command=cargo nextest run --workspace"
    ));
    assert!(drawn(&mut h, "gates-recent-0-result=✓"));
    assert!(drawn(&mut h, "gates-recent-0-held=1m 48s"));
    assert!(drawn(&mut h, "gates-recent-1-command=dotnet test"));
    assert!(drawn(&mut h, "gates-recent-1-result=✕"));
    assert!(drawn(
        &mut h,
        "gates-recent-1-detail=yaat · heavy #2 · failed · exit 1"
    ));
    assert!(drawn(
        &mut h,
        "gates-recent-2-detail=claude-config · heavy #1"
    ));
    assert!(drawn(&mut h, "gates-recent-2-fallback=fell back"));
    assert!(drawn(&mut h, "gates-recent-2-ago=30m ago"));
    assert!(
        !drawn(&mut h, "gates-recent-0-fallback=fell back"),
        "no tag on a run that did not fall back"
    );
    for n in 0..4 {
        assert!(h.in_model(&format!("gates-week-{n}")), "week row {n}");
        assert!(drawn(&mut h, &format!("gates-week-{n}-command=cmd-{n}")));
    }
    assert!(!h.in_model("gates-week-4"), "four rows at most");
    assert!(drawn(&mut h, "gates-week-0-runs=10 runs"));
    assert!(drawn(&mut h, "gates-week-0-p90=p90 1m 48s"));

    h.advance(Duration::from_secs(15));
    let id = one_history(&mut h);
    h.send(gate_history_msg(
        Some(id),
        gate_history_runs().split_off(3),
        Vec::new(),
        Vec::new(),
    ));
    assert_eq!(recent_count(&mut h), Some(0));
    assert!(h.in_model("gates-recent-empty"));
    assert!(drawn(&mut h, "gates-recent-empty=No runs in the last 2 h"));
    assert!(drawn(&mut h, "gates-week-empty=No passed runs in 7 days"));
}

#[gpui::test]
fn a_click_on_the_week_header_folds_and_unfolds_its_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    opened(&mut h);
    let id = one_history(&mut h);
    answer(&mut h, id);
    assert!(h.in_model("gates-week-0"), "open by default");
    h.click_on("gates-week-toggle");
    assert!(!h.root(|root, _| root.gates_week_open()));
    assert!(!h.in_model("gates-week-0"), "folded");
    h.click_on("gates-week-toggle");
    assert!(h.in_model("gates-week-0"), "unfolded");
}

#[gpui::test]
fn a_gone_gate_state_drops_the_history(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    opened(&mut h);
    let id = one_history(&mut h);
    answer(&mut h, id);
    assert_eq!(recent_count(&mut h), Some(3));

    h.send(gate_state_msg(None));
    assert_eq!(
        recent_count(&mut h),
        None,
        "the history went with the state"
    );
    h.send(gate_state_msg(Some(gate_state("BOX"))));
    assert!(h.in_model("gates-panel"), "the view is back");
    assert!(!h.in_model("gates-recent"), "no block until a new reply");
    let id = one_history(&mut h);
    answer(&mut h, id);
    assert!(h.in_model("gates-recent"));
}

#[gpui::test]
fn an_error_echoing_the_history_id_ends_the_request_quietly(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    opened(&mut h);
    let id = one_history(&mut h);
    answer(&mut h, id);
    h.advance(Duration::from_secs(15));
    let id = one_history(&mut h);
    h.send(DaemonMessage::Error {
        code: None,
        message: "the gate journal is unreadable".to_owned(),
        request_id: Some(id),
    });
    assert!(!h.root(|root, _| root.gates_history_pending()));
    assert!(
        h.root(|root, _| root.toasts().is_empty()),
        "no toast for a history refusal"
    );
    for kind in ["heavy", "light"] {
        assert!(
            !drawn(
                &mut h,
                &format!("gates-error-{kind}=the gate journal is unreadable")
            ),
            "no pool error under {kind}"
        );
    }
    assert_eq!(recent_count(&mut h), Some(3), "the earlier history stays");
    h.advance(Duration::from_secs(15));
    one_history(&mut h);
}

#[gpui::test]
fn another_hosts_gate_history_is_ignored(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &Fixture::default(), &[]);
    opened(&mut h);
    let id = one_history(&mut h);
    h.host(&saved_remote()).send(gate_history_msg(
        Some(id),
        gate_history_runs(),
        Vec::new(),
        gate_history_summary(),
    ));
    assert_eq!(recent_count(&mut h), None);
    assert!(h.root(|root, _| root.gates_history_pending()));
}

#[gpui::test]
fn a_reply_to_another_id_is_ignored(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    opened(&mut h);
    let id = one_history(&mut h);
    answer(&mut h, format!("{id}-other"));
    assert_eq!(recent_count(&mut h), None);
    assert!(h.root(|root, _| root.gates_history_pending()));
    answer(&mut h, id);
    assert_eq!(recent_count(&mut h), Some(3));
}
