//! Appearance editor specs: the editor a session's menu, a repo's or a
//! workspace's right-click and the Settings modal open, its resolved-value
//! hints, what each row sends at each level, and how it blocks and closes.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]
#![expect(clippy::unreadable_literal, reason = "hex colors read as #rrggbb")]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{Modifiers, TestAppContext};
use protocol::{
    AppearanceOverrides, ClientMessage, DaemonMessage, RepoEntry, SessionSnapshot, WorkspaceEntry,
};
use rustling_tulip_native::RootView;
use support::{Fixture, Harness, TestDir, pane, repo, session, tab, workspace};

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

/// Session `s1` of repo `r1`, alone in pane `p1`, with `own` appearance.
fn single_in_repo(own: AppearanceOverrides, container: RepoEntry) -> Fixture {
    let mut s = session("s1").in_repo("r1").build();
    s.appearance = own;
    Fixture {
        repos: vec![container],
        ..Fixture::single(s)
    }
}

/// Types `text` into the field that has the keyboard, one keystroke per
/// character.
fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
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

/// The repo and workspace appearances sent, each as `repo <id>` or
/// `workspace <id>` with its appearance.
fn container_appearances(sent: &[ClientMessage]) -> Vec<(String, AppearanceOverrides)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetRepoAppearance {
                repo_id,
                appearance,
            } => Some((format!("repo {repo_id}"), appearance.clone())),
            ClientMessage::SetWorkspaceAppearance {
                workspace_id,
                appearance,
            } => Some((format!("workspace {workspace_id}"), appearance.clone())),
            _ => None,
        })
        .collect()
}

/// Every appearance message sent, of any level.
fn appearance_sends(sent: &[ClientMessage]) -> usize {
    sent.iter()
        .filter(|msg| {
            matches!(
                msg,
                ClientMessage::SetSessionAppearance { .. }
                    | ClientMessage::SetRepoAppearance { .. }
                    | ClientMessage::SetWorkspaceAppearance { .. }
            )
        })
        .count()
}

fn editor_title(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.appearance_editor_title())
}

fn hint(h: &mut Harness<'_>, row: &str) -> String {
    let row = row.to_owned();
    h.root(move |root, _| root.appearance_hint(&row))
        .expect("the editor is open")
}

fn can_apply(h: &mut Harness<'_>, row: &str) -> bool {
    let row = row.to_owned();
    h.root(move |root, cx| root.appearance_can_apply(&row, cx))
}

fn family_names(h: &mut Harness<'_>) -> Vec<String> {
    h.root(RootView::appearance_family_names)
}

/// `native-ui.json` in the spec's ui dir, as it stands on disk.
fn saved_ui(dir: &TestDir) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("a saved layout");
    serde_json::from_str(&text).expect("native-ui.json is JSON")
}

/// Opens `session`'s editor from its sidebar row's menu.
fn open_session_editor(h: &mut Harness<'_>, session: &str) {
    h.right_click_on(&format!("leaf-{session}"));
    h.click_on("session-menu-appearance");
    assert_eq!(editor_title(h).as_deref(), Some("Session appearance"));
}

fn colored(accent: Option<&str>, background: Option<&str>) -> AppearanceOverrides {
    AppearanceOverrides {
        accent_color: accent.map(str::to_owned),
        terminal_frame_color: accent.map(str::to_owned),
        terminal_background_color: background.map(str::to_owned),
        ..AppearanceOverrides::default()
    }
}

