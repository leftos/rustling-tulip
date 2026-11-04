//! The Default shell row of a host's Settings page, as plain state: the
//! host's saved choice and its choices as its daemon reported them, which
//! entries can be picked, the note under the row, and when a pick is a
//! pending edit. The GPUI view only draws this and forwards events.

use protocol::ShellChoice;

/// The id of the Custom choice, the one that carries a command line.
pub const CUSTOM: &str = "custom";

/// The id of the Automatic choice, what a host with nothing saved runs.
pub const AUTOMATIC: &str = "auto";

/// What the closed dropdown shows before the daemon lists its choices.
pub const LOADING: &str = "Loading shells…";

/// The tooltip of the row on a daemon without the `default_shell` feature.
pub const UNSUPPORTED_TIP: &str = "Update this host's daemon to choose a default shell";

/// The Custom command field's placeholder.
pub const CUSTOM_PLACEHOLDER: &str = "Command line, for example pwsh -NoLogo";

/// A default shell: a choice's id and, for Custom, its command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSetting {
    pub id: String,
    pub command: Option<String>,
}

impl ShellSetting {
    #[must_use]
    pub fn new(id: &str, command: Option<&str>) -> Self {
        Self {
            id: id.to_owned(),
            command: command.map(str::to_owned),
        }
    }

    /// The setting as Apply sends it and a pick is compared: the command
    /// only for Custom, trimmed, and a blank one as none.
    #[must_use]
    pub fn normalized(&self) -> Self {
        let command = if self.id == CUSTOM {
            self.command
                .as_deref()
                .map(str::trim)
                .filter(|command| !command.is_empty())
        } else {
            None
        };
        Self::new(&self.id, command)
    }
}

/// One host's Default shell row as its daemon last reported it, and the
/// apply it waits on. Empty until the daemon reports; goes with the
/// connection.
#[derive(Debug, Default)]
pub struct HostShell {
    /// The host's effective default shell, from its last `DefaultShell`.
    pub saved: Option<ShellSetting>,
    /// The host's choices, from its last `ShellChoices`, in its order.
    pub choices: Option<Vec<ShellChoice>>,
    /// The set Apply sent, with its request id, until its echo or refusal.
    pub applying: Option<(String, ShellSetting)>,
}

impl HostShell {
    /// The daemon's `DefaultShell`: the saved choice, which also answers
    /// the apply waiting. Returns whether the choices are to be asked for
    /// again: always, since Custom's `hosts_agents` is worked out from the
    /// saved command when the daemon lists the choices.
    pub fn on_saved(&mut self, saved: ShellSetting) -> bool {
        self.applying = None;
        self.saved = Some(saved);
        true
    }

    /// The daemon's `ShellChoices`, in its order.
    pub fn on_choices(&mut self, choices: Vec<ShellChoice>) {
        self.choices = Some(choices);
    }

    /// Apply sent `setting` under `request_id`.
    pub fn on_apply(&mut self, request_id: String, setting: ShellSetting) {
        self.applying = Some((request_id, setting));
    }

    /// A refusal carrying `request_id`: when it answers the apply waiting,
    /// that apply ends. Returns whether it did.
    pub fn on_refused(&mut self, request_id: &str) -> bool {
        if self
            .applying
            .as_ref()
            .is_none_or(|(waiting, _)| waiting != request_id)
        {
            return false;
        }
        self.applying = None;
        true
    }

    /// The saved Custom command, empty when the saved shell is not Custom.
    #[must_use]
    pub fn saved_command(&self) -> String {
        self.saved
            .as_ref()
            .filter(|saved| saved.id == CUSTOM)
            .and_then(|saved| saved.command.clone())
            .unwrap_or_default()
    }

    /// The setting the row shows: `pending`, else the one being applied,
    /// else the saved one.
    #[must_use]
    pub fn shown<'a>(&'a self, pending: Option<&'a ShellSetting>) -> Option<&'a ShellSetting> {
        pending
            .or_else(|| self.applying.as_ref().map(|(_, setting)| setting))
            .or(self.saved.as_ref())
    }
}

/// Whether the row takes input, and why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    /// The host's daemon stores no default shell: drawn disabled with
    /// [`UNSUPPORTED_TIP`].
    Unsupported,
    /// The connection is not open, or the choices have not arrived: the
    /// dropdown shows [`LOADING`], dimmed, and does not open.
    Loading,
    /// The dropdown opens.
    Live,
}

/// The row's state on a connection that is `open`, whose daemon
/// `supported` the feature, with the choices held or not.
#[must_use]
pub const fn row_state(open: bool, supported: bool, has_choices: bool) -> RowState {
    if !open {
        RowState::Loading
    } else if !supported {
        RowState::Unsupported
    } else if has_choices {
        RowState::Live
    } else {
        RowState::Loading
    }
}

