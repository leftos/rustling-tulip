//! Pane action specs: the pane-close dialog, the header's move button, the
//! empty pane's menu and the Move panes dialog.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{Modifiers, TestAppContext, px};
use protocol::{
    ClientMessage, DaemonMessage, GridNode, PaneDropEdge, RearrangeLayout, SessionSnapshot,
    SplitDirection, TabEntry,
};
use rustling_tulip_native::appearance::{BUILTIN_ACCENT, PaneFrame};
use rustling_tulip_native::palette::{LINE, RAISED, SURFACE};
use rustling_tulip_native::{HeaderChip, HeaderChipKind, PANE_GUTTER, PaneHeaderParts};
use serde_json::json;
use support::{Fixture, Harness, TestDir, pane, session, split, tab};

/// Tab `t1`: pane `p1` showing `s1` beside the empty pane `p2`.
fn with_empty(s1: SessionSnapshot) -> Fixture {
    Fixture {
        sessions: vec![s1],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", None),
            ),
        )],
        ..Fixture::default()
    }
}

/// Tab `t1` with a pane for each of `sessions`, side by side.
fn grid_of(sessions: &[&str]) -> GridNode {
    let mut panes = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| pane(&format!("p{}", i + 1), Some(s)));
    let first = panes.next().expect("a session at least");
    panes.fold(first, |grid, next| {
        split(SplitDirection::Horizontal, grid, next)
    })
}

fn grid_fixture(sessions: &[&str]) -> Fixture {
    Fixture {
        sessions: sessions.iter().map(|s| session(s).build()).collect(),
        tabs: vec![tab("t1", &grid_of(sessions))],
        ..Fixture::default()
    }
}

fn diff_tab(id: &str) -> TabEntry {
    serde_json::from_value(json!({
        "id": id,
        "name": "a.rs",
        "content": { "kind": "diff", "repo_id": "r1", "path": "a.rs", "against": null },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("diff tab fixture")
}

fn kinds(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .map(|msg| {
            serde_json::to_value(msg).expect("a client message serializes")["type"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

fn active(h: &mut Harness) -> Option<String> {
    h.root(|root, _| root.active_tab_id().map(str::to_owned))
}

/// The daemon answers an extract with the new tab; it becomes active.
fn assert_new_tab_activates(h: &mut Harness) {
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t9", &pane("p9", Some("s1"))),
    });
    assert_eq!(active(h).as_deref(), Some("t9"), "the new tab shows");
}

#[gpui::test]
fn pane_close_on_empty_pane_sends_close_immediately(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").build()));
    h.sent();
    h.click_on("close-pane-p2");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ClosePane { tab_id, pane_id }] if tab_id == "t1" && pane_id == "p2"),
        "sent {sent:?}"
    );
    assert!(!h.in_model("pane-close"), "no dialog for an empty pane");
}

#[gpui::test]
fn pane_close_with_session_opens_dialog_pane_only_focused(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").label("work").build()));
    h.sent();
    h.click_on("close-pane-p1");
    assert!(h.sent().is_empty(), "the dialog asks first");
    assert!(h.in_model("pane-close"));
    assert_eq!(
        h.root(|root, _| root.pane_close_focus()).as_deref(),
        Some("pane-close-only")
    );
    let labels: Vec<String> = h
        .root(|root, _| root.pane_close_controls())
        .into_iter()
        .map(|(_, label)| label)
        .collect();
    assert_eq!(
        labels,
        [
            "Cancel",
            "Close pane but keep session in sidebar",
            "Close pane and close session",
            "✕"
        ]
    );
    assert!(!h.in_model("pane-close-delete"), "no worktree, no delete");
    h.keys("tab");
    assert_eq!(
        h.root(|root, _| root.pane_close_focus()).as_deref(),
        Some("pane-close-discard"),
        "Tab moves on"
    );
}

#[gpui::test]
fn pane_only_sends_close_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").build()));
    h.sent();
    h.click_on("close-pane-p1");
    h.click_on("pane-close-only");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ClosePane { tab_id, pane_id }] if tab_id == "t1" && pane_id == "p1"),
        "sent {sent:?}"
    );
    assert!(!h.in_model("pane-close"), "the dialog closes");
}

