//! The workspace creator's plain state: its two modes, the name text, the
//! ticked members, the picked `.code-workspace` file and its parsed
//! suggestion, the inline error and the wait for the daemon.
//!
//! The view ([`crate::workspace_creator_view`]) only renders what this holds;
//! the submit gates, the Tab order, the messages and the wait's end are
//! decided and unit-tested here.

use std::path::Path;

use protocol::{
    ClientMessage, RepoEntry, VscodeWorkspaceFolder, VscodeWorkspaceSuggestion, WorkspaceEntry,
};

/// The creator's title.
pub(crate) const TITLE: &str = "New workspace";
/// The placeholder of the name field.
pub(crate) const NAME_PLACEHOLDER: &str = "e.g. yaat-stack";
/// The fallback inline error when the daemon's message carries none.
pub(crate) const PARSE_FAILED: &str = "Failed to parse workspace file.";
/// The label of the From-repos members field.
pub(crate) const MEMBERS_LABEL: &str = "Members (pick at least 2)";
/// The From-repos note while no repo is registered.
pub(crate) const NO_REPOS_NOTE: &str = "No repos registered yet.";
/// The VS Code file field's label.
pub(crate) const FILE_LABEL: &str = "VS Code workspace file";
/// What the file field shows while no file is chosen.
pub(crate) const NO_FILE: &str = "No file selected";
/// What the creator says while a parse is in flight.
pub(crate) const PARSING: &str = "Parsing workspace file…";
/// The folder field's label under a parsed suggestion.
pub(crate) const WORKSPACE_NAME_LABEL: &str = "Workspace name";

/// Which half of the creator is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreatorMode {
    FromRepos,
    FromVscode,
}

impl CreatorMode {
    /// The mode tabs in reading and Tab order.
    pub(crate) const ALL: [Self; 2] = [Self::FromRepos, Self::FromVscode];

    pub(crate) const fn selector(self) -> &'static str {
        match self {
            Self::FromRepos => "workspace-creator-tab-repos",
            Self::FromVscode => "workspace-creator-tab-vscode",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::FromRepos => "From repos",
            Self::FromVscode => "From VS Code workspace",
        }
    }
}

/// A keyboard stop of the creator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreatorControl {
    FromRepos,
    FromVscode,
    Name,
    Browse,
    Cancel,
    Create,
}

impl CreatorControl {
    pub(crate) const fn selector(self) -> &'static str {
        match self {
            Self::FromRepos => CreatorMode::FromRepos.selector(),
            Self::FromVscode => CreatorMode::FromVscode.selector(),
            Self::Name => "workspace-creator-name",
            Self::Browse => "workspace-creator-browse",
            Self::Cancel => "workspace-creator-cancel",
            Self::Create => "workspace-creator-create",
        }
    }
}

/// How the creator's wait for the daemon ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Settled {
    /// A workspace carrying the submitted name arrived: the creator closes.
    Closed,
    /// The daemon refused the submit; the creator stays open with the error.
    Refused(String),
    /// Nothing settled it yet.
    Pending,
}

/// One row of a parsed file's folder list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FolderRow {
    pub(crate) label: String,
    pub(crate) registered: bool,
}

impl FolderRow {
    /// The badge the row draws: registered already, or registered on accept.
    pub(crate) const fn badge(&self) -> &'static str {
        if self.registered {
            "registered"
        } else {
            "will register"
        }
    }
}

/// The rows `suggestion`'s folders draw, in file order: the folder's `name`
/// when it carries one, else its leaf.
pub(crate) fn folder_rows(suggestion: &VscodeWorkspaceSuggestion) -> Vec<FolderRow> {
    suggestion
        .folders
        .iter()
        .map(|folder| FolderRow {
            label: folder_label(folder),
            registered: folder.matched_repo_id.is_some(),
        })
        .collect()
}

/// Whether two paths name the same file: separators and case do not matter
/// on Windows, where the two forms name the same folder.
fn paths_eq(a: &str, b: &str) -> bool {
    a.replace('\\', "/").to_lowercase() == b.replace('\\', "/").to_lowercase()
}

/// A folder's label: its `name`, else the last component of its path.
fn folder_label(folder: &VscodeWorkspaceFolder) -> String {
    folder.name.clone().unwrap_or_else(|| {
        Path::new(&folder.path).file_name().map_or_else(
            || folder.path.clone(),
            |leaf| leaf.to_string_lossy().into_owned(),
        )
    })
}

