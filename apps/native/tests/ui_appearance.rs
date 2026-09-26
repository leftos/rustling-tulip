//! Appearance specs: the session accent on the sidebar stripe and the pane
//! frame, its inheritance from the container, the session menu's Accent
//! submenu, and the per-session terminal background.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]
#![expect(clippy::unreadable_literal, reason = "hex colors read as #rrggbb")]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{Modifiers, TestAppContext};
use protocol::{
    AppearanceOverrides, ClientMessage, DaemonMessage, RepoEntry, SessionSnapshot, SplitDirection,
};
use rustling_tulip_native::appearance::{BUILTIN_ACCENT, BUILTIN_BACKGROUND, PaneFrame};
use support::{Fixture, Harness, TestDir, pane, repo, session, split, tab};

/// The version the harness's handshake speaks.
const HARNESS_PROTOCOL: u32 = 1;
/// The Paper background preset.
const PAPER: u32 = 0xf6f4ef;
/// The default text colour the contrast-adjusted theme gives Paper.
const PAPER_FOREGROUND: u32 = 0x1a1c22;
/// The pane border of a pane without its tab's focus.
const BORDER: u32 = 0x0020_222a;

/// The harness on `fixture`, with every session's scrollback answered and
/// pane `p1` focused, and everything sent so far drained.
fn focused<'a>(cx: &'a mut TestAppContext, dir: &TestDir, fixture: &Fixture) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, fixture);
    for s in &fixture.sessions {
        h.answer_scrollback(&s.id, b"");
    }
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h
}

fn accent_of(h: &mut Harness<'_>, id: &str) -> Option<u32> {
    let id = id.to_owned();
    h.root(move |root, _| root.session_accent(&id))
}

fn frame_of(h: &mut Harness<'_>, pane_id: &str) -> PaneFrame {
    let pane_id = pane_id.to_owned();
    h.root(move |root, _| root.pane_frame_colors(&pane_id))
        .expect("the pane is in a tab")
}

/// What pane `pane_id` fills: its terminal's area and the ring around it.
fn fills_of(h: &mut Harness<'_>, pane_id: &str) -> (u32, u32) {
    let pane_id = pane_id.to_owned();
    h.root(move |root, cx| root.pane_fills(&pane_id, cx))
        .expect("the pane has a terminal")
}

/// Pane `pane_id`'s terminal background and default text colour.
fn terminal_colors(h: &mut Harness<'_>, pane_id: &str) -> (u32, u32) {
    let pane_id = pane_id.to_owned();
    h.root(move |root, cx| root.pane_terminal_colors(&pane_id, cx))
        .expect("the pane has a terminal")
}

/// The appearances of the `SetSessionAppearance`s sent for `session`.
fn session_appearances(sent: &[ClientMessage], session: &str) -> Vec<AppearanceOverrides> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetSessionAppearance {
                session_id,
                appearance,
                ..
            } if session_id == session => Some(appearance.clone()),
            _ => None,
        })
        .collect()
}

/// The `request_id`s of the `SetSessionAppearance`s sent for `session`.
fn appearance_request_ids(sent: &[ClientMessage], session: &str) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetSessionAppearance {
                session_id,
                request_id,
                ..
            } if session_id == session => request_id.clone(),
            _ => None,
        })
        .collect()
}

/// The titles and details of the toasts showing.
fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|t| (t.title.clone(), t.detail.clone()))
            .collect()
    })
}

/// Picks accent preset `name` from `session`'s menu.
fn pick_preset(h: &mut Harness<'_>, session: &str, name: &str) {
    open_accent_menu(h, session);
    h.click_on(&format!("accent-preset-{name}"));
}

/// The font size and accent of the send one Ctrl+= makes now.
fn next_step(h: &mut Harness<'_>) -> (Option<u16>, Option<String>) {
    h.sent();
    h.keys("ctrl-=");
    let sent = h.sent();
    let appearance = session_appearances(&sent, "s1")
        .pop()
        .expect("Ctrl+= sends the session's appearance");
    (appearance.terminal_font_size, appearance.accent_color)
}

