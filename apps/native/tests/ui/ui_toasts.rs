//! The app's own toast specs: with `toast_style` set to `app`, a Needs you
//! or Finished toast is a window of the client's own, placed through the
//! recording placer, never an OS toast.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use chrono::{DateTime, TimeZone as _, Utc};
use gpui::{Modifiers, TestAppContext, VisualTestContext, point, px};
use protocol::{AttentionReason, ClientMessage, DaemonMessage, PromptChoice, SessionSnapshot};
use rustling_tulip_native::{BundledSound, ClipSource, Connection, NetEvent, PhysicalRect};
use serde_json::{Value, json};
use std::time::Duration;
use support::{Fixture, Harness, TestDir, pane, remote_lists, saved_remote, seed_ui, session, tab};

fn needs_you(id: &str) -> DaemonMessage {
    DaemonMessage::Attention {
        session_id: id.to_owned(),
        reason: AttentionReason::AwaitingInput,
    }
}

/// Delivers `id`'s turn and waits out its grace, raising its Finished toast.
fn finish(h: &mut Harness<'_>, id: &str) {
    for status in ["working", "idle"] {
        h.send(DaemonMessage::SessionUpdated {
            session: session(id).status(status).build(),
            request_id: None,
        });
    }
    h.advance(Duration::from_secs(5));
}

/// `s1` in `t1`'s pane and each of `others` in a tab of its own.
fn sessions(others: &[&str]) -> Fixture {
    let mut fixture = Fixture::single(session("s1").build());
    for (index, id) in others.iter().enumerate() {
        fixture.sessions.push(session(id).build());
        let pane_id = format!("p-{index}");
        fixture
            .tabs
            .push(tab(&format!("t-{index}"), &pane(&pane_id, Some(id))));
    }
    fixture
}

/// The harness over `fixture` with `notifications` saved, its own toasts on.
fn opened<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    fixture: &Fixture,
    mut notifications: Value,
) -> Harness<'a> {
    notifications["toast_style"] = json!("app");
    seed_ui(dir, &json!({ "notifications": notifications }));
    let mut h = Harness::with(cx, dir, fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    h
}

/// Presses `selector` in `toast`'s window.
fn press(toast: &mut VisualTestContext, selector: &'static str) {
    toast.run_until_parked();
    let bounds = toast
        .debug_bounds(selector)
        .expect("the control is drawn in the toast");
    toast.simulate_click(bounds.center(), Modifiers::none());
    toast.run_until_parked();
}

#[gpui::test]
fn app_style_opens_a_toast_window_and_no_os_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2"]), json!({}));

    h.send(needs_you("s2"));

    assert_eq!(h.window_count(), 2, "the toast has a window of its own");
    assert!(h.notified().is_empty(), "never the OS's toast beside it");
    assert_eq!(
        h.toasts_shown(),
        [PhysicalRect {
            x: 1544,
            y: 948,
            width: 360,
            height: 116,
        }],
        "in the work area's bottom-right corner, 16 px in"
    );
    let mut toast = h.popout_window();
    toast.run_until_parked();
    for part in ["toast-card", "toast-go-to-pane", "toast-dismiss"] {
        assert!(toast.debug_bounds(part).is_some(), "{part} is drawn");
    }
    assert!(
        !toast.update(|window, _| window.is_window_active()),
        "the toast never takes focus"
    );
}

#[gpui::test]
fn os_style_opens_no_toast_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &sessions(&["s2"]));
    h.answer_scrollback("s1", b"");

    h.send(needs_you("s2"));

    assert_eq!(h.window_count(), 1, "the OS draws this toast");
    assert_eq!(h.notified().len(), 1);
    assert_eq!(h.toasts_shown(), Vec::<PhysicalRect>::new());
}

#[gpui::test]
fn a_busy_user_gets_no_app_toast_and_no_sound(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2"]), json!({}));
    h.set_do_not_disturb(true);

    h.send(needs_you("s2"));

    assert_eq!(h.window_count(), 1, "no toast window opens");
    assert!(h.sounds().is_empty(), "and no sound plays");
    assert!(h.notified().is_empty(), "nor does the OS's toast stand in");
}