/// The creator's state.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceCreator {
    mode: CreatorMode,
    focus: CreatorControl,
    name: String,
    checked: Vec<String>,
    file: Option<String>,
    parsing: bool,
    suggestion: Option<VscodeWorkspaceSuggestion>,
    error: Option<String>,
    /// The trimmed name sent with Create, until a `Workspaces` broadcast holds
    /// it.
    pending: Option<String>,
}

impl Default for WorkspaceCreator {
    fn default() -> Self {
        Self {
            mode: CreatorMode::FromRepos,
            focus: CreatorControl::FromRepos,
            name: String::new(),
            checked: Vec::new(),
            file: None,
            parsing: false,
            suggestion: None,
            error: None,
            pending: None,
        }
    }
}

impl WorkspaceCreator {
    pub(crate) fn mode(&self) -> CreatorMode {
        self.mode
    }

    /// Switches the visible half, landing the keyboard on its tab.
    pub(crate) fn set_mode(&mut self, mode: CreatorMode) {
        self.mode = mode;
        self.focus = match mode {
            CreatorMode::FromRepos => CreatorControl::FromRepos,
            CreatorMode::FromVscode => CreatorControl::FromVscode,
        };
    }

    pub(crate) fn focus(&self) -> CreatorControl {
        self.focus
    }

    pub(crate) fn set_focus(&mut self, control: CreatorControl) {
        if self.controls().contains(&control) {
            self.focus = control;
        }
    }

    /// The keyboard stops on screen, in reading and Tab order.
    pub(crate) fn controls(&self) -> Vec<CreatorControl> {
        let mut controls = vec![CreatorControl::FromRepos, CreatorControl::FromVscode];
        match self.mode {
            CreatorMode::FromRepos => controls.push(CreatorControl::Name),
            CreatorMode::FromVscode => {
                controls.push(CreatorControl::Browse);
                if self.suggestion.is_some() {
                    controls.push(CreatorControl::Name);
                }
            }
        }
        controls.push(CreatorControl::Cancel);
        controls.push(CreatorControl::Create);
        controls
    }

    /// Tab and Shift+Tab: the next or previous stop, wrapping.
    pub(crate) fn move_focus(&mut self, forward: bool) {
        let controls = self.controls();
        let count = controls.len();
        let index = controls
            .iter()
            .position(|control| *control == self.focus)
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % count
        } else {
            (index + count - 1) % count
        };
        self.focus = controls[next];
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn set_name(&mut self, name: impl Into<String>) {
        self.name = name.into();
    }

    fn trimmed_name(&self) -> String {
        self.name.trim().to_owned()
    }

    #[cfg(test)]
    pub(crate) fn checked(&self) -> &[String] {
        &self.checked
    }

    /// Ticks or unticks `repo_id`; the checked list keeps the click order.
    pub(crate) fn toggle_member(&mut self, repo_id: &str) {
        if let Some(index) = self.checked.iter().position(|id| id == repo_id) {
            self.checked.remove(index);
        } else {
            self.checked.push(repo_id.to_owned());
        }
    }

    pub(crate) fn file(&self) -> Option<&str> {
        self.file.as_deref()
    }

    pub(crate) fn parsing(&self) -> bool {
        self.parsing
    }

    pub(crate) fn suggestion(&self) -> Option<&VscodeWorkspaceSuggestion> {
        self.suggestion.as_ref()
    }

    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether Create can send: a trimmed name and, from repos, at least two
    /// members; from a VS Code file, a parsed suggestion. The file's folder
    /// count is not a gate, and a submit already waiting on the daemon blocks
    /// a second one.
    pub(crate) fn can_submit(&self) -> bool {
        if self.pending.is_some() {
            return false;
        }
        let named = !self.trimmed_name().is_empty();
        match self.mode {
            CreatorMode::FromRepos => named && self.checked.len() >= 2,
            CreatorMode::FromVscode => named && self.suggestion.is_some(),
        }
    }

    /// "From repos" Create: the workspace with the ticked members and no
    /// linked file.
    pub(crate) fn create_message(&self) -> Option<ClientMessage> {
        if self.mode != CreatorMode::FromRepos || !self.can_submit() {
            return None;
        }
        Some(ClientMessage::UpsertWorkspace {
            id: None,
            name: self.trimmed_name(),
            member_repo_ids: self.checked.clone(),
            linked_vscode_workspace: None,
        })
    }

    /// "From VS Code" Create: the parsed suggestion under the typed name,
    /// without watching the file.
    pub(crate) fn accept_message(&self) -> Option<ClientMessage> {
        if self.mode != CreatorMode::FromVscode || !self.can_submit() {
            return None;
        }
        let mut suggestion = self.suggestion.clone()?;
        suggestion.suggested_name = self.trimmed_name();
        Some(ClientMessage::AcceptVscodeWorkspaceSuggestion {
            suggestion,
            watch: false,
        })
    }