#[gpui::test]
fn discard_keep_sends_close_stop_discard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").in_repo("r1").build()));
    h.sent();
    h.click_on("close-pane-p1");
    h.click_on("pane-close-discard");
    let sent = h.sent();
    assert_eq!(
        kinds(&sent),
        ["close_pane", "stop_session", "discard_session"],
        "sent {sent:?}"
    );
    assert!(
        matches!(&sent[2], ClientMessage::DiscardSession { session_id, cleanup }
            if session_id == "s1" && cleanup.len() == 1 && !cleanup[0].remove_worktree),
        "sent {sent:?}"
    );
    assert!(!h.in_model("pane-close"));
}

#[gpui::test]
fn delete_worktree_opens_branch_fate_confirm(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").worktree("r1").build()));
    h.sent();
    h.click_on("close-pane-p1");
    assert_eq!(
        h.root(|root, _| root.pane_close_controls())
            .into_iter()
            .map(|(_, label)| label)
            .collect::<Vec<_>>(),
        [
            "Cancel",
            "Close pane, keep session, keep worktree",
            "Close pane, don't keep session, keep worktree",
            "Close pane, don't keep session, delete worktree",
            "✕"
        ]
    );
    h.click_on("pane-close-delete");
    assert!(!h.in_model("pane-close"), "the pane dialog gives way");
    assert_eq!(
        h.root(|root, _| root.delete_dialog_session().map(str::to_owned))
            .as_deref(),
        Some("s1"),
        "the delete-worktree confirm opens"
    );
    let sent = h.sent();
    assert_eq!(kinds(&sent), ["preview_discard"], "sent {sent:?}");
}

#[gpui::test]
fn pane_close_esc_cancels(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").build()));
    h.sent();
    h.click_on("close-pane-p1");
    h.keys("escape");
    assert!(!h.in_model("pane-close"), "Esc closes it");
    assert!(h.sent().is_empty(), "and sends nothing");

    h.click_on("close-pane-p1");
    h.keys("shift-tab");
    assert_eq!(
        h.root(|root, _| root.pane_close_focus()).as_deref(),
        Some("pane-close-cancel")
    );
    h.keys("enter");
    assert!(!h.in_model("pane-close"), "Enter presses Cancel");
    assert!(h.sent().is_empty());
}

const TRUSTED_TIP: &str = "Trusted launch: permission prompts were bypassed";
const HEADLESS_TIP: &str = "Headless session: it runs without a terminal";
/// A pane header's height, in logical pixels.
const HEADER_HEIGHT: f32 = 36.0;
/// A header chip's height, in logical pixels.
const CHIP_HEIGHT: f32 = 20.0;

fn chip(kind: HeaderChipKind, text: &str, tip: &str) -> HeaderChip {
    HeaderChip {
        kind,
        text: text.to_owned(),
        tip: tip.to_owned(),
    }
}

fn runtime(name: &str) -> HeaderChip {
    chip(HeaderChipKind::Runtime, name, &format!("Running {name}"))
}

/// The height of `selector`'s element, in logical pixels.
fn height_of(h: &mut Harness<'_>, selector: &str) -> f32 {
    h.bounds(selector).size.height / px(1.0)
}

/// Whether `selector`'s element has never been painted.
fn never_painted(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x < px(0.0)
}

fn frame_of(h: &mut Harness<'_>, pane_id: &str) -> PaneFrame {
    let pane_id = pane_id.to_owned();
    h.root(move |root, _| root.pane_frame_colors(&pane_id))
        .expect("the pane is in a tab")
}

fn header_fill_of(h: &mut Harness<'_>, pane_id: &str) -> u32 {
    let pane_id = pane_id.to_owned();
    h.root(move |root, _| root.pane_header_fill(&pane_id))
        .expect("the pane is in a tab")
}

fn header_of(h: &mut Harness<'_>, pane_id: &str) -> PaneHeaderParts {
    let pane_id = pane_id.to_owned();
    h.root(move |root, _| root.pane_header_parts(&pane_id))
        .expect("the pane shows a session")
}

/// Tab `t1`: `s1` in pane `p1` beside `s2` in pane `p2`.
fn side_by_side(s1: SessionSnapshot, s2: SessionSnapshot) -> Fixture {
    Fixture {
        sessions: vec![s1, s2],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        )],
        ..Fixture::default()
    }
}

