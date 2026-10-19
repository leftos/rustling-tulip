//! The daemon-pushed "VS Code workspace detected" suggestions: a queue shown
//! one at a time, the focused button and the answers they send.
//!
//! The view ([`crate::vscode_suggestion_view`]) only renders what this holds;
//! the queue order, the body text, the folder rows and the messages are
//! decided and unit-tested here.

use std::collections::VecDeque;

use protocol::{ClientMessage, VscodeWorkspaceSuggestion};

use crate::workspace_creator::{FolderRow, folder_rows};

/// The prompt's title.
pub(crate) const TITLE: &str = "VS Code workspace detected";
/// The dismiss button's accessibility label.
pub(crate) const DISMISS_LABEL: &str = "Dismiss suggestion";
/// The watch button's tooltip.
pub(crate) const WATCH_TIP: &str = "watch the file and re-sync members";

/// One suggestion as the queue holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Suggestion {
    pub(crate) repo_id: String,
    pub(crate) suggestion: VscodeWorkspaceSuggestion,
}

/// How one part of the prompt's body is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyStyle {
    /// Ordinary sentence text.
    Text,
    /// The `.code-workspace` path, in the monospace family.
    Code,
    /// The suggested workspace name, bold.
    Bold,
}

impl BodyStyle {
    /// The part's style name, for specs reading the drawn parts.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Code => "code",
            Self::Bold => "bold",
        }
    }
}

/// The prompt's buttons, in footer and Tab order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SuggestionButton {
    #[default]
    NotNow,
    Create,
    CreateWatch,
}

impl SuggestionButton {
    pub(crate) const ALL: [Self; 3] = [Self::NotNow, Self::Create, Self::CreateWatch];

    pub(crate) const fn selector(self) -> &'static str {
        match self {
            Self::NotNow => "vscode-prompt-not-now",
            Self::Create => "vscode-prompt-create",
            Self::CreateWatch => "vscode-prompt-create-watch",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::NotNow => "Not now",
            Self::Create => "Create",
            Self::CreateWatch => "Create & watch",
        }
    }
}

/// The suggestions the daemon pushed, oldest first, and the focused button of
/// the one on screen.
#[derive(Debug, Clone, Default)]
pub(crate) struct VscodeSuggestions {
    queue: VecDeque<Suggestion>,
    focus: SuggestionButton,
}

impl VscodeSuggestions {
    /// Queues a pushed suggestion; the first one queued is the one shown.
    pub(crate) fn push(&mut self, repo_id: String, suggestion: VscodeWorkspaceSuggestion) {
        if self.queue.is_empty() {
            self.focus = SuggestionButton::NotNow;
        }
        self.queue.push_back(Suggestion {
            repo_id,
            suggestion,
        });
    }

    /// The suggestion on screen.
    pub(crate) fn front(&self) -> Option<&Suggestion> {
        self.queue.front()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }

    /// Drops the shown suggestion; the next one, if any, takes its place with
    /// Not now focused.
    pub(crate) fn dismiss(&mut self) {
        self.queue.pop_front();
        self.focus = SuggestionButton::NotNow;
    }

    /// Forgets every queued suggestion, as a new connection does.
    pub(crate) fn clear(&mut self) {
        self.queue.clear();
        self.focus = SuggestionButton::NotNow;
    }

    pub(crate) fn focus(&self) -> SuggestionButton {
        self.focus
    }