    /// The message the creator's Create sends, whichever half is on screen.
    pub(crate) fn submit_message(&self) -> Option<ClientMessage> {
        match self.mode {
            CreatorMode::FromRepos => self.create_message(),
            CreatorMode::FromVscode => self.accept_message(),
        }
    }

    /// Arms the wait for the daemon with the name just sent. The view closes
    /// the creator only when [`Self::settle`] says the name arrived.
    pub(crate) fn begin_create(&mut self) {
        self.error = None;
        self.pending = Some(self.trimmed_name());
    }

    /// The picked file, while its parse is in flight.
    pub(crate) fn begin_parse(&mut self, path: String) {
        self.file = Some(path);
        self.parsing = true;
        self.suggestion = None;
        self.error = None;
    }

    /// The daemon's parsed suggestion for the file last sent: accepted only
    /// when `parsing` and the reply names that same file, so a stale reply
    /// from an earlier Browse cannot fill the form with the wrong file.
    /// Returns whether it was accepted.
    pub(crate) fn parsed(&mut self, suggestion: VscodeWorkspaceSuggestion) -> bool {
        if !self.parsing {
            return false;
        }
        let current = self
            .file
            .as_deref()
            .is_some_and(|file| paths_eq(file, &suggestion.source_path));
        if !current {
            return false;
        }
        self.parsing = false;
        self.error = None;
        self.name.clone_from(&suggestion.suggested_name);
        self.suggestion = Some(suggestion);
        true
    }

    /// The parse failed: the form stays usable with the daemon's message (or
    /// the fallback) inline. Returns whether a parse was in flight.
    pub(crate) fn fail_parse(&mut self, message: Option<&str>) -> bool {
        if !self.parsing {
            return false;
        }
        self.parsing = false;
        self.suggestion = None;
        self.error = Some(message.unwrap_or(PARSE_FAILED).to_owned());
        true
    }

    /// The daemon refused the Create: the error shows inline and the typed
    /// name and ticks stay. Returns whether a create was in flight.
    pub(crate) fn refuse(&mut self, message: &str) -> bool {
        if self.pending.take().is_none() {
            return false;
        }
        self.error = Some(message.to_owned());
        true
    }