/// One entry of the open list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellEntry {
    pub id: String,
    pub label: String,
    /// The shell is on the host: the entry is drawn plain and takes a pick.
    pub pickable: bool,
    /// It is the shown choice: it carries a check.
    pub chosen: bool,
    /// The tooltip of an entry that cannot be picked.
    pub tip: Option<String>,
}

/// The tooltip of a choice the host `host` does not have.
#[must_use]
pub fn unavailable_tip(host: &str) -> String {
    format!("Not installed on {host}")
}

/// The open list's entries for `choices`, in their order, `chosen` checked,
/// on host `host`.
#[must_use]
pub fn entries(choices: &[ShellChoice], chosen: Option<&str>, host: &str) -> Vec<ShellEntry> {
    choices
        .iter()
        .map(|choice| ShellEntry {
            id: choice.id.clone(),
            label: choice.label.clone(),
            pickable: choice.available,
            chosen: chosen.unwrap_or(AUTOMATIC) == choice.id,
            tip: (!choice.available).then(|| unavailable_tip(host)),
        })
        .collect()
}

/// Whether the choice `id` among `choices` takes a pick: it is listed and
/// the host has it.
#[must_use]
pub fn can_pick(choices: Option<&[ShellChoice]>, id: &str) -> bool {
    choices
        .into_iter()
        .flatten()
        .any(|choice| choice.id == id && choice.available)
}

/// The closed dropdown's text: [`LOADING`] before the choices arrive, else
/// the label of `chosen`, or its id when the list does not name it.
#[must_use]
pub fn chosen_label(choices: Option<&[ShellChoice]>, chosen: Option<&str>) -> String {
    let Some(choices) = choices else {
        return LOADING.to_owned();
    };
    let id = chosen.unwrap_or(AUTOMATIC);
    choices
        .iter()
        .find(|choice| choice.id == id)
        .map_or_else(|| id.to_owned(), |choice| choice.label.clone())
}

/// The note under the row: shown exactly when the `chosen` choice does not
/// host agents, naming it; none before the choices arrive.
#[must_use]
pub fn agents_note(choices: Option<&[ShellChoice]>, chosen: Option<&str>) -> Option<String> {
    let id = chosen.unwrap_or(AUTOMATIC);
    let choice = choices?.iter().find(|choice| choice.id == id)?;
    (!choice.hosts_agents)
        .then(|| format!("Agents start on their own, not inside {}.", choice.label))
}

/// The pick of choice `id`, carrying `command` when it is Custom, as Apply
/// would send it.
#[must_use]
pub fn pick(id: &str, command: &str) -> ShellSetting {
    ShellSetting::new(id, Some(command)).normalized()
}

/// Whether `pick` is a pending edit against `saved` (nothing saved reads as
/// Automatic): another choice, or Custom with another command.
#[must_use]
pub fn is_dirty(saved: Option<&ShellSetting>, pick: &ShellSetting) -> bool {
    let saved = saved.map_or_else(
        || ShellSetting::new(AUTOMATIC, None),
        ShellSetting::normalized,
    );
    saved != pick.normalized()
}

