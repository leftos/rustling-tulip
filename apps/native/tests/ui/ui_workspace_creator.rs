//! Workspace creator specs: the toolbar's "Add workspace…" row opens it (and
//! does nothing while fewer than two repos exist); "From repos" needs a name
//! and two members, sends an `UpsertWorkspace` and stays open until the
//! daemon's workspace list holds the name; a refusal leaves the form intact
//! with its error inline; "From VS Code workspace" browses for a file, shows
//! the parsed folders with their badges and accepts the suggestion.

#![expect(
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::path::PathBuf;

use gpui::{TestAppContext, px};
use protocol::{ClientMessage, DaemonMessage, VscodeWorkspaceFolder, VscodeWorkspaceSuggestion};
use support::{Fixture, Harness, TestDir, pane, repo, tab, workspace};

/// `repos` registered, in a single empty tab.
fn fixture(repos: Vec<protocol::RepoEntry>) -> Fixture {
    Fixture {
        repos,
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    }
}

fn two_repos() -> Fixture {
    fixture(vec![repo("r1", "C:/r1"), repo("r2", "C:/r2")])
}

fn one_repo() -> Fixture {
    fixture(vec![repo("r1", "C:/r1")])
}

fn open(h: &mut Harness<'_>) {
    h.click_on("sidebar-more");
    h.click_on("sidebar-more-add-workspace");
}

fn is_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.workspace_creator_open())
}

fn can_submit(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.workspace_creator_can_submit())
}

fn name(h: &mut Harness<'_>) -> Option<String> {
    h.root(rustling_tulip_native::RootView::workspace_creator_name)
}

fn error(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.workspace_creator_error())
}

fn toasts(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| toast.title.clone())
            .collect()
    })
}

/// Types `text` into the name field that has the keyboard: the harness takes
/// one keystroke per token, so `team` goes in as `t e a m`.
fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
}

fn suggestion() -> VscodeWorkspaceSuggestion {
    VscodeWorkspaceSuggestion {
        source_path: "C:/work/team.code-workspace".to_owned(),
        suggested_name: "team".to_owned(),
        folders: vec![
            VscodeWorkspaceFolder {
                path: "C:/work/r1".to_owned(),
                name: None,
                matched_repo_id: Some("r1".to_owned()),
            },
            VscodeWorkspaceFolder {
                path: "C:/work/r2".to_owned(),
                name: None,
                matched_repo_id: None,
            },
        ],
    }
}

#[gpui::test]
fn the_menu_row_opens_the_creator(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.sent();
    assert!(!is_open(&mut h));

    h.click_on("sidebar-more");
    assert!(
        h.bounds("sidebar-more-add-workspace-hint").origin.x < px(0.0),
        "with two repos the row is enabled and draws no hint"
    );
    h.click_on("sidebar-more-add-workspace");
    assert!(is_open(&mut h), "Add workspace… opens the creator");
    assert!(
        !h.root(|root, _| root.sidebar_more_open()),
        "the menu closes"
    );
    assert_eq!(
        h.root(|root, _| root.workspace_creator_title()).as_deref(),
        Some("New workspace")
    );
    assert_eq!(
        h.root(|root, _| root.workspace_creator_tabs()),
        [
            (
                "workspace-creator-tab-repos".to_owned(),
                "From repos".to_owned()
            ),
            (
                "workspace-creator-tab-vscode".to_owned(),
                "From VS Code workspace".to_owned()
            ),
        ],
        "the tabs read left to right"
    );
    assert_eq!(
        h.root(|root, _| root.workspace_creator_mode()),
        Some("workspace-creator-tab-repos"),
        "From repos opens first"
    );

    h.click_on("workspace-creator-cancel");
    assert!(!is_open(&mut h), "Cancel closes it");
    assert!(h.sent().is_empty(), "opening and closing send nothing");

    open(&mut h);
    h.keys("escape");
    assert!(!is_open(&mut h), "Esc closes it too");
    assert!(h.sent().is_empty());
}

#[gpui::test]
fn the_disabled_row_does_nothing_with_one_repo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &one_repo());

    h.click_on("sidebar-more");
    assert!(
        h.bounds("sidebar-more-add-workspace").origin.x >= px(0.0),
        "the row is drawn while the menu is open"
    );
    assert!(
        h.bounds("sidebar-more-add-workspace-hint").origin.x >= px(0.0),
        "and it says why it is disabled: needs 2 repos"
    );
    h.click_on("sidebar-more-add-workspace");
    assert!(
        !is_open(&mut h),
        "with fewer than two repos the row opens nothing"
    );
    assert!(
        h.root(|root, _| root.sidebar_more_open()),
        "and it does not close the menu either"
    );
}