#[gpui::test]
fn pane_header_shows_runtime_trusted_and_headless_chips(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = side_by_side(
        session("s1")
            .trusted()
            .status("awaiting_input")
            .label("repo · main")
            .cwd("D:/src/repo")
            .build(),
        session("s2").headless().agent("codex").build(),
    );
    let mut h = Harness::with(cx, &dir, &fixture);

    let header = header_of(&mut h, "p1");
    assert_eq!(header.status_tip, "status: awaiting input");
    assert_eq!(header.title, "repo · main");
    assert_eq!(header.title_tip, "repo · main\nCwd: D:/src/repo");
    assert_eq!(
        header.chips,
        [
            runtime("claude"),
            chip(HeaderChipKind::Trusted, "trusted", TRUSTED_TIP),
        ]
    );

    let header = header_of(&mut h, "p2");
    assert_eq!(
        header.chips,
        [
            runtime("codex"),
            chip(HeaderChipKind::Headless, "headless", HEADLESS_TIP),
        ],
        "a headless session says so in a chip"
    );

    for selector in [
        "pane-chip-p1-runtime",
        "pane-chip-p1-trusted",
        "pane-chip-p2-runtime",
        "pane-chip-p2-headless",
    ] {
        assert!(
            (height_of(&mut h, selector) - CHIP_HEIGHT).abs() < 0.5,
            "{selector} is a {CHIP_HEIGHT} px chip"
        );
    }
    assert!(never_painted(&mut h, "pane-chip-p1-headless"));
    assert!(never_painted(&mut h, "pane-chip-p2-trusted"));
}

#[gpui::test]
fn workspace_header_folds_further_members_into_a_count_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = side_by_side(
        session("s1")
            .in_workspace("ws1")
            .members(&[
                ("r1", "wt/a", "C:/wt/a/r1"),
                ("r2", "wt/a", "C:/wt/a/r2"),
                ("r3", "wt/b", "C:/wt/b/r3"),
            ])
            .build(),
        session("s2").build(),
    );
    let mut h = Harness::with(cx, &dir, &fixture);

    assert_eq!(
        header_of(&mut h, "p1").chips,
        [
            runtime("claude"),
            chip(HeaderChipKind::Branch, "r1:wt/a", "r1:wt/a\nC:/wt/a/r1"),
            chip(
                HeaderChipKind::More,
                "+2",
                "r2:wt/a\nC:/wt/a/r2\nr3:wt/b\nC:/wt/b/r3"
            ),
        ]
    );
    assert_eq!(header_of(&mut h, "p2").chips, [runtime("claude")]);
    for selector in ["pane-chip-p1-branch", "pane-chip-p1-more"] {
        assert!(
            (height_of(&mut h, selector) - CHIP_HEIGHT).abs() < 0.5,
            "{selector} is a {CHIP_HEIGHT} px chip"
        );
    }
    assert!(never_painted(&mut h, "pane-chip-p2-more"));
    for pane in ["p1", "p2"] {
        let height = height_of(&mut h, &format!("pane-header-{pane}"));
        assert!(
            (height - HEADER_HEIGHT).abs() < 0.5,
            "{pane}'s header is one {HEADER_HEIGHT} px row, members or not: {height}"
        );
    }
}

#[gpui::test]
fn repo_and_shell_headers_are_one_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = side_by_side(
        session("s1")
            .shell("D:/scratch")
            .program_name("pwsh")
            .build(),
        session("s2").in_repo("r1").build(),
    );
    let mut h = Harness::with(cx, &dir, &fixture);

    let header = header_of(&mut h, "p1");
    assert_eq!(header.title, "scratch");
    assert_eq!(header.chips, [runtime("pwsh")]);
    assert_eq!(
        header_of(&mut h, "p2").chips,
        [
            runtime("claude"),
            chip(HeaderChipKind::Branch, "r1:main", "r1:main"),
        ],
        "a member without a worktree path hovers as its name alone"
    );
    for pane in ["p1", "p2"] {
        let height = height_of(&mut h, &format!("pane-header-{pane}"));
        assert!(
            (height - HEADER_HEIGHT).abs() < 0.5,
            "{pane}'s header is {HEADER_HEIGHT} px: {height}"
        );
    }
}

#[gpui::test]
fn pane_cards_sit_a_gutter_apart_inside_the_grid_padding(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = side_by_side(session("s1").build(), session("s2").build());
    let mut h = Harness::with(cx, &dir, &fixture);

    let grid = h.bounds("tab-grid-t1");
    let (first, second) = (h.bounds("pane-frame-p1"), h.bounds("pane-frame-p2"));
    let divider = h.bounds("divider-t1-[]");
    let gap = (second.left() - first.right()) / px(1.0);
    assert!(
        (gap - PANE_GUTTER).abs() < 0.5,
        "the cards sit {PANE_GUTTER} px apart: {gap}"
    );
    assert!(
        (divider.size.width / px(1.0) - PANE_GUTTER).abs() < 0.5,
        "the gutter is the drag handle"
    );
    for (edge, inset) in [
        ("left", first.left() - grid.left()),
        ("top", first.top() - grid.top()),
        ("right", grid.right() - second.right()),
        ("bottom", grid.bottom() - second.bottom()),
    ] {
        let inset = inset / px(1.0);
        assert!(
            (inset - PANE_GUTTER).abs() < 0.5,
            "the grid's {edge} padding is {PANE_GUTTER} px: {inset}"
        );
    }
}