/// Session `id` of repo `r1` whose own accent and frame are `color`.
fn accented(id: &str, color: &str) -> SessionSnapshot {
    let mut s = session(id).in_repo("r1").build();
    s.appearance.accent_color = Some(color.to_owned());
    s.appearance.terminal_frame_color = Some(color.to_owned());
    s
}

/// Repo `r1`, whose own accent is `color`.
fn accented_repo(color: &str) -> RepoEntry {
    let mut container = repo("r1", "C:/repos/r1");
    container.appearance.accent_color = Some(color.to_owned());
    container
}

/// Opens `session`'s menu from its sidebar row and then its Accent submenu.
fn open_accent_menu(h: &mut Harness<'_>, session: &str) {
    h.right_click_on(&format!("leaf-{session}"));
    assert!(h.in_model("session-menu-accent"), "the menu offers Accent");
    h.click_on("session-menu-accent");
    assert!(h.in_model("accent-inherit"), "the submenu is open");
    assert_eq!(
        h.root(|root, _| root.accent_menu_rows())
            .first()
            .map(String::as_str),
        Some("accent-back"),
        "Back comes first"
    );
    assert!(
        h.root(|root, _| root.menu_rows()).is_empty(),
        "the submenu replaces the action rows"
    );
}

#[gpui::test]
fn session_accent_colours_the_sidebar_stripe_and_focused_border(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut other = session("s2").in_repo("r1").build();
    other.appearance.accent_color = Some("#22C55E".to_owned());
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s2")),
    );
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/repos/r1")],
        sessions: vec![accented("s1", "#fb7185"), other],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    let mut h = focused(cx, &dir, &fixture);

    assert!(h.in_model("leaf-s1"));
    assert_eq!(
        accent_of(&mut h, "s1"),
        Some(0xfb7185),
        "the stripe's colour"
    );
    assert_eq!(
        frame_of(&mut h, "p1"),
        PaneFrame {
            border: 0xfb7185,
            accent_line: 0xfb7185,
        },
        "the focused pane's border and accent line take the session's accent"
    );
    assert_eq!(
        fills_of(&mut h, "p1"),
        (BUILTIN_BACKGROUND, 0xfb7185),
        "the ring around the terminal takes the session's frame"
    );
    assert_eq!(
        accent_of(&mut h, "s2"),
        Some(0x22c55e),
        "parsed in any case"
    );
    assert_eq!(
        frame_of(&mut h, "p2"),
        PaneFrame {
            border: BORDER,
            accent_line: 0x22c55e,
        },
        "a pane without the focus keeps the plain border"
    );
    assert_eq!(
        fills_of(&mut h, "p2"),
        (BUILTIN_BACKGROUND, BUILTIN_BACKGROUND),
        "no frame follows the terminal's background"
    );
    assert_eq!(accent_of(&mut h, "nope"), None, "no such session");
}

#[gpui::test]
fn repo_accent_is_inherited_by_its_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![accented_repo("#f59e0b")],
        sessions: vec![session("s1").in_repo("r1").build()],
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
        ..Fixture::default()
    };
    let mut h = focused(cx, &dir, &fixture);
    assert_eq!(accent_of(&mut h, "s1"), Some(0xf59e0b), "the repo's accent");
    assert_eq!(frame_of(&mut h, "p1").border, 0xf59e0b);

    h.send(DaemonMessage::Repos {
        repos: vec![accented_repo("#8b5cf6")],
    });
    assert_eq!(
        accent_of(&mut h, "s1"),
        Some(0x8b5cf6),
        "the repo's new accent applies live"
    );
    assert_eq!(frame_of(&mut h, "p1").border, 0x8b5cf6);

    h.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/repos/r1")],
    });
    assert_eq!(
        accent_of(&mut h, "s1"),
        Some(BUILTIN_ACCENT),
        "with no level setting one, the built-in accent"
    );
}