#[gpui::test]
fn from_repos_sends_the_form_and_waits_for_the_daemon(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.sent();
    open(&mut h);
    assert_eq!(
        h.root(|root, _| root.workspace_creator_focus()).as_deref(),
        Some("workspace-creator-tab-repos"),
        "the first tab has the keyboard"
    );
    assert!(!can_submit(&mut h), "nothing typed and nothing ticked");

    h.click_on("workspace-creator-name");
    type_text(&mut h, "team");
    assert_eq!(name(&mut h).as_deref(), Some("team"));
    assert!(!can_submit(&mut h), "a name alone is not enough");

    h.click_on("workspace-creator-member-r1");
    assert!(!can_submit(&mut h), "one member is not enough");
    h.click_on("workspace-creator-member-r2");
    assert!(can_submit(&mut h), "two members and a name");
    assert_eq!(
        h.root(|root, _| root.workspace_creator_members()),
        [
            (
                "workspace-creator-member-r1".to_owned(),
                "r1".to_owned(),
                true
            ),
            (
                "workspace-creator-member-r2".to_owned(),
                "r2".to_owned(),
                true
            ),
        ]
    );

    h.click_on("workspace-creator-create");
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::UpsertWorkspace {
                id: None,
                name,
                member_repo_ids,
                linked_vscode_workspace: None,
            }] if name == "team"
                && member_repo_ids == &["r1".to_owned(), "r2".to_owned()]
        ),
        "the form goes out as an UpsertWorkspace: {sent:?}"
    );
    assert!(
        is_open(&mut h),
        "Create keeps the creator open until the daemon's list holds the name"
    );
    assert!(!can_submit(&mut h), "the form blocks a second submit");
    h.click_on("workspace-creator-create");
    assert!(
        h.sent().is_empty(),
        "a second Create while the first waits sends nothing"
    );

    h.send(DaemonMessage::Workspaces {
        workspaces: vec![workspace("other", &["r1"])],
    });
    assert!(is_open(&mut h), "another workspace does not close it");

    h.send(DaemonMessage::Workspaces {
        workspaces: vec![workspace("Team", &["r1", "r2"])],
    });
    assert!(
        !is_open(&mut h),
        "the trimmed, case-insensitive name arriving closes it"
    );
}

#[gpui::test]
fn a_refused_create_stays_open_with_its_form_and_error(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.sent();
    open(&mut h);
    h.click_on("workspace-creator-name");
    type_text(&mut h, "taken");
    h.click_on("workspace-creator-member-r1");
    h.click_on("workspace-creator-member-r2");
    h.click_on("workspace-creator-create");
    h.sent();

    h.send(DaemonMessage::Error {
        message: "another request failed".to_owned(),
        request_id: Some("other".to_owned()),
        code: None,
    });
    assert!(is_open(&mut h), "a routed error answers its own request");
    assert_eq!(
        error(&mut h),
        None,
        "a routed error is not the create's refusal"
    );
    assert_eq!(
        toasts(&mut h),
        ["Daemon error"],
        "and it takes the generic path"
    );

    h.send(DaemonMessage::Error {
        message: "another workspace already uses the name \"taken\"".to_owned(),
        request_id: None,
        code: None,
    });
    assert!(is_open(&mut h), "a refusal keeps the creator open");
    assert_eq!(
        error(&mut h).as_deref(),
        Some("another workspace already uses the name \"taken\""),
        "the daemon's message shows inline"
    );
    assert_eq!(
        name(&mut h).as_deref(),
        Some("taken"),
        "the typed name survives"
    );
    assert_eq!(
        h.root(|root, _| root.workspace_creator_members())
            .iter()
            .filter(|(_, _, checked)| *checked)
            .count(),
        2,
        "the ticks survive"
    );
    assert_eq!(
        toasts(&mut h),
        ["Daemon error"],
        "the unrouted refusal adds no second toast: only the earlier routed one"
    );
}

