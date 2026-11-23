//! Claude Code's screen rules, and the readiness and file-reference answers
//! the plugin gives the daemon.
//!
//! The rule table decides the state one rendered session shows: the
//! highest-priority rule whose matcher hits its region, and none of whose
//! guards does, gives the reading. An overlay rule keeps whatever state the
//! session already has, since the overlay is drawn over a turn that may be
//! running or done. The two background rules read the footer apart from the
//! state rules: a background-work count on the mode line (shells, monitors,
//! MCP tasks), or a running subagent's row.

use agent_api::{LineMatcher, ReadySignal, ScreenRegion, ScreenRule, ScreenRules, ScreenVerdict};
use std::sync::LazyLock;
use std::time::Duration;

/// The spinner line of a turn in progress, in one of two forms after its
/// cycling glyph. The usual form is a capitalised verb and an ellipsis, then
/// the optional phase, the elapsed time and the token details, which the
/// terminal width may cut off mid-detail (`· Cooking… (21s · ↓ 1.1k tokens)`).
/// While a task-list item is in progress Claude shows the task's subject
/// instead: any text without an ellipsis, then the ellipsis and the optional
/// phase, which must be followed by the parenthesised details (`✽ Fix the
/// build… · thinking (5m 3s · ↓ 8.3k tokens)`), so a multi-word line without
/// them is not a spinner.
const SPINNER: &str = r"^\s*[·✢*✶✻✽✳]\s+(?:\p{Lu}[\p{L}'’-]*…(?:\s+·\s+[\p{L} ]+?)?(?:\s+\(.*)?|\S[^…]*…(?:\s+·\s+[\p{L} ]+?)?\s+\(.*)\s*$";

/// [`SPINNER`], compiled once for the plugin's own screen reading; `None`
/// when the pattern does not compile, which reads no row as a spinner.
static SPINNER_RE: LazyLock<Option<regex::Regex>> = LazyLock::new(|| {
    regex::Regex::new(SPINNER)
        .inspect_err(|error| {
            tracing::error!(
                %error,
                pattern = SPINNER,
                "the spinner pattern does not compile; no row reads as a spinner"
            );
        })
        .ok()
});

/// Whether `row` is a turn's spinner line, by [`SPINNER`].
pub(crate) fn is_spinner_line(row: &str) -> bool {
    SPINNER_RE
        .as_ref()
        .is_some_and(|spinner| spinner.is_match(row))
}

/// The spinner a turn keeps painted while its hooks run: Claude leaves it up
/// across a hook-reported stop or tool start, so the line is not a turn of its
/// own.
const HOOKS_RUNNING: &str = r"^\s*[·✢*✶✻✽✳]\s+\S.*\(running \p{L}+ hooks…";

/// The mode line under the prompt box counting the work still running in the
/// background: the mode's glyph, then one `·`-separated item per kind of work,
/// drawn as Claude Code 2.1.317 renders it: `· 1 shell`/`· N shells`,
/// `· 1 monitor`/`· N monitors`, `· 1 MCP task`/`· N MCP tasks`, or
/// `· N background tasks` when several kinds run together. Shells and monitors
/// share an item, joined with a comma (`· 1 shell, 2 monitors`). The vim
/// editor mode's `-- INSERT -- ` marker heads the line in insert mode.
const BACKGROUND_SHELLS: &str = r"^\s*(?:-- INSERT -- )?(?:⏵⏵|⏸) .* · \d+ (?:shells?|monitors?|MCP tasks?|background tasks?)(?:, \d+ (?:shells?|monitors?))*(?: ·.*)?\s*$";

/// A running subagent's row in the panel under the prompt box: the hollow
/// marker, the agent type, its task when the width leaves room for one, then
/// its elapsed time and token count, hours and minutes ahead of the seconds;
/// at narrow widths the task is gone and the word `tokens` with it.
const RUNNING_SUBAGENT: &str =
    r"^\s+◯ \S+(?:  \S.*)?\s(?:\d+[hm] )*\d+s · [↑↓] \S+(?: tokens)?\s*$";

