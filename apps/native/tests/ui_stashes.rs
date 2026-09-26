//! Source-control stash specs: the list requests seeding and Refresh send,
//! the Stashes header and its persisted caret, the rows, the push input,
//! pop, apply and the drop confirm, the pending state and a failed list.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{TestAppContext, px};
use protocol::{ClientMessage, DaemonMessage};
use rustling_tulip_native::{ScKey, ScStashes, StashButton, ToastKind};
use serde_json::{Value, json};
use support::{Fixture, Harness, TestDir, repo, session};

const HEADER: &str = "sc-stashes-r1::";
const INPUT: &str = "sc-stash-input-r1::";
const PUSH: &str = "sc-stash-push-r1::";

fn one_repo() -> Fixture {
    Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        ..Fixture::default()
    }
}

fn main_key() -> ScKey {
    ScKey {
        repo_id: "r1".to_owned(),
        worktree: None,
    }
}

/// r1's stash list as the daemon sends it from `worktree`, each stash as
/// `(subject, created_at)` and numbered newest first.
fn stashes_from(worktree: Option<&str>, stashes: &[(&str, &str)]) -> DaemonMessage {
    let stashes: Vec<Value> = stashes
        .iter()
        .enumerate()
        .map(|(i, (subject, created_at))| {
            json!({ "id": format!("stash@{{{i}}}"), "subject": subject, "created_at": created_at })
        })
        .collect();
    serde_json::from_value(json!({
        "type": "stashes",
        "repo_id": "r1",
        "stashes": stashes,
        "worktree_path": worktree,
    }))
    .expect("stashes fixture")
}

/// r1's main-tree stash list.
fn stashes(stashes: &[(&str, &str)]) -> DaemonMessage {
    stashes_from(None, stashes)
}

fn two_stashes() -> DaemonMessage {
    stashes(&[
        ("WIP on main: 1a2b3c4 newest", "2024-05-01T12:34:56Z"),
        ("On main: older", "2024-04-30T08:00:00+02:00"),
    ])
}

/// The list requests sent since the last read, as `(repo, worktree,
/// request id)`.
fn list_requests(h: &mut Harness<'_>) -> Vec<(String, Option<String>, Option<String>)> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ListStashes {
                repo_id,
                worktree_path,
                request_id,
            } => Some((repo_id, worktree_path, request_id)),
            _ => None,
        })
        .collect()
}

/// A stash write the client sent: its kind, repo, message or stash id, and
/// worktree.
type Write = (&'static str, String, String, Option<String>);

/// The stash writes sent since the last read.
fn writes(h: &mut Harness<'_>) -> Vec<Write> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::StashPush {
                repo_id,
                message,
                worktree_path,
            } => Some(("push", repo_id, message, worktree_path)),
            ClientMessage::StashPop {
                repo_id,
                stash_id,
                worktree_path,
            } => Some(("pop", repo_id, stash_id, worktree_path)),
            ClientMessage::StashApply {
                repo_id,
                stash_id,
                worktree_path,
            } => Some(("apply", repo_id, stash_id, worktree_path)),
            ClientMessage::StashDrop {
                repo_id,
                stash_id,
                worktree_path,
            } => Some(("drop", repo_id, stash_id, worktree_path)),
            _ => None,
        })
        .collect()
}

fn write(kind: &'static str, text: &str) -> Write {
    (kind, "r1".to_owned(), text.to_owned(), None)
}

/// The first section's Stashes part.
fn part(h: &mut Harness<'_>) -> ScStashes {
    h.root(|root, _| root.source_control_panel())
        .sections
        .into_iter()
        .next()
        .expect("a section")
        .stashes
        .expect("the Stashes part shows")
}

fn labels(buttons: &[StashButton]) -> Vec<(&'static str, bool)> {
    buttons
        .iter()
        .map(|button| (button.label, button.enabled))
        .collect()
}

/// Every row's buttons, as `(label, enabled)`.
fn row_buttons(h: &mut Harness<'_>) -> Vec<Vec<(&'static str, bool)>> {
    part(h)
        .rows
        .iter()
        .map(|row| labels(&row.buttons))
        .collect()
}

fn push_state(h: &mut Harness<'_>) -> (&'static str, bool) {
    let push = part(h).push.expect("the push row shows");
    (push.label, push.enabled)
}

fn input_text(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, cx| root.sc_stash_input(&main_key(), cx))
}

fn confirm_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    h.root(|root, _| root.stash_drop_confirm())
        .map(|confirm| confirm.focused)
}

fn saved_ui(dir: &TestDir) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("native-ui.json")).expect("native-ui.json"),
    )
    .expect("native-ui.json is JSON")
}