#[gpui::test]
fn the_app_toast_plays_the_chosen_sound(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let chosen = json!({
        "needs_you_sound": { "bundled": "bell_falling_fifth" },
        "finished_sound": "off",
    });
    let mut h = opened(cx, &dir, &sessions(&["s2", "s3"]), chosen);

    h.send(needs_you("s2"));
    finish(&mut h, "s3");

    assert_eq!(h.window_count(), 3, "both toasts are up");
    assert_eq!(
        h.sounds(),
        [ClipSource::Bundled(BundledSound::BellFallingFifth)],
        "Needs you plays its choice, and Finished's is off"
    );
}

#[gpui::test]
fn a_later_toast_for_a_session_replaces_it_in_place(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2"]), json!({}));
    h.send(needs_you("s2"));
    assert_eq!(
        h.root(|root, _| root.shown_toasts()),
        [("s2".to_owned(), "Needs you".to_owned())]
    );

    finish(&mut h, "s2");

    assert_eq!(h.window_count(), 2, "one window for the session");
    assert_eq!(h.toasts_shown().len(), 1, "that window was shown once");
    assert_eq!(
        h.root(|root, _| root.shown_toasts()),
        [("s2".to_owned(), "Finished".to_owned())],
        "it takes the later toast's words"
    );
}

#[gpui::test]
fn dismiss_closes_the_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2"]), json!({}));
    h.send(needs_you("s2"));
    let mut toast = h.popout_window();

    press(&mut toast, "toast-dismiss");

    assert_eq!(h.window_count(), 1, "the toast window closed");
    assert_eq!(
        h.root(|root, _| root.shown_toasts()),
        Vec::<(String, String)>::new()
    );
}

#[gpui::test]
fn go_to_pane_selects_the_session_and_closes_the_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2"]), json!({}));
    h.send(needs_you("s2"));
    assert_ne!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t-0"),
        "s2's tab starts behind"
    );
    let mut toast = h.popout_window();

    press(&mut toast, "toast-go-to-pane");

    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t-0"),
        "the session's tab is on screen"
    );
    assert_eq!(h.window_count(), 1, "the toast window closed");
    assert!(
        h.cx.update(|window, _| window.is_window_active()),
        "the main window is in front"
    );
}

#[gpui::test]
fn a_toast_expires_after_its_time(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2"]), json!({}));
    finish(&mut h, "s2");
    assert_eq!(h.window_count(), 2, "the Finished toast is up");

    h.advance(Duration::from_millis(5999));
    assert_eq!(h.window_count(), 2, "still up just before six seconds");

    h.advance(Duration::from_millis(1));
    assert_eq!(h.window_count(), 1, "gone at six seconds");
    assert_eq!(
        h.root(|root, _| root.shown_toasts()),
        Vec::<(String, String)>::new()
    );
}

#[gpui::test]
fn hover_holds_the_countdown(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2"]), json!({}));
    finish(&mut h, "s2");
    let mut toast = h.popout_window();
    toast.run_until_parked();
    let card = toast
        .debug_bounds("toast-card")
        .expect("the card is drawn in the toast");

    toast.simulate_mouse_move(card.center(), None, Modifiers::none());
    h.advance(Duration::from_secs(60));
    assert_eq!(h.window_count(), 2, "held while the pointer rests on it");

    toast.simulate_mouse_move(point(px(2000.0), px(2000.0)), None, Modifiers::none());
    h.advance(Duration::from_millis(5999));
    assert_eq!(h.window_count(), 2, "it had its six seconds left");
    h.advance(Duration::from_millis(1));
    assert_eq!(h.window_count(), 1, "the countdown ran on from the leave");
}

#[gpui::test]
fn a_fifth_toast_drops_the_oldest_finished(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = sessions(&["s2", "s3", "s4", "s5", "s6"]);
    let mut h = opened(cx, &dir, &fixture, json!({}));
    h.send(needs_you("s2"));
    finish(&mut h, "s3");
    for id in ["s4", "s5"] {
        h.send(needs_you(id));
    }
    assert_eq!(h.window_count(), 5, "four toasts are up");

    h.send(needs_you("s6"));

    assert_eq!(
        h.window_count(),
        5,
        "the fifth takes the Finished one's place"
    );
    let up: Vec<String> = h
        .root(|root, _| root.shown_toasts())
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(up, ["s2", "s4", "s5", "s6"], "s3 went, not the older s2");
    assert!(
        !h.toasts_restacked().is_empty(),
        "the rest moved to close the gap"
    );
}