/// How much output and how long a silence the daemon waits for before it
/// types a prompt into Claude's TUI: the banner tuning the heuristic was
/// measured against.
pub(crate) const READY_SIGNAL: ReadySignal = ReadySignal {
    min_output_bytes: 1024,
    quiet_for: Duration::from_millis(1500),
};

/// Where the dialog rules read: the bottom rows from the last `❯` row down,
/// or from the last `!` row in the bash mode, whose box draws `!` where the
/// cursor goes and so anchors the region as the `❯` row does. A dialog draws
/// its footer under its own cursor row, while a footer a reply quoted sits in
/// the transcript above the idle box's prompt row, so it is cut off (the
/// chat's dialog reader anchors its footer the same way).
const DIALOG_REGION: ScreenRegion = ScreenRegion::FromLastMarkedRow {
    marker: '❯',
    or_marker: Some('!'),
    rows: 24,
};

/// A question's footer row: `Enter to select ·`, headed by the vim mode's
/// insert marker on `Type something`; in a narrow pane that marker's head row
/// ends at `Enter to` or at `Enter to select`, its tail on the row beneath.
const QUESTION_FOOTER: &str =
    r"^\s*(?:(?:-- INSERT -- · )?Enter to select ·|-- INSERT -- · Enter to(?: select)?\s*$)";

