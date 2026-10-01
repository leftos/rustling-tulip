//! Repo specs: Add repo in the sessions toolbar's ⋯ menu registers the
//! folder the picker returns and opens the next pick beside it; with no repo
//! or workspace, the sidebar and the main area offer Add repo.

use crate::support;

use std::path::PathBuf;

use gpui::{TestAppContext, px};
use protocol::{ClientMessage, DaemonMessage};
use support::{Fixture, Harness, TestDir, repo, session};

/// Repo `r1`; session `s1` of `r1` alone in pane `p1` of tab `t1`.
fn fixture() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "C:/r1")];
    fixture
}

/// The folder and name of every `AddRepo` in `sent`.
fn added(sent: &[ClientMessage]) -> Vec<(String, Option<String>)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::AddRepo { path, name } => Some((path.clone(), name.clone())),
            _ => None,
        })
        .collect()
}

fn one_add(path: &str, name: &str) -> Vec<(String, Option<String>)> {
    vec![(path.to_owned(), Some(name.to_owned()))]
}

/// Whether the model shows what `selector` tags and it was painted; gpui
/// keeps an element's bounds after it leaves, so the model decides.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.in_model(selector) && h.bounds(selector).origin.x >= px(0.0)
}

fn add_from_more(h: &mut Harness<'_>) {
    h.click_on("sidebar-more");
    h.click_on("sidebar-more-add-repo");
}

#[gpui::test]
fn add_repo_from_the_more_menu_sends_add_repo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();

    h.set_picked_folder(None);
    add_from_more(&mut h);
    assert_eq!(h.folder_asks(), 1, "the row asks the picker");
    assert!(
        !h.root(|root, _| root.sidebar_more_open()),
        "the menu closes"
    );
    assert!(added(&h.sent()).is_empty(), "a cancel sends nothing");

    h.set_picked_folder(Some("C:/src/tulip"));
    add_from_more(&mut h);
    assert_eq!(added(&h.sent()), one_add("C:/src/tulip", "tulip"));
}

#[gpui::test]
fn the_picker_opens_at_the_last_repo_folder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.set_picked_folder(Some("C:/src/tulip"));
    add_from_more(&mut h);
    h.set_picked_folder(None);
    add_from_more(&mut h);
    h.set_picked_folder(Some("D:/other/repo"));
    add_from_more(&mut h);
    assert_eq!(
        h.folder_starts(),
        vec![
            None,
            Some(PathBuf::from("C:/src")),
            Some(PathBuf::from("C:/src")),
        ],
        "the first pick opens at the picker's own folder, the next beside the last added; a cancel keeps it"
    );
    let saved = std::fs::read_to_string(dir.path().join("native-ui.json")).unwrap_or_default();
    assert!(
        saved.contains("\"last_repo_dir\": \"D:/other\""),
        "the folder is kept in native-ui.json: {saved}"
    );
}

#[gpui::test]
fn the_empty_sidebar_offers_add_repo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    assert!(drawn(&mut h, "sidebar-no-repos"), "no repo or workspace");
    assert!(
        drawn(&mut h, "leaf-s1"),
        "a session tied to no repo still lists"
    );
    h.set_picked_folder(Some("C:/src/tulip"));
    h.click_on("sidebar-add-repo");
    assert_eq!(added(&h.sent()), one_add("C:/src/tulip", "tulip"));

    h.send(DaemonMessage::Repos {
        repos: vec![repo("tulip", "C:/src/tulip")],
    });
    assert!(
        !drawn(&mut h, "sidebar-no-repos"),
        "a registered repo ends the note"
    );
}

#[gpui::test]
fn the_empty_main_area_offers_add_repo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    assert!(drawn(&mut h, "empty-spawn-session"));
    assert!(drawn(&mut h, "empty-open-shell"));
    assert!(drawn(&mut h, "empty-add-repo"), "beside them, with no repo");
    h.set_picked_folder(Some("D:/work/repo"));
    h.click_on("empty-add-repo");
    assert_eq!(added(&h.sent()), one_add("D:/work/repo", "repo"));

    h.send(DaemonMessage::Repos {
        repos: vec![repo("repo", "D:/work/repo")],
    });
    assert!(
        !drawn(&mut h, "empty-add-repo"),
        "with a repo the main area offers no Add repo"
    );
}