#[gpui::test]
fn a_remote_session_shows_its_host_in_the_title(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "name": support::REMOTE, "port": 8787 }],
            "notifications": { "toast_style": "app" },
        }),
    );
    let mut h = Harness::with(cx, &dir, &sessions(&[]));
    h.host(&saved_remote()).connect();
    remote_lists(&mut h, &["b1"]);

    h.host(&saved_remote()).send(needs_you("b1"));
    h.send(needs_you("s1"));

    assert_eq!(
        h.root(|root, _| root.shown_toasts()),
        [
            ("b1".to_owned(), "Needs you · box".to_owned()),
            ("s1".to_owned(), "Needs you".to_owned()),
        ],
        "only the other host's toast names it"
    );
}

/// A fixed status stamp, `minute` minutes into the fixture's hour.
fn stamp(minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 9, minute, 0)
        .single()
        .expect("a valid stamp")
}

fn permission() -> Value {
    json!({ "kind": "permission", "tool_name": "Bash", "summary": "gh pr merge 118 --squash" })
}

fn question(options: &[&str]) -> Value {
    json!({
        "kind": "question",
        "questions": [{ "question": "Which branch?", "header": "", "options": options, "multi_select": false }],
    })
}

/// `id` waiting on `pending` since `since`.
fn waiting(id: &str, since: DateTime<Utc>, pending: Value) -> SessionSnapshot {
    session(id)
        .status("awaiting_input")
        .status_since(since)
        .pending_input(pending)
        .build()
}

/// `s1` in `t1`'s pane and `s2` behind it, waiting on `pending`.
fn asking(pending: Value) -> Fixture {
    let mut fixture = sessions(&["s2"]);
    fixture.sessions[1] = waiting("s2", stamp(1), pending);
    fixture
}

/// The harness with `s2`'s Needs you toast up on `pending`.
fn raised<'a>(cx: &'a mut TestAppContext, dir: &TestDir, pending: Value) -> Harness<'a> {
    let mut h = opened(cx, dir, &asking(pending), json!({}));
    h.send(needs_you("s2"));
    h
}

/// The harness with the remote host's `b1` up in a Needs you toast.
fn raised_on_box<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({
            "saved_hosts": [{ "name": support::REMOTE, "port": 8787 }],
            "notifications": { "toast_style": "app" },
        }),
    );
    let mut h = Harness::with(cx, dir, &sessions(&[]));
    h.host(&saved_remote()).connect();
    let listed = vec![waiting("b1", stamp(1), permission())];
    h.host(&saved_remote())
        .send(DaemonMessage::Sessions { sessions: listed });
    h.host(&saved_remote()).send(needs_you("b1"));
    h
}

fn drawn(toast: &mut VisualTestContext, selector: &'static str) -> bool {
    toast.run_until_parked();
    toast.debug_bounds(selector).is_some()
}

/// The `AnswerPrompt`s among `sent`, as (session, stamp, choice, request id).
fn answers(sent: Vec<ClientMessage>) -> Vec<(String, DateTime<Utc>, PromptChoice, Option<String>)> {
    sent.into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::AnswerPrompt {
                session_id,
                since,
                choice,
                request_id,
            } => Some((session_id, since, choice, request_id)),
            _ => None,
        })
        .collect()
}

fn no_toasts() -> Vec<(String, String)> {
    Vec::new()
}

#[gpui::test]
fn a_permission_toast_offers_allow_and_deny(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = raised(cx, &dir, permission());

    assert_eq!(h.toasts_shown()[0].height, 182, "room for the command");
    let mut toast = h.popout_window();
    for part in [
        "toast-command",
        "toast-answer-0",
        "toast-answer-1",
        "toast-go-to-pane",
    ] {
        assert!(drawn(&mut toast, part), "{part} is drawn");
    }
    assert!(
        !drawn(&mut toast, "toast-answer-2"),
        "with no chat loaded, no Always allow"
    );

    press(&mut toast, "toast-answer-1");
    let sent = answers(h.sent());
    assert_eq!(sent.len(), 1, "one answer: {sent:?}");
    assert_eq!(sent[0].2, PromptChoice::Deny);
}