/// Types `text` into the field that has the keyboard, a space as `space`.
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

/// The panel on r1 with the Stashes part expanded; returns the id of the
/// list request seeding sent.
fn open_expanded<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> (Harness<'a>, Option<String>) {
    let mut h = Harness::with(cx, dir, &one_repo());
    h.click_on("activity-source-control");
    let requests = list_requests(&mut h);
    assert_eq!(requests.len(), 1, "one list request for r1");
    h.click_on(HEADER);
    assert!(!part(&mut h).collapsed, "the part unfolded");
    let id = requests.into_iter().next().and_then(|(_, _, id)| id);
    (h, id)
}

#[gpui::test]
fn seeding_sends_one_list_request_per_repo_with_an_id(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let s1 = session("s1")
        .members(&[
            ("r1", "main", "D:/src/r1"),
            ("r1", "feat", "C:/wt/r1"),
            ("r2", "feat", "C:/wt/r2"),
        ])
        .build();
    let mut fixture = Fixture::single(s1);
    fixture.repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(
        list_requests(&mut h),
        [
            ("r1".to_owned(), None, Some("sc-stashes-1".to_owned())),
            (
                "r2".to_owned(),
                Some("C:/wt/r2".to_owned()),
                Some("sc-stashes-2".to_owned())
            ),
        ],
        "one per repo, from its first section's tree"
    );

    h.send(DaemonMessage::Repos {
        repos: fixture.repos.clone(),
    });
    assert!(
        list_requests(&mut h).is_empty(),
        "a repo already asked for is not asked again"
    );

    h.click_on("activity-source-control");
    h.click_on("sc-refresh");
    assert_eq!(
        list_requests(&mut h),
        [
            ("r1".to_owned(), None, Some("sc-stashes-3".to_owned())),
            (
                "r2".to_owned(),
                Some("C:/wt/r2".to_owned()),
                Some("sc-stashes-4".to_owned())
            ),
        ],
        "Refresh asks again for every section repo"
    );
}

#[gpui::test]
fn the_header_counts_and_its_caret_persists(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    {
        let mut h = Harness::with(cx, &dir, &one_repo());
        h.click_on("activity-source-control");
        let folded = part(&mut h);
        assert!(folded.collapsed, "folded by default");
        assert_eq!(folded.count, None, "no count before the list");
        assert!(folded.push.is_none() && folded.rows.is_empty());
        assert!(h.bounds(HEADER).size.height > px(0.0));

        h.send(two_stashes());
        let folded = part(&mut h);
        assert_eq!(folded.count, Some(2));
        assert!(folded.rows.is_empty(), "folded, no rows");

        h.click_on(HEADER);
        assert!(!part(&mut h).collapsed);
        assert_eq!(
            saved_ui(&dir)["source_control"]["collapsed"]["r1::|stashes"],
            json!(false)
        );
    }

    // The panel itself is restored, so it is not clicked open again.
    let mut h = Harness::with(cx, &dir, &one_repo());
    assert!(!part(&mut h).collapsed, "restored unfolded");
    h.click_on(HEADER);
    assert!(part(&mut h).collapsed);
    assert_eq!(
        saved_ui(&dir)["source_control"]["collapsed"]["r1::|stashes"],
        json!(true)
    );
}

#[gpui::test]
fn no_stashes_and_the_rows_render_with_their_tooltip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_expanded(cx, &dir);
    assert_eq!(part(&mut h).body.as_deref(), Some("loading…"));
    assert_eq!(push_state(&mut h), ("Stash", true));

    h.send(stashes(&[]));
    let empty = part(&mut h);
    assert_eq!(empty.body.as_deref(), Some("no stashes"));
    assert_eq!(empty.count, Some(0));

    h.send(stashes_from(
        Some("C:/wt/r1"),
        &[
            ("WIP on main: 1a2b3c4 newest", "2024-05-01T12:34:56Z"),
            ("On main: older", "not a date"),
        ],
    ));
    let listed = part(&mut h);
    assert_eq!(listed.body, None);
    assert_eq!(
        listed.count,
        Some(2),
        "a worktree's list is the repo's, so the main tree shows it"
    );
    let rows: Vec<(String, String, String)> = listed
        .rows
        .iter()
        .map(|row| (row.id.clone(), row.subject.clone(), row.tooltip.clone()))
        .collect();
    assert_eq!(
        rows,
        [
            (
                "stash@{0}".to_owned(),
                "WIP on main: 1a2b3c4 newest".to_owned(),
                "WIP on main: 1a2b3c4 newest\n2024-05-01 12:34".to_owned()
            ),
            (
                "stash@{1}".to_owned(),
                "On main: older".to_owned(),
                "On main: older\nnot a date".to_owned()
            ),
        ]
    );
    assert_eq!(
        labels(&listed.rows[0].buttons),
        [("pop", true), ("apply", true), ("drop", true)]
    );
    let danger: Vec<bool> = listed.rows[0]
        .buttons
        .iter()
        .map(|button| button.danger)
        .collect();
    assert_eq!(danger, [false, false, true], "drop in the danger colour");
    assert!(h.bounds("sc-stash-row-r1::|stash@{1}").size.height > px(0.0));
    assert!(h.bounds("sc-stash-drop-r1::|stash@{0}").size.height > px(0.0));
}