#[gpui::test]
fn session_menu_opens_the_editor_with_resolved_hints(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut container = repo("r1", "C:/repos/r1");
    container.appearance.terminal_background_color = Some("#0B1020".to_owned());
    let fixture = single_in_repo(colored(Some("#fb7185"), None), container);
    let mut h = focused(cx, &dir, &fixture);

    h.right_click_on("leaf-s1");
    assert_eq!(
        h.root(|root, _| root.accent_menu_rows()),
        ["session-menu-appearance", "session-menu-accent"],
        "Appearance… sits right before Accent"
    );
    h.click_on("session-menu-appearance");

    assert_eq!(editor_title(&mut h).as_deref(), Some("Session appearance"));
    assert!(!h.in_model("session-menu"), "the menu gave way");
    assert_eq!(
        hint(&mut h, "background"),
        "Resolved: #0b1020 from repo",
        "the container's background"
    );
    assert_eq!(
        hint(&mut h, "accent"),
        "Resolved: #fb7185 from current",
        "a colour set at this level"
    );
    assert_eq!(
        hint(&mut h, "family"),
        "Resolved: default cascade from built-in"
    );
    assert_eq!(hint(&mut h, "size"), "Resolved: 13px from built-in");
    assert_eq!(hint(&mut h, "bold"), "Resolved: normal from built-in");
    assert_eq!(
        h.root(|root, _| root.appearance_reset_label()),
        Some("Inherit")
    );
}

#[gpui::test]
fn a_preset_click_sends_accent_and_frame_for_the_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let own = AppearanceOverrides {
        terminal_font_size: Some(16),
        ..AppearanceOverrides::default()
    };
    let fixture = single_in_repo(own, repo("r1", "C:/repos/r1"));
    let mut h = focused(cx, &dir, &fixture);
    open_session_editor(&mut h, "s1");

    h.click_on("appearance-accent-preset-rose");

    let sent = h.sent();
    assert_eq!(
        session_appearances(&sent, "s1"),
        [AppearanceOverrides {
            terminal_font_size: Some(16),
            ..colored(Some("#fb7185"), None)
        }],
        "accent and frame together, the size kept; sent {sent:?}"
    );
    assert!(
        sent.iter().all(|msg| !matches!(
            msg,
            ClientMessage::SetSessionAppearance {
                request_id: None,
                ..
            }
        )),
        "the send goes out under a request id"
    );
    assert!(
        editor_title(&mut h).is_some(),
        "changes apply live; the editor stays"
    );
    assert_eq!(
        hint(&mut h, "accent"),
        "Resolved: #fb7185 from current",
        "the send still on its way shows"
    );
    assert!(
        h.root(|root, _| root.appearance_recent_rows("accent"))
            .is_empty(),
        "a preset is not a recent colour"
    );
}

#[gpui::test]
fn custom_hex_apply_sends_and_enters_recent(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = single_in_repo(AppearanceOverrides::default(), repo("r1", "C:/repos/r1"));
    let mut h = focused(cx, &dir, &fixture);
    open_session_editor(&mut h, "s1");

    h.click_on("appearance-background-hex");
    type_text(&mut h, "#12AB34");
    assert!(can_apply(&mut h, "background"));
    h.click_on("appearance-background-apply");

    let sent = h.sent();
    assert_eq!(
        session_appearances(&sent, "s1"),
        [colored(None, Some("#12ab34"))],
        "the background alone, lowercased; sent {sent:?}"
    );
    assert_eq!(
        h.root(|root, _| root.appearance_recent_rows("accent")),
        ["appearance-accent-recent-12ab34"],
        "one recent list serves both rows"
    );
    assert_eq!(
        saved_ui(&dir)["recent_colors"],
        serde_json::json!(["#12ab34"]),
        "the recent colour is saved"
    );

    h.click_on("appearance-accent-hex");
    type_text(&mut h, "ABCDEF");
    h.keys("enter");
    assert_eq!(
        session_appearances(&h.sent(), "s1"),
        [AppearanceOverrides {
            terminal_background_color: Some("#12ab34".to_owned()),
            ..colored(Some("#abcdef"), None)
        }],
        "Enter applies too, without the #"
    );

    h.keys("escape");
    assert!(editor_title(&mut h).is_none(), "Esc in a field closes");
    open_session_editor(&mut h, "s1");
    assert_eq!(
        h.root(|root, _| root.appearance_recent_rows("background")),
        [
            "appearance-background-recent-abcdef",
            "appearance-background-recent-12ab34"
        ],
        "the newest first, on reopen"
    );
    assert_eq!(
        saved_ui(&dir)["recent_colors"],
        serde_json::json!(["#abcdef", "#12ab34"])
    );
}