#[gpui::test]
fn accent_preset_from_the_session_menu_sends_both_fields(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r##"{ "recent_colors": ["#123456", "not a colour"] }"##,
    )
    .expect("write the saved layout");
    let mut s = session("s1").in_repo("r1").build();
    s.appearance.terminal_font_size = Some(16);
    s.appearance.terminal_background_color = Some("#111318".to_owned());
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/repos/r1")],
        ..Fixture::single(s)
    };
    let mut h = focused(cx, &dir, &fixture);

    open_accent_menu(&mut h, "s1");
    for name in [
        "default", "sky", "blue", "violet", "emerald", "amber", "rose",
    ] {
        assert!(h.in_model(&format!("accent-preset-{name}")), "{name}");
    }
    assert!(
        h.in_model("accent-recent-123456"),
        "the saved recent colour"
    );
    h.click_on("accent-preset-rose");

    let sent = h.sent();
    let appearances = session_appearances(&sent, "s1");
    assert_eq!(
        appearances,
        [AppearanceOverrides {
            accent_color: Some("#fb7185".to_owned()),
            terminal_frame_color: Some("#fb7185".to_owned()),
            terminal_background_color: Some("#111318".to_owned()),
            terminal_font_size: Some(16),
            ..AppearanceOverrides::default()
        }],
        "accent and frame both, the other fields as they were; sent {sent:?}"
    );
    assert!(!h.in_model("session-menu"), "the pick closes the menu");

    open_accent_menu(&mut h, "s1");
    h.click_on("accent-back");
    assert!(
        !h.root(|root, _| root.menu_rows()).is_empty(),
        "Back shows the action rows again"
    );
    assert!(h.in_model("session-menu-accent"));
    assert!(!h.in_model("accent-inherit"), "the submenu is closed");
    h.click_on("session-menu-accent");
    h.click_on("accent-recent-123456");
    let appearances = session_appearances(&h.sent(), "s1");
    assert_eq!(appearances.len(), 1);
    assert_eq!(appearances[0].accent_color.as_deref(), Some("#123456"));
    assert_eq!(
        appearances[0].terminal_frame_color.as_deref(),
        Some("#123456")
    );
}

#[gpui::test]
fn an_accent_pick_keeps_a_font_step_still_on_its_way(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/repos/r1")],
        ..Fixture::single(session("s1").in_repo("r1").build())
    };
    let mut h = focused(cx, &dir, &fixture);

    h.keys("ctrl-=");
    let sizes: Vec<Option<u16>> = session_appearances(&h.sent(), "s1")
        .iter()
        .map(|appearance| appearance.terminal_font_size)
        .collect();
    assert_eq!(sizes, [Some(14)], "the step goes out");

    open_accent_menu(&mut h, "s1");
    h.click_on("accent-preset-amber");

    let sent = h.sent();
    assert_eq!(
        session_appearances(&sent, "s1"),
        [AppearanceOverrides {
            accent_color: Some("#f59e0b".to_owned()),
            terminal_frame_color: Some("#f59e0b".to_owned()),
            terminal_font_size: Some(14),
            ..AppearanceOverrides::default()
        }],
        "the accent carries the size not yet echoed; sent {sent:?}"
    );
}

#[gpui::test]
fn a_status_update_between_accent_picks_leaves_the_last_pick(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/repos/r1")],
        ..Fixture::single(session("s1").in_repo("r1").build())
    };
    let mut h = focused(cx, &dir, &fixture);

    pick_preset(&mut h, "s1", "sky");
    pick_preset(&mut h, "s1", "rose");
    pick_preset(&mut h, "s1", "sky");
    let sent = h.sent();
    let accents: Vec<Option<String>> = session_appearances(&sent, "s1")
        .into_iter()
        .map(|appearance| appearance.accent_color)
        .collect();
    assert_eq!(
        accents,
        [
            Some("#38bdf8".to_owned()),
            Some("#fb7185".to_owned()),
            Some("#38bdf8".to_owned())
        ],
        "A, B, A all go out; sent {sent:?}"
    );

    for color in ["#38bdf8", "#fb7185"] {
        h.send(DaemonMessage::SessionUpdated {
            session: accented("s1", color),
            request_id: None,
        });
    }

    assert_eq!(
        next_step(&mut h),
        (Some(14), Some("#38bdf8".to_owned())),
        "status updates answer no send, so the last pick still stands"
    );
}

