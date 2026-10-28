//! The chat's reading of a session's rendered screen: the input box's state
//! and the prompt a dialog shows, as the session's agent plugin reads them,
//! and the keys that answer that prompt.
//!
//! The plugin parses the rows ([`agent_api::AgentPlugin::read_chat_screen`])
//! and builds the recipes ([`agent_api::AgentPlugin::answer_recipe`]); this
//! module hands it the grid's rows and gives each prompt the identity its id
//! is minted from, so a prompt keeps its id while only its cursor or a typed
//! field changes.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use agent_api::{AnswerRecipe, AnswerRefusal};
use protocol::{AgentId, ChatAnswerKind, ChatPrompt, ChatScreen};

use super::Screen;

/// What `screen` shows the chat, as `agent`'s plugin reads it; `None` for an
/// agent with no registered plugin or one whose screen the chat does not
/// mirror.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "no session watcher reads the chat screen yet")
)]
#[must_use]
pub fn read(agent: &AgentId, screen: &Screen) -> Option<ChatScreen> {
    let Some(plugin) = agent_registry::plugin(agent) else {
        tracing::debug!(
            "no agent plugin `{}` is registered; its screen has no chat reading",
            agent.0
        );
        return None;
    };
    plugin.read_chat_screen(&screen.rows())
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "no session watcher reads the chat screen yet")
)]
/// The keys that give `answer` to `prompt`, as `agent`'s plugin types them.
///
/// # Errors
///
/// Why the plugin has no recipe for this answer to this prompt;
/// [`AnswerRefusal::Unsupported`] for an agent with no registered plugin.
pub fn recipe_for(
    agent: &AgentId,
    prompt: &ChatPrompt,
    answer: &ChatAnswerKind,
) -> Result<AnswerRecipe, AnswerRefusal> {
    let Some(plugin) = agent_registry::plugin(agent) else {
        tracing::warn!(
            "no agent plugin `{}` is registered; refusing the answer",
            agent.0
        );
        return Err(AnswerRefusal::Unsupported);
    };
    plugin.answer_recipe(prompt, answer)
}