#[gpui::test]
fn a_single_choice_question_offers_its_options(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = raised(cx, &dir, question(&["main", "dev", "next"]));

    assert_eq!(h.toasts_shown()[0].height, 186, "a second row of choices");
    let mut toast = h.popout_window();
    for part in ["toast-answer-0", "toast-answer-1", "toast-answer-2"] {
        assert!(drawn(&mut toast, part), "{part} is drawn");
    }
    assert!(!drawn(&mut toast, "toast-command"), "a question has none");

    press(&mut toast, "toast-answer-2");
    let sent = answers(h.sent());
    assert_eq!(sent.len(), 1, "one answer: {sent:?}");
    assert_eq!(sent[0].2, PromptChoice::Option { index: 2 });
}

#[gpui::test]
fn a_plan_approval_shows_only_go_to_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let plan = json!({ "kind": "plan_approval", "plan": "the plan" });
    let mut h = raised(cx, &dir, plan);

    assert_eq!(h.toasts_shown()[0].height, 116, "the plain card");
    let mut toast = h.popout_window();
    assert!(drawn(&mut toast, "toast-go-to-pane"));
    assert!(!drawn(&mut toast, "toast-answer-0"), "nothing to answer");
}

#[gpui::test]
fn an_answer_sends_answer_prompt_to_the_sessions_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = raised_on_box(cx, &dir);
    h.send(DaemonMessage::SessionUpdated {
        session: waiting("s1", stamp(2), permission()),
        request_id: None,
    });
    h.sent();
    h.host(&saved_remote()).sent();

    let mut toast = h.popout_window();
    press(&mut toast, "toast-answer-0");

    let on_box = answers(h.host(&saved_remote()).sent());
    assert_eq!(on_box.len(), 1, "the remote host's: {on_box:?}");
    assert_eq!(on_box[0].0, "b1");
    assert_eq!(on_box[0].1, stamp(1), "chosen against the snapshot's stamp");
    assert_eq!(on_box[0].2, PromptChoice::Allow);
    assert!(answers(h.sent()).is_empty(), "This PC is not asked");

    press(&mut toast, "toast-dismiss");
    h.send(needs_you("s1"));
    let mut toast = h.popout_window();
    press(&mut toast, "toast-answer-0");

    let here = answers(h.sent());
    assert_eq!(here.len(), 1, "This PC's: {here:?}");
    assert_eq!(here[0].0, "s1");
    assert_eq!(here[0].1, stamp(2));
    assert!(
        answers(h.host(&saved_remote()).sent()).is_empty(),
        "the remote host is not asked"
    );
}

#[gpui::test]
fn a_sent_answer_draws_sent_and_a_second_press_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = raised(cx, &dir, permission());
    let mut toast = h.popout_window();
    toast.run_until_parked();
    let allow = toast
        .debug_bounds("toast-answer-0")
        .expect("Allow is drawn");

    press(&mut toast, "toast-answer-0");
    assert_eq!(answers(h.sent()).len(), 1, "the first press sends");

    assert!(drawn(&mut toast, "toast-sent"), "`Sent` takes their place");
    toast.simulate_click(allow.center(), Modifiers::none());
    toast.run_until_parked();
    assert!(answers(h.sent()).is_empty(), "a second press sends nothing");
    assert_eq!(h.window_count(), 2, "the toast stays until the wait moves");
}

#[gpui::test]
fn a_refusal_brings_the_buttons_back(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = raised(cx, &dir, permission());
    let mut toast = h.popout_window();
    press(&mut toast, "toast-answer-0");
    let id = answers(h.sent())
        .pop()
        .and_then(|(_, _, _, request_id)| request_id)
        .expect("the answer carries a request id");
    assert!(drawn(&mut toast, "toast-sent"));

    h.send(DaemonMessage::Error {
        message: "the prompt changed since".to_owned(),
        request_id: Some(id),
        code: None,
    });

    assert!(drawn(&mut toast, "toast-answer-0"), "the buttons are back");
    press(&mut toast, "toast-answer-1");
    assert_eq!(answers(h.sent()).len(), 1, "and they answer again");
}

#[gpui::test]
fn a_session_leaving_awaiting_input_closes_its_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = raised(cx, &dir, permission());
    assert_eq!(h.window_count(), 2, "the toast is up");

    h.send(DaemonMessage::SessionUpdated {
        session: session("s2").status("working").build(),
        request_id: None,
    });

    assert_eq!(h.window_count(), 1, "the toast went with the wait");
    assert_eq!(h.root(|root, _| root.shown_toasts()), no_toasts());
}