#[gpui::test]
fn a_refused_send_raises_a_toast_and_stops_overlaying(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/repos/r1")],
        ..Fixture::single(session("s1").in_repo("r1").build())
    };
    let mut h = focused(cx, &dir, &fixture);

    pick_preset(&mut h, "s1", "rose");
    let id = appearance_request_ids(&h.sent(), "s1")
        .pop()
        .expect("the pick carries a request id");
    let refusal = "accent color must use #RRGGBB format";
    h.send(DaemonMessage::Error {
        message: refusal.to_owned(),
        request_id: Some(id),
    });

    assert_eq!(
        toasts(&mut h),
        [(
            "Couldn't change the appearance".to_owned(),
            Some(refusal.to_owned())
        )]
    );
    assert_eq!(
        next_step(&mut h),
        (Some(14), None),
        "the refused accent no longer rides on later sends"
    );
}

#[gpui::test]
fn a_session_list_keeps_the_sends_in_flight_of_the_sessions_it_holds(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/repos/r1")],
        ..Fixture::single(session("s1").in_repo("r1").build())
    };
    let mut h = focused(cx, &dir, &fixture);
    let other = session("s2").in_repo("r1").build();
    pick_preset(&mut h, "s1", "sky");
    h.sent();

    h.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").in_repo("r1").build(), other.clone()],
    });
    assert_eq!(
        next_step(&mut h),
        (Some(14), Some("#38bdf8".to_owned())),
        "a listed session's pick may still be on the wire, so the next send carries it"
    );

    h.send(DaemonMessage::Sessions {
        sessions: vec![other],
    });
    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").in_repo("r1").build(),
        request_id: None,
    });
    assert_eq!(
        next_step(&mut h),
        (Some(14), None),
        "a session the list no longer holds loses its sends in flight"
    );
}

#[gpui::test]
fn a_program_background_fills_the_grid_and_an_unset_ring(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = focused(cx, &dir, &Fixture::single(session("s1").build()));

    h.pty("s1", b"\x1b]11;rgb:12/34/56\x07");

    assert_eq!(
        fills_of(&mut h, "p1"),
        (0x123456, 0x123456),
        "OSC 11 sets the terminal background, and no frame follows it"
    );
}

#[gpui::test]
fn resetting_the_program_background_returns_to_the_configured_one(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut paper = session("s1").build();
    paper.appearance.terminal_background_color = Some("#f6f4ef".to_owned());
    let mut h = focused(cx, &dir, &Fixture::single(paper));

    h.pty("s1", b"\x1b]11;rgb:12/34/56\x07");
    assert_eq!(fills_of(&mut h, "p1"), (0x123456, 0x123456));

    h.pty("s1", b"\x1b]111\x07");
    assert_eq!(
        fills_of(&mut h, "p1"),
        (PAPER, PAPER),
        "OSC 111 drops the program's background for the configured one"
    );
}

#[gpui::test]
fn a_removed_session_forgets_its_sends_in_flight(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/repos/r1")],
        ..Fixture::single(session("s1").in_repo("r1").build())
    };
    let mut h = focused(cx, &dir, &fixture);
    h.keys("ctrl-=");
    h.sent();

    h.send(DaemonMessage::SessionRemoved {
        session_id: "s1".to_owned(),
    });
    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").in_repo("r1").build(),
        request_id: None,
    });
    pick_preset(&mut h, "s1", "sky");

    let sent = h.sent();
    assert_eq!(
        session_appearances(&sent, "s1"),
        [AppearanceOverrides {
            accent_color: Some("#38bdf8".to_owned()),
            terminal_frame_color: Some("#38bdf8".to_owned()),
            ..AppearanceOverrides::default()
        }],
        "the unanswered size step went with the session; sent {sent:?}"
    );
}

#[gpui::test]
fn the_grid_paints_the_terminal_background_and_the_ring_the_frame(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut s = accented("s1", "#fb7185");
    s.appearance.terminal_background_color = Some("#f6f4ef".to_owned());
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/repos/r1")],
        ..Fixture::single(s)
    };
    let mut h = focused(cx, &dir, &fixture);
    assert_eq!(fills_of(&mut h, "p1"), (PAPER, 0xfb7185));
}

