//! The Add repo prompt on a remote host: a folder path typed on the host
//! instead of this PC's folder picker. Its title, hint and placeholder name
//! the host, Add checks the path's text ([`RepoAdd::from_host_path`]) and a
//! refusal shows under the field until the next edit. Plain Rust, so every
//! rule is unit-tested; `host_path_prompt_view` renders it and forwards
//! input.

use crate::repo_add::RepoAdd;

/// A focusable control of the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptControl {
    Path,
    Cancel,
    Add,
}

impl PromptControl {
    /// The controls in reading and Tab order.
    const RING: [Self; 3] = [Self::Path, Self::Cancel, Self::Add];

    /// The debug selector the control's element carries.
    pub(crate) const fn selector(self) -> &'static str {
        match self {
            Self::Path => "host-path-input",
            Self::Cancel => "host-path-cancel",
            Self::Add => "host-path-add",
        }
    }
}

/// The prompt's state. The path itself lives in the prompt's text field,
/// and every rule here reads the text it is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostPathPrompt {
    /// The connection's short name, which every text names.
    host: String,
    focus: PromptControl,
    error: Option<String>,
}

impl HostPathPrompt {
    /// The prompt for `host`, the path field holding the keyboard.
    pub(crate) fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            focus: PromptControl::Path,
            error: None,
        }
    }

    pub(crate) fn title(&self) -> String {
        format!("Add a repo on {}", self.host)
    }

    pub(crate) fn placeholder(&self) -> String {
        format!("Full path on {}", self.host)
    }

    pub(crate) fn hint(&self) -> String {
        format!("A folder on {} that holds a git repo.", self.host)
    }

    pub(crate) fn focus(&self) -> PromptControl {
        self.focus
    }

    pub(crate) fn set_focus(&mut self, control: PromptControl) {
        self.focus = control;
    }

    /// Tab (`forward`) or Shift+Tab: the next or previous control, wrapping.
    pub(crate) fn move_focus(&mut self, forward: bool) {
        let ring = PromptControl::RING;
        let len = ring.len();
        let at = ring.iter().position(|c| *c == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % len
        } else {
            (at + len - 1) % len
        };
        self.focus = ring[next];
    }

    /// Add: the repo `text` registers, or `None` with the refusal under the
    /// field and the keyboard back in it.
    pub(crate) fn submit(&mut self, text: &str) -> Option<RepoAdd> {
        match RepoAdd::from_host_path(text) {
            Ok(add) => {
                self.error = None;
                Some(add)
            }
            Err(problem) => {
                self.error = Some(problem.message(&self.host));
                self.focus = PromptControl::Path;
                None
            }
        }
    }

    /// The path was edited, which clears the refusal under it.
    pub(crate) fn edited(&mut self) {
        self.error = None;
    }

    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use protocol::ClientMessage;

    use super::{HostPathPrompt, PromptControl};

    #[test]
    fn texts_name_the_host() {
        let prompt = HostPathPrompt::new("box");
        assert_eq!(prompt.title(), "Add a repo on box");
        assert_eq!(prompt.placeholder(), "Full path on box");
        assert_eq!(prompt.hint(), "A folder on box that holds a git repo.");
    }

    #[test]
    fn a_refused_path_shows_why_under_the_field() {
        let mut prompt = HostPathPrompt::new("box");
        prompt.set_focus(PromptControl::Add);
        assert_eq!(prompt.submit("work\\repo"), None);
        assert_eq!(
            prompt.error(),
            Some("Use a full path on box, starting with a drive letter, \\\\server or /.")
        );
        assert_eq!(prompt.focus(), PromptControl::Path, "back to the field");
        assert_eq!(prompt.submit(" "), None);
        assert_eq!(prompt.error(), Some("Type a folder path on box."));
    }

    #[test]
    fn an_edit_clears_the_refusal() {
        let mut prompt = HostPathPrompt::new("box");
        assert_eq!(prompt.submit("C:\\a\\..\\b"), None);
        assert_eq!(prompt.error(), Some("Use a path without .. in it."));
        prompt.edited();
        assert_eq!(prompt.error(), None);
    }

    #[test]
    fn add_sends_the_trimmed_path() {
        let mut prompt = HostPathPrompt::new("box");
        let add = prompt.submit("  D:\\work\\repo  ").expect("an add");
        assert!(
            matches!(
                add.message(),
                ClientMessage::AddRepo { path, name: Some(name) }
                    if path == "D:\\work\\repo" && name == "repo"
            ),
            "{:?}",
            add.message()
        );
        assert_eq!(add.last_dir, None, "nothing to remember");
        assert_eq!(prompt.error(), None);
    }

    #[test]
    fn the_focus_ring_wraps() {
        let mut prompt = HostPathPrompt::new("box");
        assert_eq!(prompt.focus(), PromptControl::Path);
        prompt.move_focus(true);
        assert_eq!(prompt.focus(), PromptControl::Cancel);
        prompt.move_focus(true);
        assert_eq!(prompt.focus(), PromptControl::Add);
        prompt.move_focus(true);
        assert_eq!(prompt.focus(), PromptControl::Path, "Tab wraps");
        prompt.move_focus(false);
        assert_eq!(prompt.focus(), PromptControl::Add, "and Shift+Tab");
        assert_eq!(PromptControl::Add.selector(), "host-path-add");
    }
}