#[gpui::test]
fn enter_in_the_input_sends_a_trimmed_push_and_clears_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_expanded(cx, &dir);
    h.send(stashes(&[]));
    assert_eq!(input_text(&mut h).as_deref(), Some(""));
    assert!(h.bounds(INPUT).size.height > px(0.0));

    h.click_on(INPUT);
    type_text(&mut h, " fix it ");
    assert_eq!(input_text(&mut h).as_deref(), Some(" fix it "));
    h.keys("enter");
    assert_eq!(writes(&mut h), [write("push", "fix it")]);
    assert_eq!(input_text(&mut h).as_deref(), Some(""), "the input clears");
    assert_eq!(push_state(&mut h), ("Stashing…", false));

    h.keys("enter");
    assert!(
        writes(&mut h).is_empty(),
        "a pending repo takes no second push"
    );
}

#[gpui::test]
fn pop_and_apply_send(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_expanded(cx, &dir);
    h.send(two_stashes());

    h.click_on("sc-stash-pop-r1::|stash@{1}");
    assert_eq!(writes(&mut h), [write("pop", "stash@{1}")]);

    h.send(two_stashes());
    h.click_on("sc-stash-apply-r1::|stash@{0}");
    assert_eq!(writes(&mut h), [write("apply", "stash@{0}")]);
    assert!(
        h.root(|root, _| root.stash_drop_confirm()).is_none(),
        "neither asks first"
    );
}

#[gpui::test]
fn drop_opens_the_confirm_cancel_sends_nothing_and_confirming_sends_a_drop(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let (mut h, _) = open_expanded(cx, &dir);
    h.send(two_stashes());
    let drop = "sc-stash-drop-r1::|stash@{0}";

    h.click_on(drop);
    let confirm = h
        .root(|root, _| root.stash_drop_confirm())
        .expect("the confirm opened");
    assert_eq!(confirm.title, "Drop stash@{0}?");
    assert_eq!(
        confirm.body,
        "\"WIP on main: 1a2b3c4 newest\" is deleted from the stash list. Getting it back afterwards needs git fsck."
    );
    assert_eq!(confirm.focused, "stash-drop-confirm-cancel", "Cancel first");
    assert!(h.bounds("stash-drop-confirm-drop").size.height > px(0.0));
    assert!(writes(&mut h).is_empty(), "opening sends nothing");

    h.click_on("stash-drop-confirm-cancel");
    assert_eq!(confirm_focus(&mut h), None, "Cancel closes it");
    h.click_on(drop);
    h.keys("escape");
    assert_eq!(confirm_focus(&mut h), None, "so does Esc");
    h.click_on(drop);
    h.click_on("stash-drop-confirm-close");
    assert_eq!(confirm_focus(&mut h), None, "and the ✕");
    assert!(writes(&mut h).is_empty(), "none of them sends");

    h.click_on(drop);
    h.keys("tab");
    assert_eq!(confirm_focus(&mut h), Some("stash-drop-confirm-drop"));
    h.keys("shift-tab");
    assert_eq!(confirm_focus(&mut h), Some("stash-drop-confirm-cancel"));
    h.keys("tab");
    h.keys("enter");
    assert_eq!(confirm_focus(&mut h), None);
    assert_eq!(writes(&mut h), [write("drop", "stash@{0}")]);

    h.send(two_stashes());
    h.click_on("sc-stash-drop-r1::|stash@{1}");
    h.click_on("stash-drop-confirm-drop");
    assert_eq!(writes(&mut h), [write("drop", "stash@{1}")]);
}