/// What a pick of `pick` holds as the host's pending edit, against what
/// `shell`'s row shows without one; `None` when it holds nothing.
#[must_use]
pub fn pending_value(shell: &HostShell, pick: &ShellSetting) -> Option<ShellSetting> {
    let pick = pick.normalized();
    if pick.id == CUSTOM && pick.command.is_none() {
        return None;
    }
    is_dirty(shell.shown(None), &pick).then_some(pick)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;

    fn choice(id: &str, label: &str, available: bool, hosts_agents: bool) -> ShellChoice {
        ShellChoice {
            id: id.to_owned(),
            label: label.to_owned(),
            available,
            path: None,
            hosts_agents,
        }
    }

    /// Automatic, PowerShell 7, WSL (no agents), fish (missing), Custom.
    fn listed(custom_hosts_agents: bool) -> Vec<ShellChoice> {
        vec![
            choice("auto", "Automatic", true, true),
            choice("pwsh", "PowerShell 7", true, true),
            choice("wsl", "WSL", true, false),
            choice("fish", "fish", false, false),
            choice("custom", "Custom", true, custom_hosts_agents),
        ]
    }

    #[test]
    fn note_shown_when_choice_does_not_host_agents() {
        let choices = listed(true);
        assert_eq!(
            agents_note(Some(&choices), Some("wsl")).as_deref(),
            Some("Agents start on their own, not inside WSL.")
        );
    }

    #[test]
    fn note_hidden_when_choice_hosts_agents() {
        let choices = listed(true);
        assert_eq!(agents_note(Some(&choices), Some("pwsh")), None);
        assert_eq!(agents_note(Some(&choices), Some("auto")), None);
        assert_eq!(
            agents_note(Some(&choices), None),
            None,
            "nothing saved reads as Automatic"
        );
    }

    #[test]
    fn note_hidden_before_choices_arrive() {
        assert_eq!(agents_note(None, Some("wsl")), None);
        assert_eq!(chosen_label(None, Some("wsl")), LOADING);
        assert_eq!(row_state(true, true, false), RowState::Loading);
        assert_eq!(row_state(true, true, true), RowState::Live);
        assert_eq!(row_state(true, false, true), RowState::Unsupported);
        assert_eq!(row_state(false, true, true), RowState::Loading);
    }

    #[test]
    fn custom_note_follows_its_hosts_agents() {
        let hosting = listed(true);
        assert_eq!(agents_note(Some(&hosting), Some(CUSTOM)), None);
        let not_hosting = listed(false);
        assert_eq!(
            agents_note(Some(&not_hosting), Some(CUSTOM)).as_deref(),
            Some("Agents start on their own, not inside Custom.")
        );
    }

    #[test]
    fn unavailable_choice_cannot_be_picked() {
        let choices = listed(true);
        assert!(!can_pick(Some(&choices), "fish"));
        assert!(can_pick(Some(&choices), "wsl"));
        assert!(!can_pick(Some(&choices), "nushell"), "an unlisted id");
        assert!(!can_pick(None, "pwsh"), "nothing before the choices arrive");
        let rows = entries(&choices, Some("pwsh"), "box");
        let fish = rows.iter().find(|row| row.id == "fish").expect("listed");
        assert!(!fish.pickable);
        assert_eq!(fish.tip.as_deref(), Some("Not installed on box"));
        let pwsh = rows.iter().find(|row| row.id == "pwsh").expect("listed");
        assert!(pwsh.pickable && pwsh.chosen && pwsh.tip.is_none());
        let order: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(order, ["auto", "pwsh", "wsl", "fish", "custom"]);
    }

    #[test]
    fn pending_edit_dirty_only_when_choice_or_command_differs() {
        let saved = ShellSetting::new("pwsh", None);
        assert!(!is_dirty(Some(&saved), &pick("pwsh", "ignored")));
        assert!(is_dirty(Some(&saved), &pick("wsl", "")));
        assert!(
            !is_dirty(None, &pick("auto", "")),
            "nothing saved is Automatic"
        );
        let custom = ShellSetting::new(CUSTOM, Some("pwsh -NoLogo"));
        assert!(!is_dirty(Some(&custom), &pick(CUSTOM, "  pwsh -NoLogo ")));
        assert!(is_dirty(Some(&custom), &pick(CUSTOM, "nu")));
        assert_eq!(pick(CUSTOM, "  "), ShellSetting::new(CUSTOM, None));
        assert_eq!(pick("pwsh", "nu"), ShellSetting::new("pwsh", None));
    }

    #[test]
    fn pick_saved_shell_while_apply_in_flight_is_pending() {
        let mut shell = HostShell::default();
        shell.on_saved(ShellSetting::new("pwsh", None));
        assert_eq!(
            pending_value(&shell, &pick("pwsh", "")),
            None,
            "the saved shell"
        );
        shell.on_apply("r1".to_owned(), ShellSetting::new("wsl", None));
        assert_eq!(
            pending_value(&shell, &pick("pwsh", "")),
            Some(ShellSetting::new("pwsh", None)),
            "WSL is on its way: picking PowerShell 7 again is an edit"
        );
        assert_eq!(
            pending_value(&shell, &pick("wsl", "")),
            None,
            "the shell being applied"
        );
    }

    #[test]
    fn custom_with_blank_command_is_not_pending() {
        let mut shell = HostShell::default();
        shell.on_saved(ShellSetting::new("pwsh", None));
        assert_eq!(pending_value(&shell, &pick(CUSTOM, "")), None);
        assert_eq!(pending_value(&shell, &pick(CUSTOM, "   ")), None);
        assert_eq!(
            pending_value(&shell, &pick(CUSTOM, " nu ")),
            Some(ShellSetting::new(CUSTOM, Some("nu")))
        );
        shell.on_saved(ShellSetting::new(CUSTOM, Some("nu")));
        assert_eq!(
            pending_value(&shell, &pick(CUSTOM, "")),
            None,
            "a cleared field over a saved command holds nothing either"
        );
    }

    #[test]
    fn refetch_wanted_after_default_shell_echo() {
        let mut shell = HostShell::default();
        shell.on_choices(listed(true));
        shell.on_apply("r1".to_owned(), ShellSetting::new(CUSTOM, Some("nu")));
        assert!(shell.on_saved(ShellSetting::new(CUSTOM, Some("nu"))));
        assert_eq!(shell.applying, None, "the echo answers the apply");
        assert_eq!(shell.saved, Some(ShellSetting::new(CUSTOM, Some("nu"))));
        assert!(
            shell.on_saved(ShellSetting::new("pwsh", None)),
            "another client's set makes the list stale too"
        );
    }
}