    /// Whether a Create is waiting for the daemon's workspace list.
    pub(crate) fn has_pending_create(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether a parse or a create waits on the daemon.
    #[cfg(test)]
    pub(crate) fn is_waiting(&self) -> bool {
        self.parsing || self.pending.is_some()
    }

    /// Folds the registry after a `Workspaces` broadcast: closes on the name
    /// just created, or reports the refusal that is showing.
    pub(crate) fn settle(&mut self, workspaces: &[WorkspaceEntry]) -> Settled {
        if let Some(pending) = self.pending.clone() {
            let arrived = workspaces
                .iter()
                .any(|workspace| workspace.name.trim().eq_ignore_ascii_case(&pending));
            if arrived {
                self.pending = None;
                return Settled::Closed;
            }
            return Settled::Pending;
        }
        match &self.error {
            Some(error) => Settled::Refused(error.clone()),
            None => Settled::Pending,
        }
    }

    /// The rows of the parsed file's folders, in file order.
    pub(crate) fn folder_rows(&self) -> Vec<FolderRow> {
        self.suggestion.as_ref().map_or_else(Vec::new, folder_rows)
    }

    /// The repos as the member list draws them: every registered repo, with
    /// whether it is ticked.
    pub(crate) fn member_rows(&self, repos: &[RepoEntry]) -> Vec<(String, String, bool)> {
        repos
            .iter()
            .map(|repo| {
                (
                    format!("workspace-creator-member-{}", repo.id),
                    repo.name.clone(),
                    self.checked.iter().any(|id| id == &repo.id),
                )
            })
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
                    path: format!("C:/work/folder{index}"),
                    name: None,
                    matched_repo_id: None,
                })
                .collect(),
        }
    }

    fn workspace(name: &str) -> WorkspaceEntry {
        serde_json::from_value(serde_json::json!({
            "id": "w1",
            "name": name,
            "member_repo_ids": [],
        }))
        .expect("workspace fixture")
    }

    #[test]
    fn from_repos_needs_a_name_and_two_members() {
        let mut creator = WorkspaceCreator::default();
        assert!(!creator.can_submit(), "nothing typed and nothing ticked");
        creator.set_name("  team  ");
        assert!(!creator.can_submit(), "a name alone is not enough");
        creator.toggle_member("r1");
        assert!(!creator.can_submit(), "one member is not enough");
        creator.toggle_member("r2");
        assert!(creator.can_submit(), "two members and a name");
        creator.set_name("   ");
        assert!(!creator.can_submit(), "a blank name is no name");
    }

    #[test]
    fn from_vscode_needs_a_parsed_file_and_a_name() {
        let mut creator = WorkspaceCreator::default();
        creator.set_mode(CreatorMode::FromVscode);
        assert!(!creator.can_submit(), "no file parsed yet");
        creator.begin_parse("C:/work/team.code-workspace".to_owned());
        assert!(creator.parsed(suggestion("team", 1)));
        assert!(creator.can_submit(), "one folder is allowed here");
        creator.set_name("");
        assert!(!creator.can_submit(), "the name is still required");
        creator.begin_parse("C:/work/team.code-workspace".to_owned());
        assert!(creator.parsed(suggestion("team", 0)));
        creator.set_name("  team ");
        assert!(creator.can_submit(), "zero folders are allowed too");
    }

    #[test]
    fn the_mode_tabs_come_first_in_tab_order() {
        assert_eq!(
            CreatorMode::ALL,
            [CreatorMode::FromRepos, CreatorMode::FromVscode],
            "the tabs read left to right"
        );
        let mut creator = WorkspaceCreator::default();
        assert_eq!(creator.focus(), CreatorControl::FromRepos);
        creator.move_focus(true);
        assert_eq!(
            creator.focus(),
            CreatorControl::FromVscode,
            "the second tab follows the first"
        );
        creator.move_focus(false);
        assert_eq!(creator.focus(), CreatorControl::FromRepos, "and back");

        creator.set_mode(CreatorMode::FromVscode);
        assert_eq!(
            creator.focus(),
            CreatorControl::FromVscode,
            "a mode switch lands on its tab"
        );
        creator.move_focus(true);
        assert_eq!(creator.focus(), CreatorControl::Browse, "then Browse");
        creator.move_focus(true);
        assert_eq!(
            creator.focus(),
            CreatorControl::Cancel,
            "no name field before a file is parsed"
        );

        creator.begin_parse("C:/work/team.code-workspace".to_owned());
        assert!(creator.parsed(suggestion("team", 2)));
        creator.set_focus(CreatorControl::Browse);
        creator.move_focus(true);
        assert_eq!(
            creator.focus(),
            CreatorControl::Name,
            "a parsed file adds its name field after Browse"
        );
    }

    #[test]
    fn from_repos_create_carries_the_ticks_and_no_file() {
        let mut creator = WorkspaceCreator::default();
        creator.set_name("  yaat-stack  ");
        creator.toggle_member("r1");
        creator.toggle_member("r2");
        assert!(
            matches!(
                creator.create_message(),
                Some(ClientMessage::UpsertWorkspace {
                    id: None,
                    name,
                    member_repo_ids,
                    linked_vscode_workspace: None,
                }) if name == "yaat-stack"
                    && member_repo_ids == ["r1".to_owned(), "r2".to_owned()]
            ),
            "the request is the trimmed name, the ticks and no file: {:?}",
            creator.create_message()
        );
        creator.toggle_member("r1");
        creator.toggle_member("r1");
        assert_eq!(
            creator.checked(),
            ["r2".to_owned(), "r1".to_owned()],
            "a toggle re-adds at the end"
        );
    }

    #[test]
    fn from_vscode_create_renames_the_suggestion_without_watching() {
        let mut creator = WorkspaceCreator::default();
        creator.set_mode(CreatorMode::FromVscode);
        creator.begin_parse("C:/work/team.code-workspace".to_owned());
        assert!(creator.parsed(suggestion("team", 2)));
        creator.set_name("  flight-deck ");
        match creator.accept_message() {
            Some(ClientMessage::AcceptVscodeWorkspaceSuggestion { suggestion, watch }) => {
                assert!(!watch, "the creator's Create does not watch");
                assert_eq!(suggestion.suggested_name, "flight-deck");
                assert_eq!(suggestion.source_path, "C:/work/team.code-workspace");
                assert_eq!(suggestion.folders.len(), 2);
            }
            other => panic!("expected an accepted suggestion, got {other:?}"),
        }
        assert!(
            creator.create_message().is_none(),
            "the VS Code half has no UpsertWorkspace"
        );
    }

    #[test]
    fn a_refused_create_stays_open_with_its_error_and_its_form() {
        let mut creator = WorkspaceCreator::default();
        creator.set_name("team");
        creator.toggle_member("r1");
        creator.toggle_member("r2");
        let message = creator.submit_message().expect("the form can submit");
        creator.begin_create();
        assert!(creator.is_waiting());
        assert!(creator.refuse("another workspace already uses the name"));
        assert_eq!(
            creator.error(),
            Some("another workspace already uses the name")
        );
        assert!(!creator.refuse("nothing in flight"), "only one refusal");
        assert_eq!(creator.name(), "team", "the typed name survives");
        assert_eq!(creator.checked(), ["r1", "r2"], "the ticks survive");
        assert_eq!(
            creator.settle(&[]),
            Settled::Refused("another workspace already uses the name".to_owned())
        );
        assert!(matches!(message, ClientMessage::UpsertWorkspace { .. }));
    }

    #[test]
    fn settle_closes_only_on_the_submitted_name() {
        let mut creator = WorkspaceCreator::default();
        creator.set_name("team");
        creator.toggle_member("r1");
        creator.toggle_member("r2");
        creator.begin_create();
        assert_eq!(
            creator.settle(&[workspace("other")]),
            Settled::Pending,
            "another workspace's name does not close it"
        );
        assert_eq!(
            creator.settle(&[workspace(" Team ")]),
            Settled::Closed,
            "trimmed and case-insensitive"
        );
        assert_eq!(
            creator.settle(&[workspace("team")]),
            Settled::Pending,
            "the wait is over once"
        );
    }

    #[test]
    fn a_stale_parse_reply_is_ignored() {
        let mut creator = WorkspaceCreator::default();
        creator.set_mode(CreatorMode::FromVscode);
        creator.begin_parse("C:/work/a.code-workspace".to_owned());
        creator.fail_parse(None);
        creator.begin_parse("C:/work/b.code-workspace".to_owned());
        let mut stale = suggestion("a", 1);
        stale.source_path = "C:/work/a.code-workspace".to_owned();
        assert!(
            !creator.parsed(stale),
            "the earlier file's reply does not fill the form"
        );
        assert!(creator.parsing(), "the current parse is still pending");
        assert_eq!(creator.file(), Some("C:/work/b.code-workspace"));
        let mut current = suggestion("b", 1);
        current.source_path = "c:\\work\\B.code-workspace".to_owned();
        assert!(
            creator.parsed(current),
            "the current file's reply is accepted, case and separators aside"
        );
        assert_eq!(creator.name(), "b");
    }

    #[test]
    fn a_create_in_flight_blocks_a_second_submit() {
        let mut creator = WorkspaceCreator::default();
        creator.set_name("team");
        creator.toggle_member("r1");
        creator.toggle_member("r2");
        assert!(creator.can_submit());
        creator.begin_create();
        assert!(creator.has_pending_create());
        assert!(!creator.can_submit(), "a second Create waits for the first");
        assert!(creator.create_message().is_none());
        assert!(creator.submit_message().is_none());
    }

    #[test]
    fn a_parse_failure_is_inline_and_falls_back() {
        let mut creator = WorkspaceCreator::default();
        creator.set_mode(CreatorMode::FromVscode);
        creator.begin_parse("C:/work/broken.code-workspace".to_owned());
        assert!(creator.is_waiting(), "the parse is in flight");
        assert!(creator.fail_parse(Some("parsing C:/work/broken: key must be a string")));
        assert!(!creator.parsing());
        assert_eq!(
            creator.error(),
            Some("parsing C:/work/broken: key must be a string")
        );
        assert!(!creator.fail_parse(None), "nothing is in flight now");
        creator.begin_parse("C:/work/broken.code-workspace".to_owned());
        creator.fail_parse(None);
        assert_eq!(creator.error(), Some(PARSE_FAILED), "the fallback");
        assert!(
            creator.file().is_some(),
            "the picked file stays for another try"
        );
    }

    #[test]
    fn a_parse_prefills_the_name_and_fills_the_folder_rows() {
        let mut creator = WorkspaceCreator::default();
        creator.set_mode(CreatorMode::FromVscode);
        creator.begin_parse("C:/work/team.code-workspace".to_owned());
        let mut parsed = suggestion("team", 1);
        parsed.folders.push(VscodeWorkspaceFolder {
            path: "C:/work/deep/folder".to_owned(),
            name: Some("Nice name".to_owned()),
            matched_repo_id: Some("r1".to_owned()),
        });
        creator.parsed(parsed);
        assert_eq!(creator.name(), "team", "the file's stem prefills the name");
        let rows = creator.folder_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "folder0", "the leaf when there is no name");
        assert_eq!(rows[0].badge(), "will register");
        assert_eq!(rows[1].label, "Nice name");
        assert_eq!(rows[1].badge(), "registered");
    }
}