#[gpui::test]
fn pending_disables_the_buttons_and_a_list_re_enables_them(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_expanded(cx, &dir);
    h.send(two_stashes());

    h.click_on(PUSH);
    assert_eq!(
        writes(&mut h),
        [write("push", "")],
        "an empty message goes as empty"
    );
    assert_eq!(push_state(&mut h), ("Stashing…", false));
    let disabled = vec![("pop", false), ("apply", false), ("drop", false)];
    assert_eq!(row_buttons(&mut h), [disabled.clone(), disabled]);

    h.click_on("sc-stash-pop-r1::|stash@{0}");
    h.click_on(PUSH);
    assert!(writes(&mut h).is_empty(), "a disabled button sends nothing");

    h.send(two_stashes());
    assert_eq!(push_state(&mut h), ("Stash", true));
    let enabled = vec![("pop", true), ("apply", true), ("drop", true)];
    assert_eq!(row_buttons(&mut h), [enabled.clone(), enabled]);
}

#[gpui::test]
fn a_stash_pop_write_error_clears_pending(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_expanded(cx, &dir);
    h.send(two_stashes());
    h.click_on("sc-stash-pop-r1::|stash@{0}");
    assert_eq!(writes(&mut h), [write("pop", "stash@{0}")]);
    assert_eq!(push_state(&mut h), ("Stash", false));

    h.send(DaemonMessage::GitWriteError {
        repo_id: "r1".to_owned(),
        operation: "stash_pop".to_owned(),
        error: "conflict in a.rs".to_owned(),
        worktree_path: None,
    });
    assert_eq!(push_state(&mut h), ("Stash", true));
    assert_eq!(
        row_buttons(&mut h)[0],
        [("pop", true), ("apply", true), ("drop", true)]
    );
    assert_eq!(
        h.root(|root, _| root.toasts().len()),
        1,
        "one toast, from the source-control routing"
    );
    let banner = h.root(|root, _| root.source_control_panel()).sections[0]
        .banner
        .clone();
    assert_eq!(banner.as_deref(), Some("stash_pop: conflict in a.rs"));
}

#[gpui::test]
fn a_failed_list_shows_its_text_and_raises_no_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, id) = open_expanded(cx, &dir);
    assert_eq!(id.as_deref(), Some("sc-stashes-1"));

    h.send(DaemonMessage::Error {
        message: "stash list failed: not a git repository".to_owned(),
        request_id: id,
    });
    assert_eq!(
        part(&mut h).body.as_deref(),
        Some("couldn't load stashes: not a git repository")
    );
    assert!(
        h.root(|root, _| root.toasts().is_empty()),
        "the part says it, so no toast does"
    );

    h.send(DaemonMessage::Repos {
        repos: one_repo().repos,
    });
    assert!(
        list_requests(&mut h).is_empty(),
        "a failed repo is not asked for again by itself"
    );
    h.click_on("sc-refresh");
    assert_eq!(
        list_requests(&mut h),
        [("r1".to_owned(), None, Some("sc-stashes-2".to_owned()))]
    );
    h.send(stashes(&[]));
    assert_eq!(part(&mut h).body.as_deref(), Some("no stashes"));
}

/// The toasts up now, as `(kind, title)`.
fn toasts(h: &mut Harness<'_>) -> Vec<(ToastKind, String)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.kind, toast.title.clone()))
            .collect()
    })
}

#[gpui::test]
fn the_drop_confirm_closes_unsent_when_the_list_shifts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_expanded(cx, &dir);
    h.send(two_stashes());
    h.click_on("sc-stash-drop-r1::|stash@{1}");
    assert!(confirm_focus(&mut h).is_some(), "the confirm opened");

    h.send(two_stashes());
    assert!(
        confirm_focus(&mut h).is_some(),
        "a list that still holds the stash leaves it open"
    );

    // A terminal popped stash@{0}, so the stash the confirm names is now
    // stash@{0} and stash@{1} is gone.
    h.send(stashes(&[("On main: older", "2024-04-30T08:00:00+02:00")]));
    assert_eq!(confirm_focus(&mut h), None, "the confirm closed");
    assert!(writes(&mut h).is_empty(), "nothing was dropped");
    assert_eq!(
        toasts(&mut h),
        [(ToastKind::Info, "Stash list changed".to_owned())]
    );
}

#[gpui::test]
fn the_drop_confirm_closes_when_a_push_renumbers_the_stash(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (mut h, _) = open_expanded(cx, &dir);
    h.send(two_stashes());
    h.click_on("sc-stash-drop-r1::|stash@{0}");
    assert!(confirm_focus(&mut h).is_some());

    h.send(stashes(&[
        ("On main: brand new", "2024-05-02T09:00:00Z"),
        ("WIP on main: 1a2b3c4 newest", "2024-05-01T12:34:56Z"),
        ("On main: older", "2024-04-30T08:00:00+02:00"),
    ]));
    assert_eq!(
        confirm_focus(&mut h),
        None,
        "stash@{{0}} now names another stash"
    );
    h.keys("enter");
    assert!(writes(&mut h).is_empty(), "nothing was dropped");
}