    /// Tab and Shift+Tab: the next or previous button, wrapping.
    pub(crate) fn move_focus(&mut self, forward: bool) {
        let count = SuggestionButton::ALL.len();
        let index = SuggestionButton::ALL
            .iter()
            .position(|button| *button == self.focus)
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % count
        } else {
            (index + count - 1) % count
        };
        self.focus = SuggestionButton::ALL[next];
    }

    /// The message a choice sends; Not now sends nothing.
    pub(crate) fn message(&self, choice: SuggestionButton) -> Option<ClientMessage> {
        let watch = match choice {
            SuggestionButton::NotNow => return None,
            SuggestionButton::Create => false,
            SuggestionButton::CreateWatch => true,
        };
        self.front()
            .map(|front| ClientMessage::AcceptVscodeWorkspaceSuggestion {
                suggestion: front.suggestion.clone(),
                watch,
            })
    }

    /// The prompt's body in draw order: the file path as code, the sentence
    /// as text, and the workspace name bold. The view draws exactly these
    /// parts, so a spec reading them reads what is on screen.
    pub(crate) fn body_parts(&self) -> Vec<(String, BodyStyle)> {
        self.front().map_or_else(Vec::new, |front| {
            vec![
                (front.suggestion.source_path.clone(), BodyStyle::Code),
                (
                    format!(
                        " references {} folders. Create a rustling-tulip workspace named ",
                        front.suggestion.folders.len()
                    ),
                    BodyStyle::Text,
                ),
                (front.suggestion.suggested_name.clone(), BodyStyle::Bold),
                (" with these members?".to_owned(), BodyStyle::Text),
            ]
        })
    }

    /// The prompt's body as one string: [`Self::body_parts`]' strings joined.
    pub(crate) fn body(&self) -> String {
        self.body_parts()
            .into_iter()
            .map(|(text, _)| text)
            .collect()
    }

    /// The shown file's folder rows, in file order.
    pub(crate) fn folder_rows(&self) -> Vec<FolderRow> {
        self.front()
            .map_or_else(Vec::new, |front| folder_rows(&front.suggestion))
    }

    /// The footer buttons as shown: selector and label, in order.
    pub(crate) fn buttons() -> Vec<(String, String)> {
        SuggestionButton::ALL
            .into_iter()
            .map(|button| (button.selector().to_owned(), button.label().to_owned()))
            .collect()
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;
    use protocol::VscodeWorkspaceFolder;

    fn suggestion(name: &str, folders: usize) -> VscodeWorkspaceSuggestion {
        VscodeWorkspaceSuggestion {
            source_path: "C:/work/team.code-workspace".to_owned(),
            suggested_name: name.to_owned(),
            folders: (0..folders)
                .map(|index| VscodeWorkspaceFolder {
                    path: format!("C:/work/repo{index}"),
                    name: None,
                    matched_repo_id: (index == 0).then(|| "r1".to_owned()),
                })
                .collect(),
        }
    }

    #[test]
    fn pushes_queue_and_dismissing_shows_the_next() {
        let mut queue = VscodeSuggestions::default();
        assert!(queue.is_empty());
        queue.push("r1".to_owned(), suggestion("first", 2));
        queue.push("r2".to_owned(), suggestion("second", 1));
        assert_eq!(queue.len(), 2);
        assert_eq!(
            queue
                .front()
                .expect("the first shows")
                .suggestion
                .suggested_name,
            "first",
            "the queue is FIFO"
        );
        queue.dismiss();
        assert_eq!(
            queue.front().expect("the second shows").repo_id,
            "r2",
            "dismissing the front shows the next"
        );
        queue.dismiss();
        assert!(queue.is_empty());
    }

    #[test]
    fn the_buttons_answer_with_the_shown_suggestion() {
        let mut queue = VscodeSuggestions::default();
        queue.push("r1".to_owned(), suggestion("team", 2));
        assert!(queue.message(SuggestionButton::NotNow).is_none());
        let Some(ClientMessage::AcceptVscodeWorkspaceSuggestion {
            suggestion: create,
            watch,
        }) = queue.message(SuggestionButton::Create)
        else {
            panic!("Create answers with an accepted suggestion");
        };
        assert!(!watch, "Create does not watch");
        assert_eq!(create, suggestion("team", 2));
        let Some(ClientMessage::AcceptVscodeWorkspaceSuggestion {
            suggestion: watch_message,
            watch,
        }) = queue.message(SuggestionButton::CreateWatch)
        else {
            panic!("Create & watch answers with an accepted suggestion");
        };
        assert!(watch, "Create & watch sets the flag");
        assert_eq!(watch_message, suggestion("team", 2));
        assert_eq!(queue.len(), 1, "messaging does not dismiss");
    }

    #[test]
    fn the_body_names_the_file_its_folders_and_the_workspace() {
        let mut queue = VscodeSuggestions::default();
        queue.push("r1".to_owned(), suggestion("team", 2));
        assert_eq!(
            queue.body(),
            "C:/work/team.code-workspace references 2 folders. Create a rustling-tulip workspace named team with these members?"
        );
        let parts = queue.body_parts();
        assert_eq!(
            parts,
            [
                ("C:/work/team.code-workspace".to_owned(), BodyStyle::Code),
                (
                    " references 2 folders. Create a rustling-tulip workspace named ".to_owned(),
                    BodyStyle::Text
                ),
                ("team".to_owned(), BodyStyle::Bold),
                (" with these members?".to_owned(), BodyStyle::Text),
            ],
            "the path draws as code, the name bold, the rest as text"
        );
        assert_eq!(
            parts.into_iter().map(|(text, _)| text).collect::<String>(),
            queue.body(),
            "body() is exactly the drawn parts joined, so they cannot diverge"
        );
        assert_eq!(
            VscodeSuggestions::buttons(),
            [
                ("vscode-prompt-not-now".to_owned(), "Not now".to_owned()),
                ("vscode-prompt-create".to_owned(), "Create".to_owned()),
                (
                    "vscode-prompt-create-watch".to_owned(),
                    "Create & watch".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn focus_moves_along_the_buttons_and_wraps() {
        let mut queue = VscodeSuggestions::default();
        queue.push("r1".to_owned(), suggestion("team", 2));
        assert_eq!(queue.focus(), SuggestionButton::NotNow);
        queue.move_focus(true);
        assert_eq!(queue.focus(), SuggestionButton::Create);
        queue.move_focus(true);
        assert_eq!(queue.focus(), SuggestionButton::CreateWatch);
        queue.move_focus(true);
        assert_eq!(queue.focus(), SuggestionButton::NotNow, "it wraps");
        queue.move_focus(false);
        assert_eq!(queue.focus(), SuggestionButton::CreateWatch);
        queue.dismiss();
        assert_eq!(
            queue.focus(),
            SuggestionButton::NotNow,
            "the next prompt starts on Not now"
        );
    }

    #[test]
    fn the_folder_rows_badge_what_is_registered() {
        let mut queue = VscodeSuggestions::default();
        queue.push("r1".to_owned(), suggestion("team", 2));
        let rows = queue.folder_rows();
        assert_eq!(rows[0].label, "repo0");
        assert_eq!(rows[0].badge(), "registered");
        assert_eq!(rows[1].badge(), "will register");
    }
}