#[gpui::test]
fn an_unset_frame_follows_the_terminal_background(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut paper = session("s1").build();
    paper.appearance.terminal_background_color = Some("#f6f4ef".to_owned());
    let mut h = focused(cx, &dir, &Fixture::single(paper));
    assert_eq!(
        fills_of(&mut h, "p1"),
        (PAPER, PAPER),
        "no level sets a frame, so the ring is the terminal's background"
    );
}

#[gpui::test]
fn inherit_clears_the_session_accent(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut s = accented("s1", "#fb7185");
    s.appearance.terminal_font_size = Some(16);
    let fixture = Fixture {
        repos: vec![accented_repo("#22c55e")],
        ..Fixture::single(s.clone())
    };
    let mut h = focused(cx, &dir, &fixture);
    assert_eq!(accent_of(&mut h, "s1"), Some(0xfb7185));

    open_accent_menu(&mut h, "s1");
    h.click_on("accent-inherit");

    let sent = h.sent();
    let appearances = session_appearances(&sent, "s1");
    let cleared = AppearanceOverrides {
        terminal_font_size: Some(16),
        ..AppearanceOverrides::default()
    };
    assert_eq!(appearances, std::slice::from_ref(&cleared), "sent {sent:?}");
    assert!(!h.in_model("session-menu"), "Inherit closes the menu");

    s.appearance = cleared;
    h.send(DaemonMessage::SessionUpdated {
        session: s,
        request_id: None,
    });
    assert_eq!(
        accent_of(&mut h, "s1"),
        Some(0x22c55e),
        "the repo's accent shows through"
    );
    assert_eq!(
        fills_of(&mut h, "p1"),
        (BUILTIN_BACKGROUND, BUILTIN_BACKGROUND),
        "the cleared frame follows the terminal's background"
    );

    open_accent_menu(&mut h, "s1");
    h.click_on("accent-inherit");
    assert!(
        session_appearances(&h.sent(), "s1").is_empty(),
        "nothing left to clear sends nothing"
    );
}

#[gpui::test]
fn session_background_rebuilds_the_terminal_theme(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").build());
    let mut h = focused(cx, &dir, &fixture);
    let (background, dark_text) = terminal_colors(&mut h, "p1");
    assert_eq!(background, BUILTIN_BACKGROUND);

    let mut paper = session("s1").build();
    paper.appearance.terminal_background_color = Some("#F6F4EF".to_owned());
    h.send(DaemonMessage::SessionUpdated {
        session: paper,
        request_id: None,
    });

    assert_eq!(
        terminal_colors(&mut h, "p1"),
        (PAPER, PAPER_FOREGROUND),
        "Paper takes the dark text the rebuilt theme gives it"
    );
    assert_ne!(
        dark_text, PAPER_FOREGROUND,
        "the default theme's text is light"
    );
}

#[gpui::test]
fn background_survives_a_reattach(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut paper = session("s1").build();
    paper.appearance.terminal_background_color = Some("#f6f4ef".to_owned());
    let fixture = Fixture::single(paper);
    let mut h = focused(cx, &dir, &fixture);
    assert_eq!(terminal_colors(&mut h, "p1"), (PAPER, PAPER_FOREGROUND));

    // A new connection resets every pane to a fresh terminal, and the lists
    // that follow attach it again on another one.
    h.send(DaemonMessage::Welcome {
        protocol_version: HARNESS_PROTOCOL,
        supported_versions: vec![HARNESS_PROTOCOL],
    });
    assert_eq!(
        terminal_colors(&mut h, "p1"),
        (PAPER, PAPER_FOREGROUND),
        "the reset's fresh terminal keeps the background"
    );
    h.load(&fixture);
    h.answer_scrollback("s1", b"");
    assert_eq!(
        terminal_colors(&mut h, "p1"),
        (PAPER, PAPER_FOREGROUND),
        "and so does the reattach's"
    );
}
