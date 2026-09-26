//! The confirm a discard opens before it sends: the paths it would throw
//! away, Cancel and the danger button, with Cancel focused first.

use crate::source_control::ScKey;

/// A button of the discard confirm, in Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscardButton {
    Cancel,
    Discard,
}

impl DiscardButton {
    pub(crate) const ALL: [Self; 2] = [Self::Cancel, Self::Discard];

    pub(crate) fn selector(self) -> &'static str {
        match self {
            Self::Cancel => "discard-confirm-cancel",
            Self::Discard => "discard-confirm-discard",
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Cancel => Self::Discard,
            Self::Discard => Self::Cancel,
        }
    }
}

/// The open discard confirm: the tree and paths it would discard, and the
/// focused button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscardConfirm {
    key: ScKey,
    paths: Vec<String>,
    focused: DiscardButton,
}

impl DiscardConfirm {
    pub(crate) fn new(key: ScKey, paths: Vec<String>) -> Self {
        Self {
            key,
            paths,
            focused: DiscardButton::Cancel,
        }
    }

    pub(crate) fn tree(&self) -> &ScKey {
        &self.key
    }

    pub(crate) fn paths(&self) -> &[String] {
        &self.paths
    }

    /// `Discard 1 change` or `Discard N changes`: the heading and the danger
    /// button's label.
    pub(crate) fn title(&self) -> String {
        match self.paths.len() {
            1 => "Discard 1 change".to_owned(),
            count => format!("Discard {count} changes"),
        }
    }

    pub(crate) fn focused(&self) -> DiscardButton {
        self.focused
    }

    /// A key while the confirm is open; returns the button it presses. Esc
    /// presses Cancel, Enter the focused button, and Tab and Shift+Tab move
    /// the focus to the other button.
    pub(crate) fn key(&mut self, key: &str) -> Option<DiscardButton> {
        match key {
            "escape" => Some(DiscardButton::Cancel),
            "enter" => Some(self.focused),
            "tab" => {
                self.focused = self.focused.other();
                None
            }
            _ => None,
        }
    }
}
