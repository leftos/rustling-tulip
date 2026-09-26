//! The confirm a terminal link to a file that runs code opens instead of
//! running it: Cancel, Show in Explorer or Run, with Cancel focused first.

use std::path::{Path, PathBuf};

/// A button of the run confirm, in Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunButton {
    Cancel,
    Reveal,
    Run,
}

impl RunButton {
    /// Every button, left to right.
    pub(crate) const ALL: [Self; 3] = [Self::Cancel, Self::Reveal, Self::Run];

    pub(crate) fn selector(self) -> &'static str {
        match self {
            Self::Cancel => "run-confirm-cancel",
            Self::Reveal => "run-confirm-reveal",
            Self::Run => "run-confirm-run",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Cancel => "Cancel",
            Self::Reveal => "Show in Explorer",
            Self::Run => "Run",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Cancel => 0,
            Self::Reveal => 1,
            Self::Run => 2,
        }
    }
}

/// The open run confirm: the file it asks about and the focused button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunConfirm {
    path: PathBuf,
    focused: RunButton,
}

impl RunConfirm {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            focused: RunButton::Cancel,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// `Run <file name>?`
    pub(crate) fn title(&self) -> String {
        let name = self.path.file_name().map_or_else(
            || self.path.to_string_lossy(),
            |name| name.to_string_lossy(),
        );
        format!("Run {name}?")
    }

    /// The file's full path.
    pub(crate) fn detail(&self) -> String {
        self.path.display().to_string()
    }

    pub(crate) fn focused(&self) -> RunButton {
        self.focused
    }

    /// A key while the confirm is open; returns the button it presses. Esc
    /// presses Cancel, Enter and Space the focused button, and Tab and
    /// Shift+Tab move the focus round the buttons.
    pub(crate) fn key(&mut self, key: &str, shift: bool) -> Option<RunButton> {
        match key {
            "escape" => Some(RunButton::Cancel),
            "enter" | "space" => Some(self.focused),
            "tab" => {
                let count = RunButton::ALL.len();
                let step = if shift { count - 1 } else { 1 };
                let next = (self.focused.index() + step) % count;
                self.focused = RunButton::ALL[next];
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{RunButton, RunConfirm};

    fn confirm() -> RunConfirm {
        RunConfirm::new(PathBuf::from(r"C:\repo\tools\run.bat"))
    }

    #[test]
    fn it_names_the_file_and_shows_its_path() {
        let confirm = confirm();
        assert_eq!(confirm.title(), "Run run.bat?");
        assert_eq!(
            confirm.detail(),
            PathBuf::from(r"C:\repo\tools\run.bat")
                .display()
                .to_string()
        );
    }

    #[test]
    fn cancel_has_the_focus_first() {
        assert_eq!(confirm().focused(), RunButton::Cancel);
        assert_eq!(confirm().key("enter", false), Some(RunButton::Cancel));
    }

    #[test]
    fn tab_cycles_the_buttons_both_ways() {
        let mut confirm = confirm();
        let mut forward = Vec::new();
        for _ in 0..3 {
            assert_eq!(confirm.key("tab", false), None);
            forward.push(confirm.focused());
        }
        assert_eq!(
            forward,
            [RunButton::Reveal, RunButton::Run, RunButton::Cancel]
        );
        assert_eq!(confirm.key("tab", true), None);
        assert_eq!(confirm.focused(), RunButton::Run);
        assert_eq!(confirm.key("tab", true), None);
        assert_eq!(confirm.focused(), RunButton::Reveal);
    }

    #[test]
    fn enter_and_space_press_the_focused_button() {
        let mut confirm = confirm();
        confirm.key("tab", false);
        assert_eq!(confirm.key("space", false), Some(RunButton::Reveal));
        confirm.key("tab", false);
        assert_eq!(confirm.key("enter", false), Some(RunButton::Run));
    }

    #[test]
    fn escape_cancels_whatever_has_the_focus() {
        let mut confirm = confirm();
        confirm.key("tab", true);
        assert_eq!(confirm.focused(), RunButton::Run);
        assert_eq!(confirm.key("escape", false), Some(RunButton::Cancel));
    }

    #[test]
    fn other_keys_do_nothing() {
        let mut confirm = confirm();
        assert_eq!(confirm.key("r", false), None);
        assert_eq!(confirm.focused(), RunButton::Cancel);
    }
}