#[gpui::test]
fn invalid_hex_disables_apply(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = single_in_repo(colored(None, Some("#111318")), repo("r1", "C:/repos/r1"));
    let mut h = focused(cx, &dir, &fixture);
    open_session_editor(&mut h, "s1");

    h.click_on("appearance-background-hex");
    type_text(&mut h, "#12345");
    assert!(!can_apply(&mut h, "background"), "five digits");
    h.click_on("appearance-background-apply");
    h.keys("enter");
    assert_eq!(appearance_sends(&h.sent()), 0, "nothing goes out");

    type_text(&mut h, "g");
    assert!(!can_apply(&mut h, "background"), "not a hex digit");

    h.keys("backspace backspace backspace backspace backspace backspace backspace");
    type_text(&mut h, "111318");
    assert!(
        !can_apply(&mut h, "background"),
        "the value the level already has"
    );
    type_text(&mut h, "0");
    assert!(!can_apply(&mut h, "background"), "seven digits");
    assert_eq!(appearance_sends(&h.sent()), 0);
}

#[gpui::test]
fn container_right_click_opens_the_container_editor_and_sends_set_repo_appearance(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut container = repo("r1", "C:/repos/r1");
    container.appearance.terminal_font_size = Some(15);
    let fixture = single_in_repo(AppearanceOverrides::default(), container);
    let mut h = focused(cx, &dir, &fixture);

    h.right_click_on("container-repo:r1");
    assert!(h.root(|root, _| root.container_menu_open()));
    h.click_on("container-menu-appearance");
    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "the menu gave way"
    );
    assert_eq!(editor_title(&mut h).as_deref(), Some("r1 appearance"));
    assert_eq!(
        h.root(|root, _| root.appearance_reset_label()),
        Some("Inherit app")
    );
    assert_eq!(hint(&mut h, "size"), "Resolved: 15px from repo");
    assert_eq!(hint(&mut h, "accent"), "Resolved: #5b9bff from built-in");

    h.click_on("appearance-accent-preset-sky");

    let sent = h.sent();
    let expected = AppearanceOverrides {
        terminal_font_size: Some(15),
        ..colored(Some("#38bdf8"), None)
    };
    assert_eq!(
        container_appearances(&sent),
        [("repo r1".to_owned(), expected)],
        "the repo's stored fields with the change over them; sent {sent:?}"
    );
    assert_eq!(appearance_sends(&sent), 1, "nothing for the session");

    h.keys("escape");
    h.right_click_on("container-repo:r1");
    assert!(h.root(|root, _| root.container_menu_open()));
    h.keys("escape");
    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "Esc closes the container menu"
    );
}

#[gpui::test]
fn accent_apply_acts_when_only_the_frame_would_change(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let own = AppearanceOverrides {
        accent_color: Some("#111111".to_owned()),
        terminal_frame_color: Some("#222222".to_owned()),
        ..AppearanceOverrides::default()
    };
    let fixture = single_in_repo(own, repo("r1", "C:/repos/r1"));
    let mut h = focused(cx, &dir, &fixture);
    open_session_editor(&mut h, "s1");

    h.click_on("appearance-accent-hex");
    type_text(&mut h, "#111111");
    assert!(
        can_apply(&mut h, "accent"),
        "the frame would move to the accent"
    );
    h.click_on("appearance-accent-apply");
    assert_eq!(
        session_appearances(&h.sent(), "s1"),
        [colored(Some("#111111"), None)]
    );
}

