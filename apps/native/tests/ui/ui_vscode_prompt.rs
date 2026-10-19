//! The "VS Code workspace detected" prompt: a daemon push opens a centred
//! modal naming the file, its folders and the suggested workspace; Not now
//! dismisses without a message, Create accepts without watching, Create &
//! watch accepts with watching, Esc dismisses, and several pushes queue with
//! one prompt on screen at a time.

#![expect(
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{TestAppContext, px};
use protocol::{ClientMessage, DaemonMessage, VscodeWorkspaceFolder, VscodeWorkspaceSuggestion};
use support::{Fixture, Harness, TestDir, repo, session};

/// Repo `r1` with `s1` live in it, in pane `p1` of tab `t1`.
fn fixture() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "C:/r1"), repo("r2", "C:/r2")];
    fixture
}

fn suggestion(name: &str) -> VscodeWorkspaceSuggestion {
    VscodeWorkspaceSuggestion {
        source_path: "C:/work/team.code-workspace".to_owned(),
        suggested_name: name.to_owned(),
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

fn push(h: &mut Harness<'_>, name: &str) {
    h.send(DaemonMessage::VscodeWorkspaceSuggestion {
        repo_id: "r1".to_owned(),
        suggestion: suggestion(name),
    });
}

fn is_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.vscode_prompt_open())
}

fn focus(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.vscode_prompt_focus())
}

#[gpui::test]
fn a_push_opens_the_modal_with_its_body_and_badges(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();
    assert!(!is_open(&mut h));

    push(&mut h, "team");
    assert!(is_open(&mut h), "the push opens the prompt");
    assert_eq!(
        h.root(|root, _| root.vscode_prompt_title()).as_deref(),
        Some("VS Code workspace detected")
    );
    let body = h
        .root(|root, _| root.vscode_prompt_body())
        .expect("the prompt has a body");
    assert_eq!(
        body,
        "C:/work/team.code-workspace references 2 folders. Create a rustling-tulip workspace named team with these members?"
    );
    let parts = h.root(|root, _| root.vscode_prompt_body_parts());
    assert_eq!(
        parts,
        [
            ("C:/work/team.code-workspace".to_owned(), "code"),
            (
                " references 2 folders. Create a rustling-tulip workspace named ".to_owned(),
                "text"
            ),
            ("team".to_owned(), "bold"),
            (" with these members?".to_owned(), "text"),
        ],
        "the drawn parts: the path as code, the name bold, the sentence plain"
    );
    assert_eq!(
        parts
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<String>(),
        body,
        "body() is exactly the drawn parts joined, so a reword cannot diverge silently"
    );
    assert_eq!(
        h.root(|root, _| root.vscode_prompt_folders()),
        [
            ("r1".to_owned(), "registered".to_owned()),
            ("r2".to_owned(), "will register".to_owned()),
        ]
    );
    assert_eq!(
        h.root(|root, _| root.vscode_prompt_buttons()),
        [
            ("vscode-prompt-not-now".to_owned(), "Not now".to_owned()),
            ("vscode-prompt-create".to_owned(), "Create".to_owned()),
            (
                "vscode-prompt-create-watch".to_owned(),
                "Create & watch".to_owned()
            ),
        ]
    );
    assert_eq!(focus(&mut h).as_deref(), Some("vscode-prompt-not-now"));
    assert!(
        h.bounds("vscode-prompt-body").origin.x >= px(0.0),
        "the model's body text is what the prompt draws"
    );
    assert!(h.sent().is_empty(), "opening sends nothing");
}

#[gpui::test]
fn the_dismiss_button_dismisses_without_a_message(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();
    push(&mut h, "team");

    h.click_on("vscode-prompt-dismiss");
    assert!(!is_open(&mut h), "the ✕ dismisses the prompt");
    assert!(h.sent().is_empty(), "and sends nothing");
}

#[gpui::test]
fn dismissing_over_the_creator_hands_it_back_the_keyboard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.answer_scrollback("s1", b"");
    h.sent();
    h.click_on("sidebar-more");
    h.click_on("sidebar-more-add-workspace");
    assert!(h.root(|root, _| root.workspace_creator_open()));

    push(&mut h, "team");
    h.click_on("vscode-prompt-not-now");
    assert!(
        h.root(|root, _| root.workspace_creator_open()),
        "the creator stays open under the prompt"
    );

    h.keys("enter");
    assert!(
        h.sent_input("s1").is_empty(),
        "Enter presses the creator's focused control, not the pane"
    );
    assert!(h.root(|root, _| root.workspace_creator_open()));
}

#[gpui::test]
fn not_now_dismisses_without_a_message(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();
    push(&mut h, "team");

    h.click_on("vscode-prompt-not-now");
    assert!(!is_open(&mut h), "Not now dismisses the prompt");
    assert!(h.sent().is_empty(), "and sends nothing");
}

#[gpui::test]
fn create_accepts_without_watching(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();
    push(&mut h, "team");

    h.click_on("vscode-prompt-create");
    let sent = h.sent();
    match sent.as_slice() {
        [
            ClientMessage::AcceptVscodeWorkspaceSuggestion {
                suggestion: accepted,
                watch,
            },
        ] => {
            assert!(!watch, "Create does not watch");
            assert_eq!(accepted, &suggestion("team"));
        }
        other => panic!("expected one accepted suggestion, sent {other:?}"),
    }
    assert!(!is_open(&mut h), "the answer dismisses the prompt");
}

#[gpui::test]
fn create_and_watch_accepts_with_watching(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();
    push(&mut h, "team");

    h.click_on("vscode-prompt-create-watch");
    let sent = h.sent();
    match sent.as_slice() {
        [
            ClientMessage::AcceptVscodeWorkspaceSuggestion {
                suggestion: accepted,
                watch,
            },
        ] => {
            assert!(watch, "Create & watch sets the flag");
            assert_eq!(accepted, &suggestion("team"));
        }
        other => panic!("expected one accepted suggestion, sent {other:?}"),
    }
    assert!(!is_open(&mut h));
}

#[gpui::test]
fn tab_moves_the_focus_and_enter_presses_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();
    push(&mut h, "team");

    h.keys("tab");
    assert_eq!(focus(&mut h).as_deref(), Some("vscode-prompt-create"));
    h.keys("tab");
    assert_eq!(focus(&mut h).as_deref(), Some("vscode-prompt-create-watch"));

    h.keys("enter");
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::AcceptVscodeWorkspaceSuggestion { watch: true, .. }]
        ),
        "Enter presses the focused button: {sent:?}"
    );
}

#[gpui::test]
fn two_pushes_queue_and_show_one_at_a_time(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();

    push(&mut h, "first");
    push(&mut h, "second");
    assert_eq!(
        h.root(|root, _| root.vscode_prompt_queued()),
        2,
        "both are queued"
    );
    assert_eq!(
        h.root(|root, _| root.vscode_prompt_body())
            .as_deref()
            .map(|body| body.contains("named first")),
        Some(true),
        "the first push is the one on screen"
    );

    h.keys("escape");
    assert!(is_open(&mut h), "the next one takes its place");
    assert_eq!(
        h.root(|root, _| root.vscode_prompt_body())
            .as_deref()
            .map(|body| body.contains("named second")),
        Some(true)
    );
    assert_eq!(focus(&mut h).as_deref(), Some("vscode-prompt-not-now"));

    h.keys("escape");
    assert!(
        !is_open(&mut h),
        "dismissing the last one closes the prompt"
    );
}