#[gpui::test]
fn from_vscode_browses_parses_and_accepts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{"last_repo_dir":"C:/src"}"#,
    )
    .expect("seed the last repo folder");
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.sent();
    open(&mut h);
    h.click_on("workspace-creator-tab-vscode");
    assert_eq!(
        h.root(|root, _| root.workspace_creator_mode()),
        Some("workspace-creator-tab-vscode")
    );

    h.set_picked_file(Some("C:/work/team.code-workspace"));
    h.click_on("workspace-creator-browse");
    assert_eq!(h.file_asks(), 1, "Browse asks the file picker");
    assert_eq!(
        h.file_starts(),
        vec![Some(PathBuf::from("C:/src"))],
        "with no folder of its own, the picker opens at the last repo folder"
    );
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::ParseVscodeWorkspace { path }]
                if path == "C:/work/team.code-workspace"
        ),
        "the picked file goes out to be parsed: {sent:?}"
    );
    assert!(h.root(|root, _| root.workspace_creator_parsing()));

    h.send(DaemonMessage::VscodeWorkspaceParsed {
        suggestion: suggestion(),
    });
    assert!(!h.root(|root, _| root.workspace_creator_parsing()));
    assert_eq!(
        name(&mut h).as_deref(),
        Some("team"),
        "the file's stem prefills the name"
    );
    assert_eq!(
        h.root(|root, _| root.workspace_creator_folder_rows()),
        [
            ("r1".to_owned(), "registered".to_owned()),
            ("r2".to_owned(), "will register".to_owned()),
        ]
    );
    assert!(can_submit(&mut h), "a parsed file and a name can submit");

    h.click_on("workspace-creator-create");
    match h.sent().as_slice() {
        [
            ClientMessage::AcceptVscodeWorkspaceSuggestion {
                suggestion: accepted,
                watch,
            },
        ] => {
            assert!(!watch, "the creator's Create does not watch");
            assert_eq!(accepted.suggested_name, "team");
            assert_eq!(accepted.source_path, "C:/work/team.code-workspace");
            assert_eq!(accepted.folders.len(), 2);
        }
        other => panic!("expected an accepted suggestion, sent {other:?}"),
    }
    assert!(is_open(&mut h), "it waits for the workspace to arrive");
}

#[gpui::test]
fn browse_remembers_its_own_folder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{"last_repo_dir":"C:/src","last_vscode_dir":"D:/vs"}"#,
    )
    .expect("seed the picker folders");
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.sent();
    open(&mut h);
    h.click_on("workspace-creator-tab-vscode");

    h.set_picked_file(Some("C:/work/team.code-workspace"));
    h.click_on("workspace-creator-browse");
    assert_eq!(
        h.file_starts(),
        vec![Some(PathBuf::from("D:/vs"))],
        "the file picker opens at its own remembered folder, not the repo one"
    );

    let saved = std::fs::read_to_string(dir.path().join("native-ui.json"))
        .expect("the UI state is written");
    assert!(
        saved.contains("\"last_vscode_dir\": \"C:/work\""),
        "the picked file's parent is kept for the next browse: {saved}"
    );
}

#[gpui::test]
fn a_parse_failure_shows_inline_and_leaves_the_form_usable(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_repos());
    h.sent();
    open(&mut h);
    h.click_on("workspace-creator-tab-vscode");
    h.set_picked_file(Some("C:/work/broken.code-workspace"));
    h.click_on("workspace-creator-browse");
    h.sent();

    h.send(DaemonMessage::Error {
        message: "another request failed".to_owned(),
        request_id: Some("other".to_owned()),
        code: None,
    });
    assert!(
        h.root(|root, _| root.workspace_creator_parsing()),
        "a routed error leaves the parse pending"
    );
    assert_eq!(
        error(&mut h),
        None,
        "a routed error is not the parse's failure"
    );
    assert_eq!(toasts(&mut h), ["Daemon error"]);

    h.send(DaemonMessage::Error {
        message: "parsing C:/work/broken.code-workspace: expected value".to_owned(),
        request_id: None,
        code: None,
    });
    assert!(is_open(&mut h), "a failed parse keeps the form open");
    assert_eq!(
        error(&mut h).as_deref(),
        Some("parsing C:/work/broken.code-workspace: expected value")
    );
    assert!(!h.root(|root, _| root.workspace_creator_parsing()));
    assert_eq!(
        toasts(&mut h),
        ["Daemon error"],
        "the inline error adds no second toast"
    );

    // The form still browses: another file sends another parse.
    h.set_picked_file(Some("C:/work/team.code-workspace"));
    h.click_on("workspace-creator-browse");
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::ParseVscodeWorkspace { path }]
                if path == "C:/work/team.code-workspace"
        ),
        "the form is usable after the failure: {sent:?}"
    );

    // The first file's reply, arriving late, is not the current parse's.
    let mut stale = suggestion();
    stale.source_path = "C:/work/broken.code-workspace".to_owned();
    h.send(DaemonMessage::VscodeWorkspaceParsed { suggestion: stale });
    assert_eq!(
        h.root(|root, _| root.workspace_creator_folder_rows()),
        [],
        "the stale reply fills no rows"
    );
    assert!(
        h.root(|root, _| root.workspace_creator_parsing()),
        "and leaves the current parse pending"
    );

    h.send(DaemonMessage::VscodeWorkspaceParsed {
        suggestion: suggestion(),
    });
    assert_eq!(
        h.root(|root, _| root.workspace_creator_file()).as_deref(),
        Some("C:/work/team.code-workspace"),
        "the current file's reply is accepted"
    );
    assert_eq!(
        h.root(|root, _| root.workspace_creator_folder_rows()).len(),
        2,
        "with its folders"
    );
}