/// The gap between a pane header's parts, in logical pixels.
const HEADER_GAP: f32 = 8.0;
/// The header parts of pane `p1` that never shrink, right of its chips.
const P1_BUTTONS: [&str; 5] = [
    "pane-stop-p1",
    "split-right-p1",
    "split-down-p1",
    "pane-move-new-tab-p1",
    "close-pane-p1",
];

fn width_of(h: &mut Harness<'_>, selector: &str) -> f32 {
    h.bounds(selector).size.width / px(1.0)
}

/// Tab `t1` with `p1` (showing `s1`) taking `ratio` of the width beside
/// `p2` (showing `s2`).
fn split_at(ratio: f32) -> TabEntry {
    tab(
        "t1",
        &GridNode::Split {
            direction: SplitDirection::Horizontal,
            ratio,
            first: Box::new(pane("p1", Some("s1"))),
            second: Box::new(pane("p2", Some("s2"))),
        },
    )
}

/// Every header button of `p1` lies inside its card.
fn assert_buttons_inside_p1(h: &mut Harness<'_>, stage: &str) {
    let frame = h.bounds("pane-frame-p1");
    for button in P1_BUTTONS {
        let bounds = h.bounds(button);
        assert!(
            bounds.origin.x >= frame.left() && bounds.right() <= frame.right() + px(0.5),
            "{stage}: {button} at {bounds:?} lies inside the card {frame:?}"
        );
    }
}

#[gpui::test]
fn a_narrow_header_gives_up_the_branch_chip_before_the_title(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let long = |id: &str| {
        session(id)
            .label("Petal footer polish")
            .members(&[(
                "rustling-tulip",
                "feat/a-rather-long-branch-name",
                "C:/wt/x/rustling-tulip",
            )])
            .build()
    };
    let mut h = Harness::with(cx, &dir, &side_by_side(long("s1"), long("s2")));

    let frame = width_of(&mut h, "pane-frame-p1");
    let row = frame / 0.5;
    let title = width_of(&mut h, "pane-title-p1");
    let branch = width_of(&mut h, "pane-chip-p1-branch");
    let free = (h.bounds("pane-stop-p1").left() - h.bounds("pane-chip-p1-branch").right())
        / px(1.0)
        - 2.0 * HEADER_GAP;
    assert!(
        title > 40.0 && branch > 40.0 && free > 0.0,
        "a half-width pane shows its header whole: title {title}, branch {branch}, room {free}"
    );

    let short_of_branch = (frame - free - branch / 2.0) / row;
    h.send(DaemonMessage::TabUpdated {
        tab: split_at(short_of_branch),
    });
    assert_buttons_inside_p1(&mut h, "half a branch short");
    let squeezed = width_of(&mut h, "pane-chip-p1-branch");
    assert!(
        squeezed < branch - 1.0,
        "the branch chip gives way first: {squeezed} of {branch}"
    );
    let whole = width_of(&mut h, "pane-title-p1");
    assert!(
        (whole - title).abs() < 0.5,
        "the title stays whole while the branch chip gives way: {whole} of {title}"
    );

    let short_of_title = (frame - free - branch - title / 2.0) / row;
    h.send(DaemonMessage::TabUpdated {
        tab: split_at(short_of_title),
    });
    assert_buttons_inside_p1(&mut h, "half a title short");
    let cut = width_of(&mut h, "pane-title-p1");
    assert!(
        cut > 0.0 && cut < title - 1.0,
        "the title truncates once the branch chip has given way: {cut} of {title}"
    );
    assert!(
        (width_of(&mut h, "pane-chip-p1-runtime") - width_of(&mut h, "pane-chip-p2-runtime")).abs()
            < 0.5,
        "the runtime chip never shrinks"
    );
}