#[gpui::test]
fn container_changes_before_the_echo_build_on_each_other(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = single_in_repo(AppearanceOverrides::default(), repo("r1", "C:/repos/r1"));
    let mut h = focused(cx, &dir, &fixture);
    h.right_click_on("container-repo:r1");
    h.click_on("container-menu-appearance");

    h.click_on("appearance-accent-preset-sky");
    h.click_on("appearance-background-preset-paper");
    let both = colored(Some("#38bdf8"), Some("#f6f4ef"));
    assert_eq!(
        container_appearances(&h.sent()),
        [
            ("repo r1".to_owned(), colored(Some("#38bdf8"), None)),
            ("repo r1".to_owned(), both.clone()),
        ],
        "the second send carries the first, not yet echoed"
    );
    assert_eq!(hint(&mut h, "accent"), "Resolved: #38bdf8 from current");

    let mut echoed = repo("r1", "C:/repos/r1");
    echoed.appearance = colored(Some("#38bdf8"), None);
    h.send(DaemonMessage::Repos {
        repos: vec![echoed],
    });
    assert_eq!(
        hint(&mut h, "background"),
        "Resolved: #f6f4ef from current",
        "the first echo leaves the second send standing"
    );
    let mut echoed = repo("r1", "C:/repos/r1");
    echoed.appearance = both;
    h.send(DaemonMessage::Repos {
        repos: vec![echoed.clone()],
    });

    echoed.appearance = AppearanceOverrides::default();
    h.send(DaemonMessage::Repos {
        repos: vec![echoed],
    });
    assert_eq!(
        hint(&mut h, "accent"),
        "Resolved: #5b9bff from built-in",
        "once echoed, the stored overrides lead again"
    );
    h.click_on("appearance-bold");
    assert_eq!(
        container_appearances(&h.sent()),
        [(
            "repo r1".to_owned(),
            AppearanceOverrides {
                terminal_font_bold: Some(true),
                ..AppearanceOverrides::default()
            }
        )]
    );
}

#[gpui::test]
fn workspace_editor_sends_set_workspace_appearance(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let s: SessionSnapshot = session("s1").in_workspace("w1").build();
    let ws: WorkspaceEntry = workspace("w1", &["r1"]);
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/repos/r1")],
        workspaces: vec![ws],
        ..Fixture::single(s)
    };
    let mut h = focused(cx, &dir, &fixture);

    h.right_click_on("container-ws:w1");
    h.click_on("container-menu-appearance");
    assert_eq!(editor_title(&mut h).as_deref(), Some("w1 appearance"));
    h.click_on("appearance-background-preset-paper");

    let sent = h.sent();
    assert_eq!(
        container_appearances(&sent),
        [("workspace w1".to_owned(), colored(None, Some("#f6f4ef")))],
        "sent {sent:?}"
    );
    assert_eq!(appearance_sends(&sent), 1);
}

#[gpui::test]
fn font_size_steppers_clamp(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let own = AppearanceOverrides {
        terminal_font_size: Some(31),
        ..AppearanceOverrides::default()
    };
    let fixture = single_in_repo(own, repo("r1", "C:/repos/r1"));
    let mut h = focused(cx, &dir, &fixture);
    open_session_editor(&mut h, "s1");

    h.click_on("appearance-size-up");
    h.click_on("appearance-size-up");
    let sizes: Vec<Option<u16>> = session_appearances(&h.sent(), "s1")
        .iter()
        .map(|a| a.terminal_font_size)
        .collect();
    assert_eq!(sizes, [Some(32)], "the second step is past the limit");
    assert_eq!(hint(&mut h, "size"), "Resolved: 32px from session");

    h.click_on("appearance-size-reset");
    h.click_on("appearance-size-down");
    let sizes: Vec<Option<u16>> = session_appearances(&h.sent(), "s1")
        .iter()
        .map(|a| a.terminal_font_size)
        .collect();
    assert_eq!(
        sizes,
        [None, Some(12)],
        "reset clears the size; a step goes from the 13 it then resolves to"
    );

    for _ in 0..6 {
        h.click_on("appearance-size-down");
    }
    let sizes: Vec<Option<u16>> = session_appearances(&h.sent(), "s1")
        .iter()
        .map(|a| a.terminal_font_size)
        .collect();
    assert_eq!(
        sizes,
        [Some(11), Some(10), Some(9), Some(8)],
        "the steps stop at the smallest size"
    );
}

