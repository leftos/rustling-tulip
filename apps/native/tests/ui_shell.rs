//! Shell specs: "+ Shell" spawns a plain shell in the remembered folder, or
//! in the user's home when there is none, in the tab on screen — with no
//! repo registered; the Shell… dialog seeds its folder field, opens only a
//! full path, remembers the folder once the spawn it came with lands, offers
//! to go back to the home folder, closes on Esc and on a lost connection but
//! not on a backdrop click; and a shell's toast says shell startup.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{Modifiers, TestAppContext, point, px};
use protocol::{
    AgentOptions, ClientMessage, DaemonMessage, SessionMode, SpawnRequest, SpawnTarget,
    SplitDirection,
};
use rustling_tulip_native::RootView;
use support::{Fixture, Harness, TestDir, pane, session, split, tab};

const SPAWNING_TITLE: &str = "Spawning session…";
const SHELL_DETAIL: &str = "Shell startup may take a few seconds.";
const PATH_HINT: &str = "Enter a full path, like C:\\Users\\you";

/// `s1` alone in pane `p1` of tab `t1`, and no repo at all.
fn fixture() -> Fixture {
    Fixture::single(session("s1").build())
}

fn is_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.shell_dialog_open())
}

fn focus(h: &mut Harness<'_>) -> Option<&'static str> {
    h.root(RootView::shell_dialog_focus)
}

fn folder(h: &mut Harness<'_>) -> Option<String> {
    h.root(RootView::shell_dialog_folder)
}

fn can_submit(h: &mut Harness<'_>) -> bool {
    h.root(RootView::shell_dialog_can_submit)
}

fn hint(h: &mut Harness<'_>) -> Option<&'static str> {
    h.root(RootView::shell_dialog_hint)
}

fn saves_default(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.shell_dialog_saves_default())
}

fn spawns(sent: &[ClientMessage]) -> Vec<SpawnRequest> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SpawnSession(request) => Some(request.clone()),
            _ => None,
        })
        .collect()
}

fn the_spawn(sent: &[ClientMessage]) -> SpawnRequest {
    match spawns(sent).as_slice() {
        [request] => request.clone(),
        other => panic!("expected one SpawnSession, sent {other:?}"),
    }
}

fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|t| (t.title.clone(), t.detail.clone()))
            .collect()
    })
}

/// Types `text` into the field that has the keyboard: the harness takes one
/// keystroke per token, so `C:/work` goes in as `C : / w o r k`.
fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
}

/// Replies to spawn `request` with a standalone shell session `new`.
fn reply(h: &mut Harness<'_>, request: &SpawnRequest) {
    h.send(DaemonMessage::SessionUpdated {
        session: session("new").shell("C:/work").build(),
        request_id: request.request_id.clone(),
    });
}

/// `native-ui.json` in the spec's ui dir, as it stands on disk.
fn saved_ui(dir: &TestDir) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("a saved layout");
    serde_json::from_str(&text).expect("native-ui.json is JSON")
}

/// Seeds `native-ui.json` with a remembered quick shell folder, before the
/// view loads it.
fn seed_ui(dir: &TestDir, quick_shell_dir: Option<&str>) {
    std::fs::write(
        dir.path().join("native-ui.json"),
        serde_json::json!({ "quick_shell_dir": quick_shell_dir }).to_string(),
    )
    .expect("seed native-ui.json");
}

#[gpui::test]
fn quick_shell_spawns_standalone_in_home_when_no_default(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-add-shell");

    let request = the_spawn(&h.sent());
    assert!(request.request_id.is_some());
    assert_eq!(
        request.target,
        SpawnTarget::Standalone { cwd: None },
        "no remembered folder: the daemon picks the home directory"
    );
    assert_eq!(request.mode, SessionMode::PlainShell);
    assert_eq!(
        request.agent_options,
        AgentOptions::Claude {
            permission_mode: None,
        }
    );
    assert_eq!(request.label, None);
    assert_eq!(request.initial_prompt, None);
    assert!(!request.dangerously_skip_permissions);
    assert_eq!(request.model, None);
    assert!(request.extra_env.is_empty());
    assert!(request.prompt_injector.is_none());
}

#[gpui::test]
fn quick_shell_uses_the_remembered_folder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, Some("C:\\work"));
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-add-shell");
    assert_eq!(
        the_spawn(&h.sent()).target,
        SpawnTarget::Standalone {
            cwd: Some("C:\\work".to_owned()),
        }
    );
}