#[gpui::test]
fn pane_frame_colors_follow_the_focus(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = side_by_side(session("s1").build(), session("s2").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    for s in &fixture.sessions {
        h.answer_scrollback(&s.id, b"");
    }
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());

    assert_eq!(
        frame_of(&mut h, "p1"),
        PaneFrame {
            border: BUILTIN_ACCENT,
            accent_line: BUILTIN_ACCENT,
        },
        "the focused card has an accent border"
    );
    assert_eq!(header_fill_of(&mut h, "p1"), RAISED, "over a raised header");
    assert_eq!(
        frame_of(&mut h, "p2"),
        PaneFrame {
            border: LINE,
            accent_line: BUILTIN_ACCENT,
        },
        "an unfocused card has a plain border"
    );
    assert_eq!(
        header_fill_of(&mut h, "p2"),
        SURFACE,
        "over a surface header"
    );
}

#[gpui::test]
fn header_move_sends_extract_and_activates_new_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").build()));
    h.sent();
    assert!(
        !h.in_model("pane-move-new-tab-p2"),
        "an empty pane has no move button"
    );
    h.click_on("pane-move-new-tab-p1");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ExtractToNewTab { source_tab_id, pane_ids, name: None, layout: None }]
            if source_tab_id == "t1" && pane_ids == &["p1"]),
        "sent {sent:?}"
    );
    assert_new_tab_activates(&mut h);
}

#[gpui::test]
fn empty_pane_menu_move_to_tab_sends_move_pane_to_drop_target(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = with_empty(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.tabs.push(tab(
        "t2",
        &split(
            SplitDirection::Vertical,
            pane("p3", Some("s2")),
            pane("p4", None),
        ),
    ));
    fixture.tabs.push(diff_tab("t3"));
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();

    h.right_click_on("pane-header-p2");
    assert!(h.in_model("empty-pane-menu"));
    assert!(h.in_model("empty-pane-close"));
    h.click_on("empty-pane-menu-move");
    let rows = h.root(|root, _| root.empty_pane_menu_rows());
    assert_eq!(
        rows,
        [
            "empty-pane-menu-back",
            "empty-pane-move-new",
            "empty-pane-move-t2"
        ],
        "the diff tab and the pane's own tab are not targets"
    );
    h.click_on("empty-pane-move-t2");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::MovePane { src_tab_id, src_pane_id, dst_tab_id, dst_pane_id, edge: PaneDropEdge::Replace }]
            if src_tab_id == "t1" && src_pane_id == "p2" && dst_tab_id == "t2" && dst_pane_id == "p4"),
        "sent {sent:?}"
    );
    assert!(!h.in_model("empty-pane-menu"), "the menu closes");
}

#[gpui::test]
fn empty_pane_menu_new_tab_sends_extract(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").build()));
    h.sent();
    h.right_click_on("pane-header-p1");
    assert!(
        !h.in_model("empty-pane-menu"),
        "a bound pane has the session menu"
    );
    h.keys("escape");

    h.right_click_on("empty-pane-shell-p2");
    assert!(h.in_model("empty-pane-menu"), "the body opens it too");
    h.click_on("empty-pane-menu-move");
    h.click_on("empty-pane-move-new");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ExtractToNewTab { source_tab_id, pane_ids, name: None, layout: None }]
            if source_tab_id == "t1" && pane_ids == &["p2"]),
        "sent {sent:?}"
    );
    assert_new_tab_activates(&mut h);

    h.click_on("tab-t1");
    h.right_click_on("pane-header-p2");
    h.click_on("empty-pane-close");
    let sent = h.sent();
    assert!(
        sent.iter()
            .any(|m| matches!(m, ClientMessage::ClosePane { pane_id, .. } if pane_id == "p2")),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn move_panes_row_hidden_under_three_bound_panes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &grid_fixture(&["s1", "s2"]));
    h.right_click_on("tab-t1");
    assert!(h.in_model("tab-menu"));
    assert!(!h.in_model("tab-menu-move-panes"), "two panes: no row");
    h.keys("escape");

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &grid_of(&["s1", "s2", "s3"])),
    });
    h.right_click_on("tab-t1");
    assert!(
        h.in_model("tab-menu-move-panes"),
        "three panes: the row shows"
    );
}