#[gpui::test]
fn bold_toggle_and_reset(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = single_in_repo(AppearanceOverrides::default(), repo("r1", "C:/repos/r1"));
    let mut h = focused(cx, &dir, &fixture);
    open_session_editor(&mut h, "s1");

    let can_reset = |h: &mut Harness<'_>| h.root(|root, _| root.appearance_can_reset("bold"));
    assert!(!can_reset(&mut h), "nothing set here to reset");
    h.click_on("appearance-bold");
    assert_eq!(hint(&mut h, "bold"), "Resolved: bold from session");
    assert!(can_reset(&mut h));
    h.click_on("appearance-bold");
    h.click_on("appearance-bold-reset");
    h.click_on("appearance-bold-reset");

    let bold: Vec<Option<bool>> = session_appearances(&h.sent(), "s1")
        .iter()
        .map(|a| a.terminal_font_bold)
        .collect();
    assert_eq!(
        bold,
        [Some(true), Some(false), None],
        "on, off, cleared; a second reset has nothing to clear"
    );
    assert_eq!(hint(&mut h, "bold"), "Resolved: normal from built-in");
}

#[gpui::test]
fn font_filter_narrows_the_list_and_a_pick_sends_the_family(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = single_in_repo(AppearanceOverrides::default(), repo("r1", "C:/repos/r1"));
    let mut h = focused(cx, &dir, &fixture);
    open_session_editor(&mut h, "s1");

    let all = family_names(&mut h);
    for bundled in ["Geist Mono", "Fira Code", "JetBrains Mono", "Cascadia Code"] {
        assert!(
            all.iter().any(|name| name == bundled),
            "{bundled} in {all:?}"
        );
    }
    assert!(
        all.iter().all(|name| !name.starts_with('.')),
        "no hidden system faces"
    );

    h.click_on("appearance-family-filter");
    type_text(&mut h, "FIRA");
    let narrowed = family_names(&mut h);
    assert!(narrowed.iter().any(|name| name == "Fira Code"));
    assert!(
        narrowed
            .iter()
            .all(|name| name.to_lowercase().contains("fira")),
        "only matches: {narrowed:?}"
    );

    h.click_on("appearance-family-option-Fira Code");
    let families: Vec<Option<String>> = session_appearances(&h.sent(), "s1")
        .into_iter()
        .map(|a| a.terminal_font_family)
        .collect();
    assert_eq!(families, [Some("Fira Code".to_owned())]);
    assert_eq!(hint(&mut h, "family"), "Resolved: Fira Code from session");

    h.click_on("appearance-family-reset");
    let families: Vec<Option<String>> = session_appearances(&h.sent(), "s1")
        .into_iter()
        .map(|a| a.terminal_font_family)
        .collect();
    assert_eq!(families, [None], "the reset row clears it");
}