#[gpui::test]
fn shell_dialog_submit_sends_standalone_spawn_and_saves_default(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-shell-dialog");
    assert!(is_open(&mut h), "the dialog opened");
    assert_eq!(
        folder(&mut h).as_deref(),
        Some(""),
        "nothing is remembered yet"
    );
    assert_eq!(
        focus(&mut h),
        Some("shell-folder"),
        "the field takes typing"
    );
    assert!(saves_default(&mut h), "the box starts ticked");
    assert!(!can_submit(&mut h), "a blank folder cannot go");
    assert_eq!(hint(&mut h), None, "and asks for nothing");

    type_text(&mut h, "C:/work");
    assert_eq!(folder(&mut h).as_deref(), Some("C:/work"));
    assert!(can_submit(&mut h), "a full path is there to open");

    h.click_on("shell-submit");
    assert!(!is_open(&mut h), "a submit closes the dialog");
    let request = the_spawn(&h.sent());
    assert!(request.request_id.is_some());
    assert_eq!(
        request.target,
        SpawnTarget::Standalone {
            cwd: Some("C:/work".to_owned()),
        }
    );
    assert!(
        saved_ui(&dir)["quick_shell_dir"].is_null(),
        "not yet: the spawn must land first"
    );

    reply(&mut h, &request);
    assert_eq!(
        saved_ui(&dir)["quick_shell_dir"].as_str(),
        Some("C:/work"),
        "the shell landed, so its folder is remembered"
    );
}

#[gpui::test]
fn successful_shell_spawn_saves_default(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-shell-dialog");
    type_text(&mut h, "C:/work");
    h.click_on("shell-submit");
    let request = the_spawn(&h.sent());
    assert!(
        saved_ui(&dir)["quick_shell_dir"].is_null(),
        "a spawn on its way remembers nothing"
    );

    reply(&mut h, &request);
    assert_eq!(saved_ui(&dir)["quick_shell_dir"].as_str(), Some("C:/work"));
}

#[gpui::test]
fn failed_shell_spawn_does_not_save_default(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-shell-dialog");
    type_text(&mut h, "C:/work");
    h.click_on("shell-submit");
    let request = the_spawn(&h.sent());
    h.send(DaemonMessage::Error {
        message: "claude is not installed".to_owned(),
        request_id: request.request_id.clone(),
    });
    assert!(
        saved_ui(&dir)["quick_shell_dir"].is_null(),
        "a spawn the daemon refused remembers nothing"
    );
    drop(h);

    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-shell-dialog");
    type_text(&mut h, "C:/work");
    h.click_on("shell-submit");
    let request = the_spawn(&h.sent());
    h.send(DaemonMessage::ActionFailed {
        title: "Spawn failed".to_owned(),
        detail: "the folder is gone".to_owned(),
        hint: None,
        request_id: request.request_id.clone(),
    });
    assert!(
        saved_ui(&dir)["quick_shell_dir"].is_null(),
        "nor does one whose action failed"
    );
}

#[gpui::test]
fn relative_folder_cannot_submit_and_shows_hint(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-shell-dialog");
    type_text(&mut h, "work");
    assert_eq!(folder(&mut h).as_deref(), Some("work"));
    assert!(!can_submit(&mut h), "a relative folder opens nothing");
    assert_eq!(hint(&mut h), Some(PATH_HINT), "and asks for a full path");

    h.click_on("shell-submit");
    assert!(is_open(&mut h), "nothing to open");
    assert!(spawns(&h.sent()).is_empty());

    h.keys("escape");
    h.click_on("sidebar-shell-dialog");
    assert_eq!(focus(&mut h), Some("shell-folder"), "the field anew");
    type_text(&mut h, "C:/work");
    assert!(can_submit(&mut h), "a drive path can go");
    assert_eq!(hint(&mut h), None, "and asks for nothing");
    h.click_on("shell-submit");
    assert!(!is_open(&mut h));
    assert_eq!(
        the_spawn(&h.sent()).target,
        SpawnTarget::Standalone {
            cwd: Some("C:/work".to_owned()),
        }
    );
}

#[gpui::test]
fn clear_default_returns_plus_shell_to_home(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, Some("C:\\work"));
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-shell-dialog");
    assert!(
        h.in_model("shell-clear-default"),
        "the link shows with a remembered folder"
    );

    h.click_on("shell-clear-default");
    assert!(!is_open(&mut h), "clearing closes the dialog");
    assert!(spawns(&h.sent()).is_empty(), "and spawns nothing");
    assert!(
        saved_ui(&dir)["quick_shell_dir"].is_null(),
        "the remembered folder is gone"
    );

    h.click_on("sidebar-add-shell");
    assert_eq!(
        the_spawn(&h.sent()).target,
        SpawnTarget::Standalone { cwd: None },
        "+ Shell goes to the home folder again"
    );
}