#[gpui::test]
fn move_panes_dialog_sends_extract_with_layout_and_name(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &grid_fixture(&["s1", "s2", "s3", "s4"]));
    h.sent();
    h.right_click_on("tab-t1");
    h.click_on("tab-menu-move-panes");
    assert!(!h.in_model("tab-menu"), "the menu gives way");
    assert!(h.in_model("move-panes"));
    assert_eq!(
        h.root(|root, _| root.move_panes_focus()).as_deref(),
        Some("move-panes-pane-p1"),
        "the first box has the focus"
    );
    h.click_on("move-panes-confirm");
    assert!(h.sent().is_empty(), "nothing ticked: Move is disabled");
    assert!(h.in_model("move-panes"));

    h.keys("space");
    h.click_on("move-panes-pane-p3");
    h.click_on("move-panes-layout-stacked");
    h.click_on("move-panes-name");
    h.cx.simulate_input("Work");
    h.keys("enter");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ExtractToNewTab { source_tab_id, pane_ids, name: Some(name), layout: Some(RearrangeLayout::Vertical) }]
            if source_tab_id == "t1" && pane_ids == &["p1", "p3"] && name == "Work"),
        "sent {sent:?}"
    );
    assert!(!h.in_model("move-panes"), "the dialog closes");
    assert_new_tab_activates(&mut h);
}

#[gpui::test]
fn move_panes_esc_cancels(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &grid_fixture(&["s1", "s2", "s3"]));
    h.sent();
    h.right_click_on("tab-t1");
    h.click_on("tab-menu-move-panes");
    h.keys("space escape");
    assert!(!h.in_model("move-panes"), "Esc closes it");

    h.right_click_on("tab-t1");
    h.click_on("tab-menu-move-panes");
    h.click_on("move-panes-name");
    h.keys("escape");
    assert!(!h.in_model("move-panes"), "Esc from the name field too");
    assert!(h.sent().is_empty(), "nothing sent");
}

#[gpui::test]
fn move_panes_prunes_a_pane_removed_while_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &grid_fixture(&["s1", "s2", "s3", "s4"]));
    h.sent();
    h.right_click_on("tab-t1");
    h.click_on("tab-menu-move-panes");
    h.keys("space");
    h.click_on("move-panes-pane-p4");

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &grid_of(&["s1", "s2", "s3"])),
    });
    assert!(h.in_model("move-panes"), "three panes are left to move");
    assert!(
        !h.in_model("move-panes-pane-p4"),
        "the gone pane is off the list"
    );
    h.click_on("move-panes-confirm");
    let extracts: Vec<ClientMessage> = h
        .sent()
        .into_iter()
        .filter(|m| matches!(m, ClientMessage::ExtractToNewTab { .. }))
        .collect();
    assert!(
        matches!(extracts.as_slice(), [ClientMessage::ExtractToNewTab { pane_ids, .. }] if pane_ids == &["p1"]),
        "sent {extracts:?}"
    );

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &grid_of(&["s1", "s2", "s3"])),
    });
    h.right_click_on("tab-t1");
    h.click_on("tab-menu-move-panes");
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p9", None)),
    });
    assert!(!h.in_model("move-panes"), "no listed pane left: it closes");
}

#[gpui::test]
fn app_shortcuts_do_nothing_while_move_panes_name_has_focus(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &grid_fixture(&["s1", "s2", "s3"]));
    h.sent();
    h.right_click_on("tab-t1");
    h.click_on("tab-menu-move-panes");
    h.click_on("move-panes-name");
    h.keys("ctrl-t ctrl-shift-t ctrl-shift-n");
    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, ClientMessage::CreateTab { .. })),
        "sent {sent:?}"
    );
    assert!(h.in_model("move-panes"), "the dialog stays");
    assert!(
        !h.root(|root, _| root.spawn_dialog_open()),
        "no spawn dialog"
    );
}

#[gpui::test]
fn pane_close_dropped_when_pane_rebinds(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = with_empty(session("s1").build());
    fixture.sessions.push(session("s2").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("close-pane-p1");
    assert!(h.in_model("pane-close"));
    h.send(DaemonMessage::TabUpdated {
        tab: tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        ),
    });
    assert!(h.in_model("pane-close"), "another pane's change keeps it");
    h.send(DaemonMessage::TabUpdated {
        tab: tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s2")),
                pane("p2", None),
            ),
        ),
    });
    assert!(
        !h.in_model("pane-close"),
        "its pane shows another session now"
    );
    h.keys("escape");
    let closes = kinds(&h.sent())
        .into_iter()
        .filter(|kind| ["close_pane", "stop_session", "discard_session"].contains(&kind.as_str()))
        .count();
    assert_eq!(closes, 0, "nothing closed or stopped");
}