#[gpui::test]
fn ctrl_comma_opens_settings_and_app_level_changes_persist_locally_without_a_message(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let fixture = single_in_repo(AppearanceOverrides::default(), repo("r1", "C:/repos/r1"));
    let mut h = focused(cx, &dir, &fixture);

    h.keys("ctrl-,");
    assert!(h.root(|root, _| root.settings_open()));
    assert_eq!(editor_title(&mut h).as_deref(), Some("Settings"));
    assert_eq!(
        h.root(|root, _| root.appearance_reset_label()),
        Some("Use built-in")
    );

    h.click_on("appearance-accent-preset-sky");
    h.click_on("appearance-size-up");
    h.click_on("appearance-bold");

    assert_eq!(appearance_sends(&h.sent()), 0, "the app level stays here");
    assert_eq!(
        h.root(|root, _| root.session_accent("s1")),
        Some(0x38bdf8),
        "the session inherits the app's accent"
    );
    assert_eq!(hint(&mut h, "accent"), "Resolved: #38bdf8 from current");
    assert_eq!(hint(&mut h, "size"), "Resolved: 14px from app");
    assert_eq!(hint(&mut h, "bold"), "Resolved: bold from app");
    let saved = saved_ui(&dir);
    assert_eq!(saved["app_appearance"]["accent_color"], "#38bdf8");
    assert_eq!(saved["app_appearance"]["terminal_frame_color"], "#38bdf8");
    assert_eq!(saved["terminal_font"]["size"], 14.0);
    assert_eq!(saved["terminal_font"]["bold"], true);

    h.click_on("appearance-size-reset");
    h.click_on("appearance-accent-reset");
    assert_eq!(hint(&mut h, "size"), "Resolved: 13px from built-in");
    assert_eq!(hint(&mut h, "accent"), "Resolved: #5b9bff from built-in");
    let saved = saved_ui(&dir);
    assert_eq!(saved["terminal_font"]["size"], 13.0);
    assert!(saved["app_appearance"]["accent_color"].is_null());
    assert_eq!(appearance_sends(&h.sent()), 0);

    h.click_on("settings-footer-close");
    assert!(!h.root(|root, _| root.settings_open()));
    h.click_on("settings-open");
    assert!(
        h.root(|root, _| root.settings_open()),
        "the gear opens it too"
    );
}

#[gpui::test]
fn escape_and_close_dismiss(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = single_in_repo(AppearanceOverrides::default(), repo("r1", "C:/repos/r1"));
    let mut h = focused(cx, &dir, &fixture);

    open_session_editor(&mut h, "s1");
    h.keys("escape");
    assert!(editor_title(&mut h).is_none(), "Esc");

    open_session_editor(&mut h, "s1");
    h.click_on("appearance-close");
    assert!(editor_title(&mut h).is_none(), "the header's ×");

    open_session_editor(&mut h, "s1");
    h.click_on("appearance-footer-close");
    assert!(editor_title(&mut h).is_none(), "the footer's Close");

    h.keys("ctrl-,");
    h.keys("escape");
    assert!(
        !h.root(|root, _| root.settings_open()),
        "Esc closes Settings"
    );
    h.keys("ctrl-,");
    h.click_on("settings-close");
    assert!(
        !h.root(|root, _| root.settings_open()),
        "so does its header's ×"
    );
}

#[gpui::test]
fn the_editor_blocks_the_spawn_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
        ..single_in_repo(AppearanceOverrides::default(), repo("r1", "C:/repos/r1"))
    };
    let mut h = focused(cx, &dir, &fixture);

    open_session_editor(&mut h, "s1");
    h.keys("ctrl-shift-n");
    assert!(
        !h.root(|root, _| root.spawn_dialog_open()),
        "Ctrl+Shift+N does nothing under the editor"
    );
    h.keys("ctrl-,");
    assert_eq!(
        editor_title(&mut h).as_deref(),
        Some("Session appearance"),
        "Settings does not replace it"
    );
    h.click_on("sidebar-shell-dialog");
    assert!(
        !h.root(|root, _| root.shell_dialog_open()),
        "the backdrop takes the click"
    );

    h.keys("escape");
    h.keys("ctrl-shift-n");
    assert!(h.root(|root, _| root.spawn_dialog_open()));
    h.keys("ctrl-,");
    assert!(
        !h.root(|root, _| root.settings_open()),
        "the spawn dialog blocks Settings"
    );
}