/// What makes `prompt` the same prompt across readings: its kind, subject,
/// body, its options' numbers in order and its question tabs' labels. The
/// cursor, which option is selected, the options' labels (a field being
/// typed into) and the id are left out. Stable within one process only.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "no session watcher reads the chat screen yet")
)]
#[must_use]
pub fn prompt_identity(prompt: &ChatPrompt) -> u64 {
    let mut hasher = DefaultHasher::new();
    std::mem::discriminant(&prompt.kind).hash(&mut hasher);
    prompt.subject.hash(&mut hasher);
    prompt.body.hash(&mut hasher);
    for option in &prompt.options {
        option.number.hash(&mut hasher);
    }
    for tab in &prompt.questions {
        tab.label.hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::screen::fixture_screen::screen_from;
    use agent_api::{Key, ScreenExpect};
    use protocol::{ChatPromptKind, ComposerState};

    /// The chat reading of a capture by Claude's plugin.
    fn read_capture(text: &str) -> ChatScreen {
        read(&agent_registry::default_agent(), &screen_from(text))
            .expect("Claude's plugin reads chat screens")
    }

    /// The prompt a capture shows.
    fn prompt_of(text: &str) -> ChatPrompt {
        read_capture(text)
            .prompt
            .expect("the capture shows a prompt")
    }

    /// What one capture must read as.
    #[derive(Debug)]
    struct Expected {
        composer: ComposerState,
        kind: Option<ChatPromptKind>,
        subject: &'static str,
        options: Vec<(u32, &'static str)>,
        cursor: Option<u32>,
        can_amend: bool,
        always_allow_tip: Option<&'static str>,
        /// Whether the options carry boxes.
        multi: bool,
        /// The ticked options of a multi-select question.
        ticked: Vec<u32>,
        /// The question tabs as `(label, answered, current)`.
        tabs: Vec<(&'static str, bool, bool)>,
        /// The body, when the row checks it.
        body: Option<&'static str>,
    }

    /// A screen with no prompt and the input box in `composer`.
    fn no_prompt(composer: ComposerState) -> Expected {
        Expected {
            composer,
            kind: None,
            subject: "",
            options: Vec::new(),
            cursor: None,
            can_amend: false,
            always_allow_tip: None,
            multi: false,
            ticked: Vec::new(),
            tabs: Vec::new(),
            body: None,
        }
    }

    /// An overlay named `name`.
    fn overlay(name: &str) -> Expected {
        no_prompt(ComposerState::Overlay {
            name: name.to_owned(),
        })
    }

    /// A dialog of `kind` that owns the keyboard.
    fn dialog(
        kind: ChatPromptKind,
        subject: &'static str,
        options: &[(u32, &'static str)],
        cursor: Option<u32>,
    ) -> Expected {
        Expected {
            composer: ComposerState::Dialog,
            kind: Some(kind),
            subject,
            options: options.to_vec(),
            cursor,
            ..no_prompt(ComposerState::Dialog)
        }
    }

    /// An `AskUserQuestion` step under `tabs`; on a multi-select step,
    /// `ticked` lists the ticked options.
    fn question(
        subject: &'static str,
        options: &[(u32, &'static str)],
        cursor: Option<u32>,
        ticked: Option<&[u32]>,
        tabs: &[(&'static str, bool, bool)],
    ) -> Expected {
        Expected {
            multi: ticked.is_some(),
            ticked: ticked.unwrap_or_default().to_vec(),
            tabs: tabs.to_vec(),
            ..dialog(ChatPromptKind::Question, subject, options, cursor)
        }
    }

    /// A permission dialog with the `Tab to amend` footer.
    fn permission(
        subject: &'static str,
        options: &[(u32, &'static str)],
        always_allow_tip: Option<&'static str>,
    ) -> Expected {
        Expected {
            can_amend: true,
            always_allow_tip,
            ..dialog(ChatPromptKind::Permission, subject, options, Some(1))
        }
    }

    /// The mismatch when `screen` does not read as `expected`.
    fn mismatch(screen: &ChatScreen, expected: &Expected) -> Option<String> {
        let prompt = screen.prompt.as_ref();
        let options: Vec<(u32, &str)> = prompt
            .map(|prompt| {
                prompt
                    .options
                    .iter()
                    .map(|option| (option.number, option.label.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let same = screen.composer == expected.composer
            && prompt.map(|prompt| prompt.kind.clone()) == expected.kind
            && prompt.and_then(|prompt| prompt.cursor) == expected.cursor
            && prompt.is_some_and(|prompt| prompt.can_amend) == expected.can_amend
            && prompt.map_or("", |prompt| prompt.subject.as_str()) == expected.subject
            && options == expected.options
            && prompt.and_then(|prompt| prompt.always_allow_tip.as_deref())
                == expected.always_allow_tip
            && prompt.is_some_and(|prompt| prompt.multi) == expected.multi
            && ticked(prompt) == expected.ticked
            && tabs(prompt) == expected.tabs
            && expected
                .body
                .is_none_or(|body| prompt.is_some_and(|prompt| prompt.body == body));
        (!same).then(|| format!("wanted {expected:?}, got {screen:?}"))
    }

    /// The ticked options of a multi-select prompt.
    fn ticked(prompt: Option<&ChatPrompt>) -> Vec<u32> {
        prompt
            .filter(|prompt| prompt.multi)
            .map(|prompt| {
                prompt
                    .options
                    .iter()
                    .filter(|option| option.selected)
                    .map(|option| option.number)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A prompt's question tabs as `(label, answered, current)`.
    fn tabs(prompt: Option<&ChatPrompt>) -> Vec<(&str, bool, bool)> {
        prompt
            .map(|prompt| {
                prompt
                    .questions
                    .iter()
                    .map(|tab| (tab.label.as_str(), tab.answered, tab.current))
                    .collect()
            })
            .unwrap_or_default()
    }

    const Q1_SUBJECT: &str = "Which function should be added to calc.py next?";

    const Q1_OPTIONS: [(u32, &str); 4] =
        [(1, "sub"), (2, "div"), (3, "pow"), (4, "Type something")];

    const Q1_TABS: [(&str, bool, bool); 3] = [
        ("Next fn", false, true),
        ("Tests", false, false),
        ("Style", false, false),
    ];

    const Q2_SUBJECT: &str = "Which tests should be written?";

    const Q2_OPTIONS: [(u32, &str); 5] = [
        (1, "add"),
        (2, "mul"),
        (3, "edge cases"),
        (4, "property tests"),
        (5, "Type something"),
    ];

    /// The tabs once the second question has a tick: the plain text has no
    /// highlight, so the first unanswered tab, Style, reads as current.
    const Q2_TICKED_TABS: [(&str, bool, bool); 3] = [
        ("Next fn", true, false),
        ("Tests", true, false),
        ("Style", false, true),
    ];

    const REVIEW_BODY: &str = "Which function should be added to calc.py next? -> div\n\
        Which tests should be written? -> add, edge cases, fuzz tests\n\
        Which code style should the functions use? -> Plain";

    const FILE_OPTIONS: [(u32, &str); 3] = [
        (1, "Yes"),
        (
            2,
            "Yes, and switch to accept edits (auto-approve file edits and common file commands) \
             for this session (shift+tab)",
        ),
        (3, "No"),
    ];

    const BASH_OPTIONS: [(u32, &str); 4] = [
        (1, "Yes"),
        (2, "Yes, and don\u{2019}t ask again for: python *"),
        (
            3,
            "Yes, and switch to auto mode · auto mode handles these prompts for you",
        ),
        (4, "No"),
    ];

    const PLAN_OPTIONS: [(u32, &str); 3] = [
        (1, "Yes, and use auto mode"),
        (2, "Yes, manually approve edits"),
        (3, "Tell Claude what to change"),
    ];

    const BASH_SUBJECT: &str = "Print the product of 6 and 7";

    const HEADS_UP_SUBJECT: &str = "Heads up · One of the notes files you had read says a \
        nightly cleanup task wipes the X: drive, where your repos live. The \"reply only \
        done\" instruction could hide that.";

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one row per capture; the table is the test"
    )]
    fn every_fixture_reads_its_dialog() {
        use ChatPromptKind::{HeadsUp, Plan, Review, Trust};
        use ComposerState::{Draft, Ready, Working};
        let mut q2_typed = Q2_OPTIONS.to_vec();
        q2_typed[4] = (5, "fuzz tests");
        let mut amend_open = BASH_OPTIONS.to_vec();
        amend_open[0] = (1, "Yes, and tell Claude what to do next");
        let mut amend_typed = BASH_OPTIONS.to_vec();
        amend_typed[0] = (1, "Yes, but print 6*8 instead");
        let mut plan_typed = PLAN_OPTIONS.to_vec();
        plan_typed[2] = (3, "also test div(6, 3) == 2");
        let cases: Vec<(&str, &str, Expected)> = vec![
            (
                "v21-perm-bash",
                include_str!("fixtures/v21-perm-bash.txt"),
                permission("notes.txt", &FILE_OPTIONS, None),
            ),
            (
                "v22-perm-bash",
                include_str!("fixtures/v22-perm-bash.txt"),
                permission(BASH_SUBJECT, &BASH_OPTIONS, Some("python *")),
            ),
            (
                "c23-perm-bash",
                include_str!("fixtures/c23-perm-bash.txt"),
                permission(BASH_SUBJECT, &BASH_OPTIONS, Some("python *")),
            ),
            (
                "c52-edit-perm",
                include_str!("fixtures/c52-edit-perm.txt"),
                permission("calc.py", &FILE_OPTIONS, None),
            ),
            (
                "claude--permission-bash",
                include_str!("fixtures/claude--permission-bash.txt"),
                permission(
                    "mkfifo fixture-fifo",
                    &[
                        (1, "Yes"),
                        (
                            2,
                            "Yes, and don\u{2019}t ask again for: mkfifo fixture-fifo *",
                        ),
                        (3, "No"),
                    ],
                    Some("mkfifo fixture-fifo *"),
                ),
            ),
            (
                "v23-amend-open",
                include_str!("fixtures/v23-amend-open.txt"),
                Expected {
                    can_amend: false,
                    ..permission(BASH_SUBJECT, &amend_open, Some("python *"))
                },
            ),
            (
                "v24-amend-typed",
                include_str!("fixtures/v24-amend-typed.txt"),
                Expected {
                    can_amend: false,
                    ..permission(BASH_SUBJECT, &amend_typed, Some("python *"))
                },
            ),
            (
                "v51-plan-dialog",
                include_str!("fixtures/v51-plan-dialog.txt"),
                dialog(Plan, "Add div to calc.py", &PLAN_OPTIONS, Some(1)),
            ),
            (
                "v52-plan-feedback-open",
                include_str!("fixtures/v52-plan-feedback-open.txt"),
                dialog(Plan, "Add div to calc.py", &PLAN_OPTIONS, Some(3)),
            ),
            (
                "v53-plan-feedback-typed",
                include_str!("fixtures/v53-plan-feedback-typed.txt"),
                dialog(Plan, "Add div to calc.py", &plan_typed, Some(3)),
            ),
            (
                "v01-start",
                include_str!("fixtures/v01-start.txt"),
                dialog(
                    Trust,
                    r"C:\Users\lefto\AppData\Local\Temp\claude\X--dev-rustling-tulip\2d069daa-1f0f-43ac-84d8-a6a2a11ed017\scratchpad\lab\vanilla",
                    &[(1, "No, exit"), (2, "Yes, I trust this folder")],
                    Some(1),
                ),
            ),
            (
                "claude--trust-prompt",
                include_str!("fixtures/claude--trust-prompt.txt"),
                dialog(
                    Trust,
                    "/tmp/claude-1000/-var-home-altan-playground-collie/bad3b8de-6e92-4541-b9cc-4af710c91d88/scratchpad/fixture-sandbox",
                    &[(1, "Yes, I trust this folder"), (2, "No, exit")],
                    Some(1),
                ),
            ),
            (
                "v152-learn-more",
                include_str!("fixtures/v152-learn-more.txt"),
                Expected {
                    composer: Ready,
                    ..dialog(
                        HeadsUp,
                        HEADS_UP_SUBJECT,
                        &[
                            (1, "Understood"),
                            (2, "Chat in main session"),
                            (0, "Dismiss"),
                        ],
                        None,
                    )
                },
            ),
            (
                // hand-made from v152-learn-more: no capture of the collapsed notice exists
                "v152-heads-up-collapsed",
                include_str!("fixtures/v152-heads-up-collapsed.txt"),
                Expected {
                    composer: Ready,
                    ..dialog(
                        HeadsUp,
                        HEADS_UP_SUBJECT,
                        &[(1, "Learn more"), (2, "Knew this already"), (0, "Dismiss")],
                        None,
                    )
                },
            ),
            (
                "v95-rewind",
                include_str!("fixtures/v95-rewind.txt"),
                overlay("rewind"),
            ),
            (
                "v96-ctrl-o",
                include_str!("fixtures/v96-ctrl-o.txt"),
                overlay("transcript"),
            ),
            (
                "v140-model-picker",
                include_str!("fixtures/v140-model-picker.txt"),
                overlay("model"),
            ),
            (
                "claude--menu-model-picker",
                include_str!("fixtures/claude--menu-model-picker.txt"),
                overlay("model"),
            ),
            (
                "v141-btw",
                include_str!("fixtures/v141-btw.txt"),
                overlay("btw"),
            ),
            (
                "v03-ready",
                include_str!("fixtures/v03-ready.txt"),
                no_prompt(Ready),
            ),
            (
                "claude--fresh-idle",
                include_str!("fixtures/claude--fresh-idle.txt"),
                no_prompt(Ready),
            ),
            (
                "v71-agents-running",
                include_str!("fixtures/v71-agents-running.txt"),
                no_prompt(Working),
            ),
            (
                "v110-paste-3lines",
                include_str!("fixtures/v110-paste-3lines.txt"),
                no_prompt(Draft),
            ),
            (
                "v111-paste-30lines",
                include_str!("fixtures/v111-paste-30lines.txt"),
                no_prompt(Draft),
            ),
            (
                "v80-bang-mode",
                include_str!("fixtures/v80-bang-mode.txt"),
                no_prompt(Ready),
            ),
            (
                "v11-turn1-end",
                include_str!("fixtures/v11-turn1-end.txt"),
                no_prompt(Ready),
            ),
            (
                "c130-ask-esc",
                include_str!("fixtures/c130-ask-esc.txt"),
                no_prompt(Ready),
            ),
            (
                "v26-deny",
                include_str!("fixtures/v26-deny.txt"),
                no_prompt(Ready),
            ),
            (
                "v31-ask-q1",
                include_str!("fixtures/v31-ask-q1.txt"),
                question(Q1_SUBJECT, &Q1_OPTIONS, Some(1), None, &Q1_TABS),
            ),
            (
                "c31-ask-q1",
                include_str!("fixtures/c31-ask-q1.txt"),
                question(Q1_SUBJECT, &Q1_OPTIONS, Some(1), None, &Q1_TABS),
            ),
            (
                "v32-ask-q2",
                include_str!("fixtures/v32-ask-q2.txt"),
                question(
                    Q2_SUBJECT,
                    &Q2_OPTIONS,
                    Some(1),
                    Some(&[]),
                    &[
                        ("Next fn", true, false),
                        ("Tests", false, true),
                        ("Style", false, false),
                    ],
                ),
            ),
            (
                "v33-ask-q2-toggled",
                include_str!("fixtures/v33-ask-q2-toggled.txt"),
                question(
                    Q2_SUBJECT,
                    &Q2_OPTIONS,
                    Some(1),
                    Some(&[1, 3]),
                    &Q2_TICKED_TABS,
                ),
            ),
            (
                "v34-ask-other",
                include_str!("fixtures/v34-ask-other.txt"),
                question(
                    Q2_SUBJECT,
                    &Q2_OPTIONS,
                    Some(1),
                    Some(&[1, 3, 5]),
                    &Q2_TICKED_TABS,
                ),
            ),
            (
                "v36-ask-other-cursor",
                include_str!("fixtures/v36-ask-other-cursor.txt"),
                question(
                    Q2_SUBJECT,
                    &Q2_OPTIONS,
                    Some(5),
                    Some(&[1, 3, 5]),
                    &Q2_TICKED_TABS,
                ),
            ),
            (
                "v37-ask-other-typed2",
                include_str!("fixtures/v37-ask-other-typed2.txt"),
                question(
                    Q2_SUBJECT,
                    &q2_typed,
                    Some(5),
                    Some(&[1, 3, 5]),
                    &Q2_TICKED_TABS,
                ),
            ),
            (
                "v38-ask-q3",
                include_str!("fixtures/v38-ask-q3.txt"),
                question(
                    Q2_SUBJECT,
                    &q2_typed,
                    None,
                    Some(&[1, 3, 5]),
                    &Q2_TICKED_TABS,
                ),
            ),
            (
                "v39-ask-q3",
                include_str!("fixtures/v39-ask-q3.txt"),
                overlay("question_preview"),
            ),
            (
                "v40-ask-q3-down",
                include_str!("fixtures/v40-ask-q3-down.txt"),
                overlay("question_preview"),
            ),
            (
                "v41-ask-notes",
                include_str!("fixtures/v41-ask-notes.txt"),
                overlay("question_preview"),
            ),
            (
                "v42-ask-review",
                include_str!("fixtures/v42-ask-review.txt"),
                Expected {
                    tabs: vec![
                        ("Next fn", true, false),
                        ("Tests", true, false),
                        ("Style", true, false),
                    ],
                    body: Some(REVIEW_BODY),
                    ..dialog(
                        Review,
                        "Review your answers",
                        &[(1, "Submit answers"), (2, "Cancel")],
                        Some(1),
                    )
                },
            ),
            (
                "v43-ask-submitted",
                include_str!("fixtures/v43-ask-submitted.txt"),
                no_prompt(Ready),
            ),
            (
                "c32-ask-chat",
                include_str!("fixtures/c32-ask-chat.txt"),
                no_prompt(Working),
            ),
        ];
        let wrong: Vec<String> = cases
            .iter()
            .filter_map(|(name, text, expected)| {
                mismatch(&read_capture(text), expected).map(|why| format!("{name}: {why}"))
            })
            .collect();
        assert!(
            wrong.is_empty(),
            "captures read the wrong dialog:\n{}",
            wrong.join("\n")
        );
    }

    /// A recipe's steps as `(keys, expect)` pairs.
    type Steps = Vec<(Vec<Key>, ScreenExpect)>;

    fn steps(recipe: AnswerRecipe) -> Steps {
        recipe
            .steps
            .into_iter()
            .map(|step| (step.keys, step.expect))
            .collect()
    }

    fn option(number: u32) -> ChatAnswerKind {
        ChatAnswerKind::Option { number }
    }

    fn toggle(number: u32) -> ChatAnswerKind {
        ChatAnswerKind::Toggle { number }
    }

    fn text(value: &str) -> String {
        value.to_owned()
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one row per answer; the table is the test"
    )]
    fn recipes_per_answer() {
        use ScreenExpect::{Any, DialogGone, OptionSelected, OptionTicked, SameDialog};
        let v31 = include_str!("fixtures/v31-ask-q1.txt");
        let v32 = include_str!("fixtures/v32-ask-q2.txt");
        let v33 = include_str!("fixtures/v33-ask-q2-toggled.txt");
        let v36 = include_str!("fixtures/v36-ask-other-cursor.txt");
        let v37 = include_str!("fixtures/v37-ask-other-typed2.txt");
        let v38 = include_str!("fixtures/v38-ask-q3.txt");
        let v42 = include_str!("fixtures/v42-ask-review.txt");
        let c23 = include_str!("fixtures/c23-perm-bash.txt");
        let v21 = include_str!("fixtures/v21-perm-bash.txt");
        let v51 = include_str!("fixtures/v51-plan-dialog.txt");
        let v01 = include_str!("fixtures/v01-start.txt");
        let collie_trust = include_str!("fixtures/claude--trust-prompt.txt");
        let v152 = include_str!("fixtures/v152-learn-more.txt");
        let v23 = include_str!("fixtures/v23-amend-open.txt");
        let v52 = include_str!("fixtures/v52-plan-feedback-open.txt");
        let v53 = include_str!("fixtures/v53-plan-feedback-typed.txt");
        let typed = |value: &str| Ok(vec![(vec![Key::Text(text(value)), Key::Enter], Any)]);
        let refused = Err(AnswerRefusal::NotThisDialog);
        let type_x = || ChatAnswerKind::TypeSomething { text: text("x") };
        let digit = |n: u8| (vec![Key::Digit(n)], DialogGone);
        let cases: Vec<(&str, &str, ChatAnswerKind, Result<Steps, AnswerRefusal>)> = vec![
            ("c23-perm-bash", c23, option(1), Ok(vec![digit(1)])),
            ("c23-perm-bash", c23, option(2), Ok(vec![digit(2)])),
            ("c23-perm-bash", c23, option(4), Ok(vec![digit(4)])),
            (
                "c23-perm-bash",
                c23,
                ChatAnswerKind::AllowWithText {
                    text: text("but print 6*8 instead"),
                },
                Ok(vec![
                    (vec![Key::Tab], SameDialog),
                    (
                        vec![Key::Text(text("but print 6*8 instead")), Key::Enter],
                        DialogGone,
                    ),
                ]),
            ),
            (
                "c23-perm-bash",
                c23,
                ChatAnswerKind::DenyWithText {
                    text: text("use 6*8"),
                },
                Ok(vec![digit(4)]),
            ),
            ("v21-perm-bash", v21, option(1), Ok(vec![digit(1)])),
            ("v21-perm-bash", v21, option(3), Ok(vec![digit(3)])),
            ("v51-plan-dialog", v51, option(1), Ok(vec![digit(1)])),
            ("v51-plan-dialog", v51, option(2), Ok(vec![digit(2)])),
            (
                "v51-plan-dialog",
                v51,
                ChatAnswerKind::TypeSomething {
                    text: text("also test div(6, 3) == 2"),
                },
                Ok(vec![
                    (vec![Key::Digit(3)], SameDialog),
                    (
                        vec![Key::Text(text("also test div(6, 3) == 2")), Key::Enter],
                        Any,
                    ),
                ]),
            ),
            (
                "v01-start",
                v01,
                option(2),
                Ok(vec![
                    (vec![Key::Down], OptionSelected(2)),
                    (vec![Key::Enter], DialogGone),
                ]),
            ),
            (
                "v01-start",
                v01,
                option(1),
                Ok(vec![(vec![Key::Enter], DialogGone)]),
            ),
            (
                "claude--trust-prompt",
                collie_trust,
                option(1),
                Ok(vec![(vec![Key::Enter], DialogGone)]),
            ),
            (
                "claude--trust-prompt",
                collie_trust,
                option(2),
                Ok(vec![
                    (vec![Key::Down], OptionSelected(2)),
                    (vec![Key::Enter], DialogGone),
                ]),
            ),
            (
                "v152-learn-more",
                v152,
                option(1),
                Ok(vec![(vec![Key::Digit(1)], Any)]),
            ),
            (
                "v152-learn-more",
                v152,
                option(2),
                Ok(vec![(vec![Key::Digit(2)], Any)]),
            ),
            (
                "v152-learn-more",
                v152,
                option(0),
                Ok(vec![(vec![Key::Digit(0)], Any)]),
            ),
            (
                "c23-perm-bash",
                c23,
                option(9),
                Err(AnswerRefusal::UnknownOption(9)),
            ),
            (
                "c23-perm-bash",
                c23,
                ChatAnswerKind::Toggle { number: 1 },
                Err(AnswerRefusal::NotThisDialog),
            ),
            (
                "v01-start",
                v01,
                ChatAnswerKind::AllowWithText { text: text("ok") },
                Err(AnswerRefusal::NotThisDialog),
            ),
            ("v52-plan-feedback-open", v52, type_x(), typed("x")),
            ("v53-plan-feedback-typed", v53, type_x(), typed("x")),
            ("v52-plan-feedback-open", v52, option(1), refused.clone()),
            ("v23-amend-open", v23, option(1), refused.clone()),
            (
                "v23-amend-open",
                v23,
                ChatAnswerKind::DenyWithText { text: text("x") },
                refused.clone(),
            ),
            (
                "v23-amend-open",
                v23,
                ChatAnswerKind::AllowWithText { text: text("x") },
                refused.clone(),
            ),
            ("v51-plan-dialog", v51, option(3), refused.clone()),
            (
                "v51-plan-dialog",
                v51,
                ChatAnswerKind::TypeSomething { text: text("") },
                Err(AnswerRefusal::TextEmpty),
            ),
            (
                "v31-ask-q1",
                v31,
                option(1),
                Ok(vec![(vec![Key::Digit(1)], Any)]),
            ),
            (
                "v31-ask-q1",
                v31,
                option(3),
                Ok(vec![(vec![Key::Digit(3)], Any)]),
            ),
            (
                "v31-ask-q1",
                v31,
                option(5),
                Err(AnswerRefusal::UnknownOption(5)),
            ),
            (
                "v31-ask-q1",
                v31,
                type_x(),
                Ok(vec![
                    (vec![Key::Down; 3], OptionSelected(4)),
                    (vec![Key::Text(text("x")), Key::Enter], Any),
                ]),
            ),
            (
                "v31-ask-q1",
                v31,
                ChatAnswerKind::TypeSomething { text: text("  ") },
                Err(AnswerRefusal::TextEmpty),
            ),
            (
                "v31-ask-q1",
                v31,
                ChatAnswerKind::Back,
                Ok(vec![(vec![Key::Left], Any)]),
            ),
            (
                "v31-ask-q1",
                v31,
                ChatAnswerKind::Skip,
                Ok(vec![(vec![Key::Right], Any)]),
            ),
            ("v31-ask-q1", v31, option(4), refused.clone()),
            ("v31-ask-q1", v31, toggle(1), refused.clone()),
            ("v31-ask-q1", v31, ChatAnswerKind::Submit, refused.clone()),
            (
                "v32-ask-q2",
                v32,
                toggle(1),
                Ok(vec![(vec![Key::Digit(1)], OptionTicked(1, true))]),
            ),
            (
                "v33-ask-q2-toggled",
                v33,
                toggle(3),
                Ok(vec![(vec![Key::Digit(3)], OptionTicked(3, false))]),
            ),
            (
                "v32-ask-q2",
                v32,
                toggle(6),
                Err(AnswerRefusal::UnknownOption(6)),
            ),
            ("v32-ask-q2", v32, option(1), refused.clone()),
            (
                "v33-ask-q2-toggled",
                v33,
                ChatAnswerKind::Submit,
                Ok(vec![
                    (vec![Key::Down; 5], SameDialog),
                    (vec![Key::Enter], Any),
                ]),
            ),
            (
                "v36-ask-other-cursor",
                v36,
                ChatAnswerKind::Submit,
                Ok(vec![(vec![Key::Down], SameDialog), (vec![Key::Enter], Any)]),
            ),
            ("v37-ask-other-typed2", v37, toggle(1), refused.clone()),
            ("v36-ask-other-cursor", v36, toggle(2), refused.clone()),
            (
                "v36-ask-other-cursor",
                v36,
                ChatAnswerKind::Back,
                refused.clone(),
            ),
            ("v38-ask-q3", v38, ChatAnswerKind::Submit, refused.clone()),
            (
                "v32-ask-q2",
                v32,
                ChatAnswerKind::TypeSomething {
                    text: text("fuzz tests"),
                },
                Ok(vec![
                    (vec![Key::Down; 4], OptionSelected(5)),
                    (vec![Key::Text(text("fuzz tests")), Key::Enter], Any),
                ]),
            ),
            (
                "v36-ask-other-cursor",
                v36,
                ChatAnswerKind::TypeSomething {
                    text: text("fuzz tests"),
                },
                typed("fuzz tests"),
            ),
            ("v38-ask-q3", v38, type_x(), refused.clone()),
            ("v42-ask-review", v42, option(1), Ok(vec![digit(1)])),
            ("v42-ask-review", v42, option(2), Ok(vec![digit(2)])),
            (
                "v42-ask-review",
                v42,
                option(3),
                Err(AnswerRefusal::UnknownOption(3)),
            ),
            ("v42-ask-review", v42, ChatAnswerKind::Back, refused.clone()),
            ("v42-ask-review", v42, type_x(), refused.clone()),
        ];
        let agent = agent_registry::default_agent();
        let wrong: Vec<String> = cases
            .into_iter()
            .filter_map(|(name, capture, answer, expected)| {
                let got = recipe_for(&agent, &prompt_of(capture), &answer).map(steps);
                (got != expected)
                    .then(|| format!("{name} {answer:?}: wanted {expected:?}, got {got:?}"))
            })
            .collect();
        assert!(
            wrong.is_empty(),
            "answers built the wrong recipe:\n{}",
            wrong.join("\n")
        );
    }

    /// Whether `after` is the reading `expect` waits for, the step having
    /// been typed on `before`.
    fn holds(expect: &ScreenExpect, before: &ChatScreen, after: &ChatScreen) -> bool {
        let identity = |screen: &ChatScreen| screen.prompt.as_ref().map(prompt_identity);
        match expect {
            ScreenExpect::DialogGone => {
                after.prompt.is_none() && after.composer != ComposerState::Dialog
            }
            ScreenExpect::SameDialog => {
                identity(before).is_some() && identity(before) == identity(after)
            }
            ScreenExpect::NextPrompt(kind) => {
                after
                    .prompt
                    .as_ref()
                    .is_some_and(|prompt| &prompt.kind == kind)
                    && identity(before) != identity(after)
            }
            ScreenExpect::ComposerReady => {
                matches!(
                    after.composer,
                    ComposerState::Ready | ComposerState::Working
                )
            }
            ScreenExpect::OptionSelected(number) => after
                .prompt
                .as_ref()
                .is_some_and(|prompt| prompt.cursor == Some(*number)),
            ScreenExpect::OptionTicked(number, ticked) => {
                after.prompt.as_ref().is_some_and(|prompt| {
                    prompt.multi
                        && prompt
                            .options
                            .iter()
                            .any(|option| option.number == *number && option.selected == *ticked)
                })
            }
            ScreenExpect::Any => true,
        }
    }

    #[test]
    fn recipe_expectations_hold_on_next_screen() {
        let cases = [
            (
                "v22-perm-bash -> v23-amend-open",
                include_str!("fixtures/v22-perm-bash.txt"),
                include_str!("fixtures/v23-amend-open.txt"),
                ChatAnswerKind::AllowWithText {
                    text: text("but print 6*8 instead"),
                },
            ),
            (
                "v51-plan-dialog -> v52-plan-feedback-open",
                include_str!("fixtures/v51-plan-dialog.txt"),
                include_str!("fixtures/v52-plan-feedback-open.txt"),
                ChatAnswerKind::TypeSomething {
                    text: text("also test div(6, 3) == 2"),
                },
            ),
            (
                "v52-plan-feedback-open -> v53-plan-feedback-typed",
                include_str!("fixtures/v52-plan-feedback-open.txt"),
                include_str!("fixtures/v53-plan-feedback-typed.txt"),
                ChatAnswerKind::TypeSomething {
                    text: text("also test div(6, 3) == 2"),
                },
            ),
            (
                "v32-ask-q2 -> v33-ask-q2-toggled (1)",
                include_str!("fixtures/v32-ask-q2.txt"),
                include_str!("fixtures/v33-ask-q2-toggled.txt"),
                toggle(1),
            ),
            (
                "v32-ask-q2 -> v33-ask-q2-toggled (3)",
                include_str!("fixtures/v32-ask-q2.txt"),
                include_str!("fixtures/v33-ask-q2-toggled.txt"),
                toggle(3),
            ),
            (
                "v31-ask-q1 -> v32-ask-q2",
                include_str!("fixtures/v31-ask-q1.txt"),
                include_str!("fixtures/v32-ask-q2.txt"),
                option(1),
            ),
        ];
        let agent = agent_registry::default_agent();
        for (pair, before, after, answer) in cases {
            let (before, after) = (read_capture(before), read_capture(after));
            let prompt = before
                .prompt
                .as_ref()
                .expect("the first capture shows a prompt");
            let recipe = recipe_for(&agent, prompt, &answer).expect("the answer has a recipe");
            let first = &recipe.steps[0].expect;
            assert!(
                holds(first, &before, &after),
                "{pair}: {first:?} does not hold"
            );
        }
    }

    #[test]
    fn a_wrapped_cui_card_reads_as_vanilla() {
        let without_descriptions = |text: &str| {
            let mut screen = read_capture(text);
            if let Some(prompt) = screen.prompt.as_mut() {
                for option in &mut prompt.options {
                    option.description = None;
                }
            }
            screen
        };
        let cui = without_descriptions(include_str!("fixtures/c31-ask-q1.txt"));
        assert!(cui.prompt.is_some(), "c31-ask-q1 reads a question");
        assert_eq!(
            cui,
            without_descriptions(include_str!("fixtures/v31-ask-q1.txt"))
        );
    }

    #[test]
    fn a_prompt_read_twice_has_one_identity() {
        let v21 = include_str!("fixtures/v21-perm-bash.txt");
        assert_eq!(
            prompt_identity(&prompt_of(v21)),
            prompt_identity(&prompt_of(v21))
        );
    }

    #[test]
    fn opening_the_amend_field_keeps_the_identity() {
        assert_eq!(
            prompt_identity(&prompt_of(include_str!("fixtures/v22-perm-bash.txt"))),
            prompt_identity(&prompt_of(include_str!("fixtures/v23-amend-open.txt")))
        );
    }

    #[test]
    fn a_plan_and_a_permission_differ() {
        assert_ne!(
            prompt_identity(&prompt_of(include_str!("fixtures/v51-plan-dialog.txt"))),
            prompt_identity(&prompt_of(include_str!("fixtures/v21-perm-bash.txt")))
        );
    }
}