/// Claude's screen rules, in descending priority order.
static SCREEN_RULES: LazyLock<ScreenRules> = LazyLock::new(|| ScreenRules {
    rules: vec![
        ScreenRule {
            name: "overlay_detailed_transcript".to_string(),
            priority: 990,
            region: ScreenRegion::BottomNonEmpty { rows: 4 },
            matcher: LineMatcher::Contains("Showing detailed transcript".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
        },
        ScreenRule {
            name: "overlay_model_picker".to_string(),
            priority: 990,
            region: ScreenRegion::BottomNonEmpty { rows: 4 },
            matcher: LineMatcher::Contains("s to use this session only".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
        },
        ScreenRule {
            name: "overlay_btw".to_string(),
            priority: 990,
            region: ScreenRegion::BottomNonEmpty { rows: 4 },
            matcher: LineMatcher::Contains("f to fork · Esc to close".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
        },
        ScreenRule {
            name: "overlay_rewind".to_string(),
            priority: 990,
            region: ScreenRegion::BottomNonEmpty { rows: 4 },
            matcher: LineMatcher::Contains("Enter to continue · Esc to cancel".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
        },
        ScreenRule {
            name: "overlay_rewind_restore".to_string(),
            priority: 990,
            // Rewind's choice screen draws no footer; its options end the screen.
            region: ScreenRegion::BottomNonEmpty { rows: 4 },
            // Its option count depends on whether the code changed since the
            // message, and a highlighted option may carry a hint after it.
            matcher: LineMatcher::Regex(r"^\s*(?:❯\s*)?\d+\. Summarize up to here\b".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
        },
        ScreenRule {
            name: "overlay_transcript_shortcuts".to_string(),
            priority: 990,
            region: ScreenRegion::BottomNonEmpty { rows: 4 },
            matcher: LineMatcher::Contains("q/ctrl+o to exit".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
        },
        ScreenRule {
            name: "overlay_transcript_search".to_string(),
            priority: 990,
            region: ScreenRegion::BottomNonEmpty { rows: 4 },
            matcher: LineMatcher::Contains("⌕ Search transcript".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
        },
        ScreenRule {
            name: "overlay_btw_pending".to_string(),
            priority: 990,
            // A side question still being answered: its footer is the bare key.
            region: ScreenRegion::BottomNonEmpty { rows: 4 },
            matcher: LineMatcher::Regex(r"^\s*Esc to close\s*$".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
        },
        ScreenRule {
            name: "overlay_background_dialog".to_string(),
            priority: 990,
            // The "Background this session?" dialog older versions draw on Left Left.
            region: ScreenRegion::BottomNonEmpty { rows: 4 },
            matcher: LineMatcher::Regex(r"^\s*(?:❯\s*)?1\. Background anyway\b".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
        },
        ScreenRule {
            name: "overlay_agents_view".to_string(),
            priority: 990,
            // Claude's agents view, which Left Left moves the session to: its
            // box is the view's own, not the session's. Its mode line under
            // the box is the anchor, which no reply's text can reach.
            region: ScreenRegion::AfterLastRule,
            matcher: LineMatcher::Regex(
                r"^\s*(?:(?:⏵⏵|⏸) )?.*· enter to return · space to reply\b".to_string(),
            ),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
        },
        ScreenRule {
            name: "dialog_permission".to_string(),
            priority: 980,
            region: DIALOG_REGION,
            // Tab and Down take the hint off the footer while the dialog stays up.
            matcher: LineMatcher::Regex(
                r"^\s*Esc to cancel(?:\s*·\s*Tab to amend.*)?\s*$".to_string(),
            ),
            not: Vec::new(),
            verdict: ScreenVerdict::AwaitingInput,
        },
        ScreenRule {
            name: "dialog_question".to_string(),
            priority: 980,
            region: DIALOG_REGION,
            // A narrow pane wraps the footer, cutting off its `Esc to cancel`;
            // the vim mode's insert marker heads it on `Type something`, with no Esc.
            matcher: LineMatcher::Regex(QUESTION_FOOTER.to_string()),
            // An Esc that does anything but cancel: a list, not a question.
            not: vec![LineMatcher::Regex(
                r"^\s*(?:-- INSERT -- · )?Enter to select ·.*Esc to\s+(?:[^c\s]|c[^a\s])"
                    .to_string(),
            )],
            verdict: ScreenVerdict::AwaitingInput,
        },
        ScreenRule {
            name: "dialog_question_review".to_string(),
            priority: 980,
            // No footer, and its matched row sits above the cursor on `2. Cancel`.
            // Its options end the screen, so only the last few rows matter; a
            // task panel drawn under them is not part of the reading.
            region: ScreenRegion::BottomNonEmptyAboveTasks { rows: 4 },
            matcher: LineMatcher::Regex(r"^\s*(?:❯\s*)?\d+\.\s+Submit answers\s*$".to_string()),
            // A bare rule row under the matched one: the input box's border,
            // which a live review never draws below its options, so the review
            // was quoted in a reply above the box, not drawn by a dialog. A
            // labelled rule is a named session's or a subagent's view.
            not: vec![LineMatcher::Regex(r"^\s*─{3,}\s*$".to_string())],
            verdict: ScreenVerdict::AwaitingInput,
        },
        ScreenRule {
            name: "dialog_plan".to_string(),
            priority: 980,
            region: DIALOG_REGION,
            matcher: LineMatcher::Contains("Tell Claude what to change".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::AwaitingInput,
        },
        ScreenRule {
            name: "dialog_trust".to_string(),
            priority: 980,
            region: DIALOG_REGION,
            matcher: LineMatcher::Contains("Enter to confirm · Esc to cancel".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::AwaitingInput,
        },
        ScreenRule {
            name: "dialog_auto_mode".to_string(),
            priority: 980,
            // No footer: the title is the anchor, above the cursor's row, so
            // the region is the rows under the last rule. Under the idle box's
            // bottom rule a title quoted in the transcript is cut off.
            region: ScreenRegion::AfterLastRule,
            matcher: LineMatcher::Regex(
                r"^\s*Make auto mode your default permission mode\?\s*$".to_string(),
            ),
            not: Vec::new(),
            verdict: ScreenVerdict::AwaitingInput,
        },
        ScreenRule {
            name: "dialog_mcp_servers".to_string(),
            priority: 980,
            region: DIALOG_REGION,
            matcher: LineMatcher::Regex(r"^\s*Space to select · Esc to reject all\s*$".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::AwaitingInput,
        },
        ScreenRule {
            name: "turn_hooks_running".to_string(),
            priority: 975,
            region: ScreenRegion::AbovePromptBox { rows: 24 },
            matcher: LineMatcher::Regex(HOOKS_RUNNING.to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::HooksRunning,
        },
        ScreenRule {
            name: "turn_spinner".to_string(),
            priority: 970,
            region: ScreenRegion::AbovePromptBox { rows: 24 },
            matcher: LineMatcher::Regex(SPINNER.to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Working,
        },
        ScreenRule {
            name: "overlay_heads_up".to_string(),
            priority: 960,
            region: ScreenRegion::AbovePromptBox { rows: 24 },
            matcher: LineMatcher::Regex(r"^✦ Heads up ·".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
        },
        ScreenRule {
            name: "overlay_you_should_know".to_string(),
            priority: 960,
            region: ScreenRegion::AbovePromptBox { rows: 24 },
            matcher: LineMatcher::Regex(r"^✦ You should know ·".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
        },
        ScreenRule {
            name: "overlay_heads_up_dismissed".to_string(),
            priority: 960,
            region: ScreenRegion::AbovePromptBox { rows: 24 },
            matcher: LineMatcher::Regex(r"^✦ Dismissed\.   ".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
        },
        ScreenRule {
            name: "overlay_heads_up_one_moment".to_string(),
            priority: 960,
            region: ScreenRegion::AbovePromptBox { rows: 24 },
            matcher: LineMatcher::Regex(r"^✦ One moment…\s*$".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
        },
        ScreenRule {
            name: "idle_interrupted".to_string(),
            priority: 950,
            region: ScreenRegion::AbovePromptBox { rows: 12 },
            // Claude's own interrupt row (`⎿`, a space and a no-break space),
            // fresh only while nothing but its chrome sits below it: the
            // turn's "✻ … for" line, a task panel and the effort indicator.
            // Any other row below is a later turn's, and the idle box reads.
            matcher: LineMatcher::MatchFollowedOnlyBy {
                pattern: r"^\s*⎿[\s\u{00A0}]+Interrupted ·".to_string(),
                allowed: vec![
                    r"^\s*✻ \S+ for \d".to_string(),
                    r"^\s*[◻◼✔✓☐☒] ".to_string(),
                    r"^\s*\d+/\d+ tasks? done\b".to_string(),
                    r"^\s*\d+ tasks? \(\d+ done, \d+ open\)".to_string(),
                    r"·\s*/effort\s*$".to_string(),
                ],
            },
            not: Vec::new(),
            verdict: ScreenVerdict::Idle,
        },
        ScreenRule {
            name: "idle_prompt_box".to_string(),
            priority: 940,
            region: ScreenRegion::PromptBoxBody,
            matcher: LineMatcher::Regex(r"^\s*(?:❯|!)(?:\s|$)".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Idle,
        },
        ScreenRule {
            name: "background_shell".to_string(),
            priority: 930,
            region: ScreenRegion::AfterLastRule,
            matcher: LineMatcher::Regex(BACKGROUND_SHELLS.to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Background,
        },
        ScreenRule {
            name: "background_subagents".to_string(),
            priority: 925,
            region: ScreenRegion::AfterLastRule,
            matcher: LineMatcher::Regex(RUNNING_SUBAGENT.to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Background,
        },
        ScreenRule {
            name: "title_working".to_string(),
            priority: 200,
            region: ScreenRegion::Title,
            matcher: LineMatcher::Regex(r"^[\u{2800}-\u{28FF}\u{25D0}-\u{25D3}] ".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Working,
        },
        ScreenRule {
            name: "title_idle".to_string(),
            priority: 100,
            region: ScreenRegion::Title,
            matcher: LineMatcher::Regex("^✳ ".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Idle,
        },
    ],
});

/// Claude's screen rules.
pub(crate) fn screen_rules() -> &'static ScreenRules {
    &SCREEN_RULES
}

/// `repo_relative` as a prompt names it: `@` in front of the path, which is
/// how Claude reads a file.
pub(crate) fn file_reference(repo_relative: &str) -> String {
    format!("@{repo_relative}")
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{READY_SIGNAL, file_reference, is_spinner_line, screen_rules};
    use crate::{Agent, ClaudePlugin};
    use std::time::Duration;

    #[test]
    fn claude_rules_round_trip_through_json() {
        let rules = screen_rules();
        let json = serde_json::to_string(rules).expect("the rules serialize");
        let back: agent_api::ScreenRules = serde_json::from_str(&json).expect("the rules decode");
        assert_eq!(&back, rules);
    }

    #[test]
    fn claude_rules_keep_the_pre_seam_table() {
        let pairs: Vec<(&str, u16)> = screen_rules()
            .rules
            .iter()
            .map(|rule| (rule.name.as_str(), rule.priority))
            .collect();
        assert_eq!(
            pairs,
            [
                ("overlay_detailed_transcript", 990),
                ("overlay_model_picker", 990),
                ("overlay_btw", 990),
                ("overlay_rewind", 990),
                ("overlay_rewind_restore", 990),
                ("overlay_transcript_shortcuts", 990),
                ("overlay_transcript_search", 990),
                ("overlay_btw_pending", 990),
                ("overlay_background_dialog", 990),
                ("overlay_agents_view", 990),
                ("dialog_permission", 980),
                ("dialog_question", 980),
                ("dialog_question_review", 980),
                ("dialog_plan", 980),
                ("dialog_trust", 980),
                ("dialog_auto_mode", 980),
                ("dialog_mcp_servers", 980),
                ("turn_hooks_running", 975),
                ("turn_spinner", 970),
                ("overlay_heads_up", 960),
                ("overlay_you_should_know", 960),
                ("overlay_heads_up_dismissed", 960),
                ("overlay_heads_up_one_moment", 960),
                ("idle_interrupted", 950),
                ("idle_prompt_box", 940),
                ("background_shell", 930),
                ("background_subagents", 925),
                ("title_working", 200),
                ("title_idle", 100),
            ]
        );
        assert_eq!(screen_rules().rules.len(), 29);
    }

    #[test]
    fn spinner_matches_a_task_subject_and_keeps_its_non_matches() {
        let spinners = [
            "✽ Settings form: validate the fields, then run the tests… (5m 3s · ↓ 8.3k tokens)",
            "· Cooking… (21s · ↓ 1.1k tokens)",
            "✻ Brewing…",
            "* fix lint errors… (3s · esc to interrupt)",
            "✳ Simmering… · thinking (12s · ↑ 400 tokens)",
            "✶ Fix the build steps 3-4: compile, live run, submit… · thinking (5m 24s · ↓ 9.7k tokens · thinking with high effort)",
            "✻ Fix the build… · tool use (40s · ↓ 2k tokens)",
        ];
        for row in spinners {
            assert!(is_spinner_line(row), "should be a spinner: {row:?}");
        }
        let others = [
            "      … +2 completed",
            "· a bullet without an ellipsis",
            "  ⎿  · Item 4 (Add a search box…) is booked → next release",
            "✽ Settings form: validate the fields…",
            "✽ Cooking… extra text",
            "✽ Settings form: validate the fields… · thinking",
        ];
        for row in others {
            assert!(!is_spinner_line(row), "should not be a spinner: {row:?}");
        }
    }

    #[test]
    fn claude_ready_signal_is_the_banner_tuning() {
        let signal = ClaudePlugin.ready_signal();
        assert_eq!(signal, READY_SIGNAL);
        assert_eq!(signal.min_output_bytes, 1024);
        assert_eq!(signal.quiet_for, Duration::from_millis(1500));
    }

    #[test]
    fn claude_file_reference_prefixes_at() {
        assert_eq!(ClaudePlugin.file_reference("prompts/a.md"), "@prompts/a.md");
        assert_eq!(file_reference(""), "@");
    }
}