#[gpui::test]
fn a_changed_status_since_closes_its_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = raised(cx, &dir, permission());

    h.send(DaemonMessage::SessionUpdated {
        session: waiting("s2", stamp(1), permission()),
        request_id: None,
    });
    assert_eq!(h.window_count(), 2, "the same wait keeps its toast");

    h.send(DaemonMessage::SessionUpdated {
        session: waiting("s2", stamp(2), permission()),
        request_id: None,
    });
    assert_eq!(h.window_count(), 1, "this wait is another one");
}

#[gpui::test]
fn a_removed_session_closes_its_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2", "s3"]), json!({}));
    finish(&mut h, "s2");
    finish(&mut h, "s3");
    assert_eq!(h.window_count(), 3, "both Finished toasts are up");

    h.send(DaemonMessage::SessionRemoved {
        session_id: "s2".to_owned(),
    });
    assert_eq!(h.window_count(), 2, "removed: its toast went");

    let listed = vec![session("s1").build()];
    h.send(DaemonMessage::Sessions { sessions: listed });
    assert_eq!(h.window_count(), 1, "gone from the list: its toast went");
}

#[gpui::test]
fn a_host_disconnect_closes_its_toasts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = raised_on_box(cx, &dir);
    h.send(needs_you("s1"));
    assert_eq!(h.window_count(), 3, "a toast from each host");

    let mut closed = Connection::for_target(saved_remote());
    closed.on_connecting();
    let _ = closed.on_closed("socket closed".to_owned());
    h.host(&saved_remote()).event(NetEvent::State(closed));

    assert_eq!(
        h.root(|root, _| root.shown_toasts()),
        [("s1".to_owned(), "Needs you".to_owned())],
        "only that host's went"
    );
    assert_eq!(h.window_count(), 2);
}

#[gpui::test]
fn a_new_welcome_closes_that_hosts_toasts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = raised_on_box(cx, &dir);
    h.send(needs_you("s1"));
    assert_eq!(h.window_count(), 3, "a toast from each host");

    h.host(&saved_remote()).connect();

    assert_eq!(
        h.root(|root, _| root.shown_toasts()),
        [("s1".to_owned(), "Needs you".to_owned())],
        "only that host's went"
    );
    assert_eq!(h.window_count(), 2);
}

#[gpui::test]
fn focusing_the_session_closes_its_toast_under_the_skip_rule(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir, &sessions(&["s2"]), json!({}));
    h.send(needs_you("s1"));
    h.send(needs_you("s2"));
    assert_eq!(h.window_count(), 3, "raised with the window unfocused");

    h.cx.update(|window, _| window.activate_window());
    h.cx.run_until_parked();
    assert_eq!(
        h.root(|root, _| root.shown_toasts()),
        [("s2".to_owned(), "Needs you".to_owned())],
        "focus puts s1 on screen in a focused window; s2 is on another tab"
    );

    h.click_on("leaf-s2");
    assert_eq!(h.window_count(), 1, "showing s2 skips it too");
}

#[gpui::test]
fn a_toast_with_answers_outlives_the_plain_needs_you_time(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = raised(cx, &dir, permission());

    h.advance(Duration::from_secs(31));
    assert_eq!(h.window_count(), 2, "past the plain toast's thirty seconds");

    h.advance(Duration::from_secs(569));
    assert_eq!(h.window_count(), 1, "the ten-minute cap ends it");
}

#[gpui::test]
fn a_toast_dropped_at_once_plays_no_sound(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = sessions(&["s2", "s3", "s4", "s5", "s6"]);
    let chosen = json!({
        "needs_you_sound": "off",
        "finished_sound": { "bundled": "bell_falling_fifth" },
    });
    let mut h = opened(cx, &dir, &fixture, chosen);
    for id in ["s2", "s3", "s4", "s5"] {
        h.send(needs_you(id));
    }

    finish(&mut h, "s6");

    assert_eq!(
        h.window_count(),
        5,
        "four Needs you toasts keep their place"
    );
    assert!(h.sounds().is_empty(), "the dropped toast plays no sound");
}