#[gpui::test]
fn clicking_the_checkbox_then_space_toggles_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-shell-dialog");
    assert!(saves_default(&mut h));

    h.click_on("shell-save-default");
    assert!(!saves_default(&mut h), "the click toggles the box");
    assert_eq!(
        focus(&mut h),
        Some("shell-save-default"),
        "and the keyboard moves to it"
    );

    h.keys("space");
    assert!(saves_default(&mut h), "Space toggles the focused box");
    assert_eq!(
        folder(&mut h).as_deref(),
        Some(""),
        "Space typed nothing into the folder"
    );
}

#[gpui::test]
fn shell_dialog_unticked_does_not_save_default(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, Some("C:\\old"));
    let mut h = Harness::with(cx, &dir, &fixture());
    assert_eq!(
        saved_ui(&dir)["quick_shell_dir"].as_str(),
        Some("C:\\old"),
        "the seed stands"
    );

    h.click_on("sidebar-shell-dialog");
    assert_eq!(
        folder(&mut h).as_deref(),
        Some("C:\\old"),
        "the field is seeded with the remembered folder"
    );
    type_text(&mut h, "C:/work");
    assert_eq!(
        folder(&mut h).as_deref(),
        Some("C:/work"),
        "typing replaces the seeded text"
    );
    h.click_on("shell-save-default");
    assert!(!saves_default(&mut h), "unticked");

    h.click_on("shell-submit");
    let request = the_spawn(&h.sent());
    reply(&mut h, &request);
    assert_eq!(
        saved_ui(&dir)["quick_shell_dir"].as_str(),
        Some("C:\\old"),
        "the box was unticked, so the folder is left alone even when the shell lands"
    );
}

#[gpui::test]
fn shell_dialog_blank_folder_cannot_submit(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-shell-dialog");
    assert!(!can_submit(&mut h));
    assert_eq!(hint(&mut h), None, "an empty field asks for nothing");

    h.click_on("shell-submit");
    assert!(is_open(&mut h), "a blank folder has nothing to open");
    assert!(spawns(&h.sent()).is_empty());
}

#[gpui::test]
fn shell_dialog_escape_closes_and_backdrop_does_not(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-shell-dialog");
    let backdrop = h.bounds("shell-dialog");
    h.click(
        point(backdrop.origin.x + px(4.0), backdrop.origin.y + px(4.0)),
        Modifiers::none(),
    );
    assert!(is_open(&mut h), "a backdrop click keeps the dialog");

    h.keys("tab");
    assert_eq!(focus(&mut h), Some("shell-browse"), "Tab moves the focus");
    h.keys("escape");
    assert!(!is_open(&mut h), "Esc on a button closes");

    h.click_on("sidebar-shell-dialog");
    assert_eq!(focus(&mut h), Some("shell-folder"));
    h.keys("escape");
    assert!(!is_open(&mut h), "Esc in the folder field closes");
    assert!(spawns(&h.sent()).is_empty());
}

#[gpui::test]
fn shell_dialog_closes_on_reconnect(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-shell-dialog");
    assert!(is_open(&mut h));
    h.lose_connection();
    assert!(!is_open(&mut h));
}

#[gpui::test]
fn shell_buttons_work_without_repos(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-add-shell");
    assert_eq!(
        the_spawn(&h.sent()).target,
        SpawnTarget::Standalone { cwd: None },
        "a shell needs no repo"
    );

    h.click_on("sidebar-shell-dialog");
    assert!(is_open(&mut h), "the dialog opens with no repo either");
}

#[gpui::test]
fn shell_reply_is_placed_in_the_current_tab_and_focused(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", None),
    );
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(h.root(|root, _| root.focused_pane()).as_deref(), Some("p1"));

    h.click_on("sidebar-add-shell");
    let request = the_spawn(&h.sent());
    reply(&mut h, &request);
    let placed = h.sent();
    assert!(
        placed.iter().any(|m| matches!(
            m,
            ClientMessage::ReplacePaneSession { tab_id, pane_id, session_id: Some(id) }
                if tab_id == "t1" && pane_id == "p2" && id == "new"
        )),
        "the empty pane of the current tab: {placed:?}"
    );
    assert_eq!(
        h.root(|root, _| root.focused_pane()).as_deref(),
        Some("p2"),
        "the new pane has the keyboard"
    );
}

#[gpui::test]
fn shell_spawn_toast_says_shell_startup(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-add-shell");
    assert_eq!(
        toasts(&mut h),
        [(SPAWNING_TITLE.to_owned(), Some(SHELL_DETAIL.to_owned()))]
    );
}