/// The undo entries on screen, as `(id, message)`, newest first.
fn undo_entries(h: &mut Harness) -> Vec<(u64, String)> {
    h.root(|root, _| {
        root.undo_entries()
            .iter()
            .map(|entry| (entry.id, entry.message.clone()))
            .collect()
    })
}

/// The first snapshot of the newest entry, as
/// `(tab id, index, was active, focus pane)`.
fn newest_snapshot(h: &mut Harness) -> Option<(String, usize, bool, Option<String>)> {
    h.root(|root, _| {
        let snapshot = root.undo_entries().first()?.snapshots.first()?;
        Some((
            snapshot.tab.id.clone(),
            snapshot.index,
            snapshot.restore_active,
            snapshot.focus_pane.clone(),
        ))
    })
}

/// The panes of `tab`, left to right, as `(pane id, session)`.
fn panes_of(tab: &TabEntry) -> Vec<(String, Option<String>)> {
    fn walk(node: &serde_json::Value, out: &mut Vec<(String, Option<String>)>) {
        match node["kind"].as_str() {
            Some("pane") => out.push((
                node["pane_id"].as_str().unwrap_or_default().to_owned(),
                node["session_id"].as_str().map(str::to_owned),
            )),
            Some("split") => {
                walk(&node["first"], out);
                walk(&node["second"], out);
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(
        &serde_json::to_value(&tab.content).expect("a tab serializes")["grid"],
        &mut out,
    );
    out
}

/// The tab each restore message carries, in the order they went out.
fn restored(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::RestoreTab { tab, .. } | ClientMessage::RestoreTabSnapshot { tab } => {
                Some(tab.id.clone())
            }
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn pane_only_close_offers_undo_with_session_label(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").label("work").build()));
    h.sent();
    h.click_on("close-pane-p1");
    h.click_on("pane-close-only");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ClosePane { tab_id, pane_id }] if tab_id == "t1" && pane_id == "p1"),
        "sent {sent:?}"
    );

    let (id, message) = undo_entries(&mut h)
        .first()
        .cloned()
        .expect("an undo entry");
    assert_eq!(message, "Closed pane \"work\"");
    assert_eq!(
        newest_snapshot(&mut h),
        Some(("t1".to_owned(), 0, true, Some("p1".to_owned()))),
        "the pane to focus again"
    );

    h.click_on(&format!("undo-action-{id}"));
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::RestoreTabSnapshot { tab }]
        if tab.id == "t1" && panes_of(tab) == [
            ("p1".to_owned(), Some("s1".to_owned())),
            ("p2".to_owned(), None),
        ]),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn empty_pane_close_says_closed_empty_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").build()));
    h.sent();
    h.click_on("close-pane-p2");
    assert_eq!(kinds(&h.sent()), ["close_pane"]);

    assert_eq!(
        undo_entries(&mut h),
        [(1, "Closed empty pane".to_owned())],
        "an empty pane has no label to name"
    );
    assert_eq!(
        newest_snapshot(&mut h),
        Some(("t1".to_owned(), 0, true, Some("p2".to_owned())))
    );
}

#[gpui::test]
fn undo_of_last_pane_close_sends_restore_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.sent();
    h.click_on("close-pane-p1");
    h.click_on("pane-close-only");
    h.sent();
    h.send(DaemonMessage::TabRemoved {
        tab_id: "t1".to_owned(),
    });
    h.sent();

    let (id, message) = undo_entries(&mut h)
        .first()
        .cloned()
        .expect("an undo entry");
    assert_eq!(message, "Closed pane \"s1\"");
    h.click_on(&format!("undo-action-{id}"));
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::RestoreTab { tab, index }] if tab.id == "t1" && *index == 0),
        "the tab went with its last pane, so it comes back: {sent:?}"
    );

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p1", Some("s1"))),
    });
    assert_eq!(
        active(&mut h).as_deref(),
        Some("t1"),
        "the restored tab shows again"
    );
}

#[gpui::test]
fn undo_of_last_pane_close_before_removal_sends_restore_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.sent();
    h.click_on("close-pane-p1");
    h.click_on("pane-close-only");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ClosePane { pane_id, .. }] if pane_id == "p1"),
        "sent {sent:?}"
    );
    let (id, message) = undo_entries(&mut h)
        .first()
        .cloned()
        .expect("an undo entry");
    assert_eq!(message, "Closed pane \"s1\"");

    // The tab goes with its last pane, and the removal has not arrived yet,
    // so the client still lists t1.
    h.click_on(&format!("undo-action-{id}"));
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::RestoreTab { tab, index }] if tab.id == "t1" && *index == 0),
        "the tab its last pane went with comes back: {sent:?}"
    );

    h.send(DaemonMessage::TabRemoved {
        tab_id: "t1".to_owned(),
    });
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p1", Some("s1"))),
    });
    assert_eq!(
        active(&mut h).as_deref(),
        Some("t1"),
        "the restored tab shows again"
    );
}