#[gpui::test]
fn a_new_prompt_under_the_same_stamp_replaces_the_ask(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = raised(cx, &dir, question(&["main", "dev"]));
    let mut toast = h.popout_window();
    press(&mut toast, "toast-answer-0");
    assert_eq!(answers(h.sent()).len(), 1, "the first prompt is answered");
    let sounds = h.sounds().len();

    h.send(DaemonMessage::SessionUpdated {
        session: waiting("s2", stamp(1), permission()),
        request_id: None,
    });

    assert_eq!(h.window_count(), 2, "the toast stays, in its window");
    assert_eq!(h.sounds().len(), sounds, "a new prompt plays no sound");
    assert_eq!(
        h.toasts_restacked().last().map(|rect| rect.height),
        Some(182),
        "the card fits the new prompt"
    );
    assert!(
        drawn(&mut toast, "toast-command"),
        "the new prompt's command"
    );
    press(&mut toast, "toast-answer-0");
    let sent = answers(h.sent());
    assert_eq!(sent.len(), 1, "the new prompt can be answered: {sent:?}");
    assert_eq!(
        sent[0].2,
        PromptChoice::Allow,
        "with the new prompt's choice"
    );
}

#[gpui::test]
fn a_prompt_reported_after_the_raise_gains_its_buttons(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = sessions(&["s2"]);
    fixture.sessions[1] = session("s2")
        .status("awaiting_input")
        .status_since(stamp(1))
        .build();
    let mut h = opened(cx, &dir, &fixture, json!({}));
    h.send(needs_you("s2"));
    assert_eq!(h.toasts_shown()[0].height, 116, "no prompt to answer yet");
    let sounds = h.sounds().len();
    h.advance(Duration::from_secs(20));

    h.send(DaemonMessage::SessionUpdated {
        session: waiting("s2", stamp(1), permission()),
        request_id: None,
    });

    assert_eq!(h.sounds().len(), sounds, "the late prompt plays no sound");
    let mut toast = h.popout_window();
    assert!(drawn(&mut toast, "toast-answer-0"), "it gains its buttons");
    h.advance(Duration::from_secs(31));
    assert_eq!(h.window_count(), 2, "it lasts as a toast with answers");
    h.advance(Duration::from_secs(549));
    assert_eq!(h.window_count(), 1, "its time still runs from the raise");
}

#[gpui::test]
fn a_wait_that_ended_during_the_busy_check_shows_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "name": support::REMOTE, "port": 8787 }],
            "notifications": { "toast_style": "app" },
        }),
    );
    let mut fixture = sessions(&["s2", "s3"]);
    fixture.sessions[1] = waiting("s2", stamp(1), permission());
    fixture.sessions[2] = waiting("s3", stamp(1), permission());
    let mut h = Harness::with(&mut *cx, &dir, &fixture);
    h.host(&saved_remote()).connect();
    let listed = vec![waiting("b1", stamp(1), permission())];
    h.host(&saved_remote())
        .send(DaemonMessage::Sessions { sessions: listed });
    let message = |msg| NetEvent::Message(Box::new(msg));

    h.events_together(vec![
        message(needs_you("s2")),
        message(DaemonMessage::SessionUpdated {
            session: session("s2").status("working").build(),
            request_id: None,
        }),
        message(needs_you("s3")),
        message(DaemonMessage::SessionRemoved {
            session_id: "s3".to_owned(),
        }),
    ]);
    let mut closed = Connection::for_target(saved_remote());
    closed.on_connecting();
    let _ = closed.on_closed("socket closed".to_owned());
    h.host(&saved_remote())
        .events_together(vec![message(needs_you("b1")), NetEvent::State(closed)]);

    assert_eq!(h.window_count(), 1, "no toast window opens");
    assert_eq!(h.root(|root, _| root.shown_toasts()), no_toasts());
    assert!(h.sounds().is_empty(), "and no sound plays");
}

#[gpui::test]
fn a_question_with_five_options_shows_only_go_to_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = raised(cx, &dir, question(&["a", "b", "c", "d", "e"]));

    assert_eq!(h.toasts_shown()[0].height, 116, "the plain card");
    let mut toast = h.popout_window();
    assert!(drawn(&mut toast, "toast-go-to-pane"));
    assert!(!drawn(&mut toast, "toast-answer-0"), "nothing is offered");
    h.advance(Duration::from_secs(30));
    assert_eq!(h.window_count(), 1, "it lasts as a plain Needs you");
}
