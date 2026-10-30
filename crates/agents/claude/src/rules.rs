//! Claude Code's screen rules, and the readiness and file-reference answers
//! the plugin gives the daemon.
//!
//! The rule table decides the state one rendered session shows: the
//! highest-priority rule whose matcher hits its region, and none of whose
//! guards does, gives the reading. An overlay rule keeps whatever state the
//! session already has, since the overlay is drawn over a turn that may be
//! running or done. The two background rules read the footer apart from the
//! state rules: a shell count on the mode line, or a running subagent's row.

use agent_api::{LineMatcher, ReadySignal, ScreenRegion, ScreenRule, ScreenRules, ScreenVerdict};
use std::sync::LazyLock;
use std::time::Duration;

/// The spinner line of a turn in progress: a cycling glyph, a capitalised
/// verb, an ellipsis, then the optional phase, the elapsed time and the token
/// details, which the terminal width may cut off mid-detail.
const SPINNER: &str = r"^\s*[·✢*✶✻✽✳]\s+\p{Lu}[\p{L}'’-]*…(?:\s+·\s+[\p{L} ]+?)?(?:\s+\(.*)?\s*$";

/// The spinner a turn keeps painted while its hooks run: Claude leaves it up
/// across a hook-reported stop or tool start, so the line is not a turn of its
/// own.
const HOOKS_RUNNING: &str = r"^\s*[·✢*✶✻✽✳]\s+\S.*\(running \p{L}+ hooks…";

/// The mode line under the prompt box counting the shells still running in
/// the background: the mode's glyph, then `· 1 shell` or `· N shells` as one
/// of its `·`-separated items.
const BACKGROUND_SHELLS: &str = r"^\s*(?:⏵⏵|⏸) .* · \d+ shells?(?: ·.*)?\s*$";

/// A running subagent's row in the panel under the prompt box: the hollow
/// marker, the agent type, two spaces, its task, then its elapsed time and
/// token count.
const RUNNING_SUBAGENT: &str = r"^\s+◯ \S+  \S.*\s\d+s · [↑↓] \S+ tokens\s*$";

/// How much output and how long a silence the daemon waits for before it
/// types a prompt into Claude's TUI: the banner tuning the heuristic was
/// measured against.
pub(crate) const READY_SIGNAL: ReadySignal = ReadySignal {
    min_output_bytes: 1024,
    quiet_for: Duration::from_millis(1500),
};

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
            matcher: LineMatcher::Contains("Enter to set as default".to_string()),
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
            name: "dialog_permission".to_string(),
            priority: 980,
            region: ScreenRegion::BottomNonEmpty { rows: 24 },
            matcher: LineMatcher::Contains("Esc to cancel · Tab to amend".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::AwaitingInput,
        },
        ScreenRule {
            name: "dialog_question".to_string(),
            priority: 980,
            region: ScreenRegion::BottomNonEmpty { rows: 24 },
            matcher: LineMatcher::Regex(r"Enter to select ·.*Esc to cancel".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::AwaitingInput,
        },
        ScreenRule {
            name: "dialog_question_review".to_string(),
            priority: 980,
            region: ScreenRegion::BottomNonEmpty { rows: 24 },
            matcher: LineMatcher::Regex(r"^\s*(?:❯\s*)?\d+\.\s+Submit answers\s*$".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::AwaitingInput,
        },
        ScreenRule {
            name: "dialog_plan".to_string(),
            priority: 980,
            region: ScreenRegion::BottomNonEmpty { rows: 24 },
            matcher: LineMatcher::Contains("Tell Claude what to change".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::AwaitingInput,
        },
        ScreenRule {
            name: "dialog_trust".to_string(),
            priority: 980,
            region: ScreenRegion::BottomNonEmpty { rows: 24 },
            matcher: LineMatcher::Contains("Enter to confirm · Esc to cancel".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::AwaitingInput,
        },
        ScreenRule {
            name: "turn_hooks_running".to_string(),
            priority: 975,
            region: ScreenRegion::AbovePromptBox { rows: 24 },
            matcher: LineMatcher::Regex(HOOKS_RUNNING.to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
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
            matcher: LineMatcher::Contains("✦ Heads up ·".to_string()),
            not: Vec::new(),
            verdict: ScreenVerdict::Keep,
        },
        ScreenRule {
            name: "idle_interrupted".to_string(),
            priority: 950,
            region: ScreenRegion::AbovePromptBox { rows: 12 },
            matcher: LineMatcher::Contains("⎿  Interrupted ·".to_string()),
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
    use super::{READY_SIGNAL, file_reference, screen_rules};
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
                ("dialog_permission", 980),
                ("dialog_question", 980),
                ("dialog_question_review", 980),
                ("dialog_plan", 980),
                ("dialog_trust", 980),
                ("turn_hooks_running", 975),
                ("turn_spinner", 970),
                ("overlay_heads_up", 960),
                ("idle_interrupted", 950),
                ("idle_prompt_box", 940),
                ("background_shell", 930),
                ("background_subagents", 925),
                ("title_working", 200),
                ("title_idle", 100),
            ]
        );
        assert_eq!(screen_rules().rules.len(), 18);
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