#[gpui::test]
fn discard_offers_no_undo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").in_repo("r1").build()));
    h.sent();
    h.click_on("close-pane-p1");
    h.click_on("pane-close-discard");
    h.sent();
    assert!(
        undo_entries(&mut h).is_empty(),
        "discarding a session cannot be taken back"
    );
}

#[gpui::test]
fn move_to_tab_undo_restores_both_tab_snapshots(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = with_empty(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.tabs.push(tab(
        "t2",
        &split(
            SplitDirection::Vertical,
            pane("p3", Some("s2")),
            pane("p4", None),
        ),
    ));
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    h.right_click_on("pane-header-p2");
    h.click_on("empty-pane-menu-move");
    h.click_on("empty-pane-move-t2");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::MovePane { src_pane_id, dst_tab_id, .. }]
            if src_pane_id == "p2" && dst_tab_id == "t2"),
        "sent {sent:?}"
    );

    let (id, message) = undo_entries(&mut h)
        .first()
        .cloned()
        .expect("an undo entry");
    assert_eq!(message, "Moved pane");
    let snapshots = h.root(|root, _| {
        root.undo_entries()
            .first()
            .map(|entry| {
                entry
                    .snapshots
                    .iter()
                    .map(|snapshot| {
                        (
                            snapshot.tab.id.clone(),
                            snapshot.index,
                            snapshot.restore_active,
                            snapshot.focus_pane.clone(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    assert_eq!(
        snapshots,
        [
            ("t1".to_owned(), 0, true, Some("p2".to_owned())),
            ("t2".to_owned(), 1, false, Some("p2".to_owned())),
        ],
        "the tab it left and the tab it went to"
    );

    h.click_on(&format!("undo-action-{id}"));
    let sent = h.sent();
    assert_eq!(restored(&sent), ["t1", "t2"], "both go back: {sent:?}");
    assert!(
        matches!(&sent[0], ClientMessage::RestoreTabSnapshot { tab }
            if tab.id == "t1" && panes_of(tab).iter().any(|(pane, session)| pane == "p2" && session.is_none())),
        "the moved pane is back in the tab it left: {sent:?}"
    );
    assert_eq!(active(&mut h).as_deref(), Some("t1"), "the tab shown stays");
}

#[gpui::test]
fn move_undo_before_source_removal_restores_source_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s2").build()],
        tabs: vec![
            tab("t1", &pane("p1", None)),
            tab(
                "t2",
                &split(
                    SplitDirection::Vertical,
                    pane("p3", Some("s2")),
                    pane("p4", None),
                ),
            ),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    h.right_click_on("pane-header-p1");
    h.click_on("empty-pane-menu-move");
    h.click_on("empty-pane-move-t2");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::MovePane { src_pane_id, dst_tab_id, .. }]
            if src_pane_id == "p1" && dst_tab_id == "t2"),
        "sent {sent:?}"
    );

    let (id, message) = undo_entries(&mut h)
        .first()
        .cloned()
        .expect("an undo entry");
    assert_eq!(message, "Moved pane");

    // t1 held that pane alone, so the move takes the tab with it and the
    // removal is on its way; the client still lists t1.
    h.click_on(&format!("undo-action-{id}"));
    let sent = h.sent();
    assert_eq!(restored(&sent), ["t1", "t2"], "both go back: {sent:?}");
    assert!(
        matches!(&sent[0], ClientMessage::RestoreTab { tab, index } if tab.id == "t1" && *index == 0),
        "the tab the pane left comes back: {sent:?}"
    );
    assert!(
        matches!(&sent[1], ClientMessage::RestoreTabSnapshot { tab } if tab.id == "t2"),
        "the destination is replaced in place: {sent:?}"
    );
}

#[gpui::test]
fn move_to_new_tab_offers_no_undo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").build()));
    h.sent();
    h.click_on("pane-move-new-tab-p1");
    assert_eq!(kinds(&h.sent()), ["extract_to_new_tab"]);
    assert!(
        undo_entries(&mut h).is_empty(),
        "a move into a new tab cannot be taken back"
    );
}
