//! The jump palette's core: the entries it lists (panes, tabs, sessions in
//! no pane, and the sidebar's repo, workspace and folder rows), their order,
//! the ranking a typed query gives them, the highlight, and what Enter does.
//! Plain Rust: `RootView` gathers the inputs and performs the action.

use std::collections::{HashSet, VecDeque};

use crate::fuzzy::{self, Candidate};
use crate::sidebar::ContainerKind;
use crate::target::HostKey;

/// What an entry stands for; its tag is drawn beside it but never matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Pane,
    Tab,
    Session,
    Repo,
    /// A registered folder that is not a git repository.
    PlainRepo,
    Workspace,
    Dir,
    /// A host the palette opens a shell on.
    Shell,
}

impl EntryKind {
    /// The tag the palette draws before the entry's name.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Pane => "[PANE]",
            Self::Tab => "[TAB]",
            Self::Session => "[SESSION]",
            Self::Repo => "[REPO]",
            Self::PlainRepo | Self::Dir => "[DIR]",
            Self::Workspace => "[WS]",
            Self::Shell => "[SHELL]",
        }
    }

    /// The entry kind of a sidebar container of `kind`; `None` for the
    /// kinds the palette does not list.
    fn of_container(kind: ContainerKind, plain: bool) -> Option<Self> {
        match kind {
            ContainerKind::Repo if plain => Some(Self::PlainRepo),
            ContainerKind::Repo => Some(Self::Repo),
            ContainerKind::Workspace => Some(Self::Workspace),
            ContainerKind::Dir => Some(Self::Dir),
            ContainerKind::Shell
            | ContainerKind::Detached
            | ContainerKind::Tab
            | ContainerKind::Unbound => None,
        }
    }
}

/// What Enter on an entry does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JumpAction {
    /// Shows `tab_id` with exactly pane `pane_id` focused.
    FocusPane {
        host: HostKey,
        tab_id: String,
        pane_id: String,
    },
    /// Shows `tab_id` as a click on its pill does.
    ShowTab { tab_id: String },
    /// Opens `host`'s session `id` as a sidebar leaf click does.
    ShowSession { host: HostKey, id: String },
    /// Reveals and focuses the sidebar container row `key`; a repo or
    /// workspace becomes the preselected target.
    SelectContainer {
        host: HostKey,
        key: String,
        kind: ContainerKind,
        id: String,
    },
    /// Opens a plain shell on `host`.
    OpenShell { host: HostKey },
    /// Unbinds every pane of `host`'s sessions; the sessions keep running.
    UnbindHost { host: HostKey },
    /// Opens the rebind dialog over `host`'s active sessions.
    RebindHost { host: HostKey },
}

/// One palette row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    /// The name it is found by and drawn with.
    pub primary: String,
    /// The display name of the host it is on, matched below the name.
    pub secondary: String,
    pub action: JumpAction,
}

/// How many focus records [`FocusHistory`] keeps.
const HISTORY_CAP: usize = 64;

/// The tabs and panes that took focus, most recent first: one record per
/// tab and pane (`None` for a tab focused with no pane).
#[derive(Debug, Clone, Default)]
pub struct FocusHistory {
    records: VecDeque<(String, Option<String>)>,
}

impl FocusHistory {
    /// Records that `tab_id` (and `pane_id` in it) took focus: the record
    /// moves to the front, and the oldest go past [`HISTORY_CAP`].
    pub fn record(&mut self, tab_id: &str, pane_id: Option<&str>) {
        let record = (tab_id.to_owned(), pane_id.map(str::to_owned));
        self.records.retain(|kept| *kept != record);
        self.records.push_front(record);
        self.records.truncate(HISTORY_CAP);
    }

    /// Drops the records of every tab not in `live`.
    pub fn prune(&mut self, live: &HashSet<&str>) {
        self.records.retain(|(tab, _)| live.contains(tab.as_str()));
    }

    /// The records, most recent first.
    pub fn records(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.records
            .iter()
            .map(|(tab, pane)| (tab.as_str(), pane.as_deref()))
    }
}

/// A pane of a tab, in the tab's pane order.
#[derive(Debug, Clone)]
pub struct PaneInput {
    pub tab_id: String,
    pub pane_id: String,
    pub host: HostKey,
    /// The session it shows; `None` for an empty pane.
    pub session_id: Option<String>,
    /// Its session's label.
    pub label: String,
    pub host_name: String,
}

/// A tab, in tab-bar order.
#[derive(Debug, Clone)]
pub struct TabInput {
    pub id: String,
    /// The name its pill shows.
    pub label: String,
    pub host_name: String,
}

/// A session the sidebar lists, in sidebar order.
#[derive(Debug, Clone)]
pub struct SessionInput {
    pub host: HostKey,
    pub id: String,
    pub label: String,
    pub host_name: String,
    /// Whether it can show in a pane (headless sessions cannot).
    pub can_attach: bool,
}

/// A sidebar container, in sidebar order.
#[derive(Debug, Clone)]
pub struct ContainerInput {
    pub host: HostKey,
    pub key: String,
    pub id: String,
    pub kind: ContainerKind,
    /// A registered folder that is not a git repository.
    pub plain: bool,
    pub name: String,
    pub host_name: String,
}

/// A host the palette offers a shell on, in host order.
#[derive(Debug, Clone)]
pub struct HostInput {
    pub host: HostKey,
    /// The name its sidebar header draws.
    pub name: String,
    /// Whether its connection is open, the header's Online.
    pub online: bool,
}

/// Everything the palette lists, as plain data.
#[derive(Debug, Clone, Default)]
pub struct JumpInputs {
    pub panes: Vec<PaneInput>,
    pub tabs: Vec<TabInput>,
    pub sessions: Vec<SessionInput>,
    pub containers: Vec<ContainerInput>,
    pub hosts: Vec<HostInput>,
}

/// The palette's entries in their empty-query order: panes and tabs by last
/// focus (each record's pane, then its tab), then the never-focused tabs in
/// tab-bar order each followed by its panes, then the sessions no pane shows,
/// then the repo, workspace and folder containers, then one shell entry per
/// online host. Empty panes, headless sessions and records of closed tabs are
/// left out.
#[must_use]
pub fn build_entries(inputs: &JumpInputs, history: &FocusHistory) -> Vec<Entry> {
    let panes: Vec<&PaneInput> = inputs
        .panes
        .iter()
        .filter(|p| p.session_id.is_some())
        .collect();
    let mut entries = Vec::new();
    let mut seen_panes: HashSet<(&str, &str)> = HashSet::new();
    let mut seen_tabs: HashSet<&str> = HashSet::new();
    for (tab_id, pane_id) in history.records() {
        let Some(tab) = inputs.tabs.iter().find(|t| t.id == tab_id) else {
            continue;
        };
        let pane =
            pane_id.and_then(|id| panes.iter().find(|p| p.tab_id == tab_id && p.pane_id == id));
        if let Some(pane) = pane
            && seen_panes.insert((&pane.tab_id, &pane.pane_id))
        {
            entries.push(pane_entry(pane));
        }
        if seen_tabs.insert(&tab.id) {
            entries.push(tab_entry(tab));
        }
    }
    for tab in &inputs.tabs {
        if seen_tabs.insert(&tab.id) {
            entries.push(tab_entry(tab));
        }
        for pane in panes.iter().filter(|p| p.tab_id == tab.id) {
            if seen_panes.insert((&pane.tab_id, &pane.pane_id)) {
                entries.push(pane_entry(pane));
            }
        }
    }
    entries.extend(session_entries(&inputs.sessions, &panes));
    entries.extend(inputs.containers.iter().filter_map(container_entry));
    entries.extend(
        inputs
            .hosts
            .iter()
            .filter(|host| host.online)
            .flat_map(host_entries),
    );
    entries
}

/// The `[SHELL]` entries of an online host: a plain shell on it, then the
/// unbind and the rebind of its sessions.
fn host_entries(host: &HostInput) -> [Entry; 3] {
    let entry = |primary: String, action: JumpAction| Entry {
        kind: EntryKind::Shell,
        primary,
        secondary: host.name.clone(),
        action,
    };
    let key = || host.host.clone();
    [
        entry(
            format!("Open shell on {}", host.name),
            JumpAction::OpenShell { host: key() },
        ),
        entry(
            format!("Unbind {}'s sessions", host.name),
            JumpAction::UnbindHost { host: key() },
        ),
        entry(
            format!("Rebind {}'s sessions…", host.name),
            JumpAction::RebindHost { host: key() },
        ),
    ]
}

fn pane_entry(pane: &PaneInput) -> Entry {
    Entry {
        kind: EntryKind::Pane,
        primary: pane.label.clone(),
        secondary: pane.host_name.clone(),
        action: JumpAction::FocusPane {
            host: pane.host.clone(),
            tab_id: pane.tab_id.clone(),
            pane_id: pane.pane_id.clone(),
        },
    }
}

fn tab_entry(tab: &TabInput) -> Entry {
    Entry {
        kind: EntryKind::Tab,
        primary: tab.label.clone(),
        secondary: tab.host_name.clone(),
        action: JumpAction::ShowTab {
            tab_id: tab.id.clone(),
        },
    }
}

/// The `[SESSION]` entries: each session that can show in a pane and that
/// none of `panes` shows.
fn session_entries<'a>(
    sessions: &'a [SessionInput],
    panes: &'a [&PaneInput],
) -> impl Iterator<Item = Entry> + 'a {
    sessions
        .iter()
        .filter(|s| s.can_attach)
        .filter(|s| {
            !panes
                .iter()
                .any(|p| p.host == s.host && p.session_id.as_deref() == Some(s.id.as_str()))
        })
        .map(|s| Entry {
            kind: EntryKind::Session,
            primary: s.label.clone(),
            secondary: s.host_name.clone(),
            action: JumpAction::ShowSession {
                host: s.host.clone(),
                id: s.id.clone(),
            },
        })
}

fn container_entry(container: &ContainerInput) -> Option<Entry> {
    Some(Entry {
        kind: EntryKind::of_container(container.kind, container.plain)?,
        primary: container.name.clone(),
        secondary: container.host_name.clone(),
        action: JumpAction::SelectContainer {
            host: container.host.clone(),
            key: container.key.clone(),
            kind: container.kind,
            id: container.id.clone(),
        },
    })
}

/// The palette's state: its entries, the query, the entries the query
/// matches (best first) and the highlighted one among them.
#[derive(Debug, Clone)]
pub struct QuickJump {
    entries: Vec<Entry>,
    query: String,
    /// Indices into `entries`, best match first.
    filtered: Vec<usize>,
    /// Index into `filtered`.
    highlight: usize,
}

impl QuickJump {
    /// The palette over `entries`, with an empty query: every entry, the
    /// first highlighted.
    #[must_use]
    pub fn new(entries: Vec<Entry>) -> Self {
        let filtered = (0..entries.len()).collect();
        Self {
            entries,
            query: String::new(),
            filtered,
            highlight: 0,
        }
    }

    /// Narrows the list to the entries `query` matches, best first and in
    /// the empty-query order on ties, and highlights the best. A shell entry
    /// is listed only on a plain substring of its name, which the fuzzy
    /// matcher would otherwise supply for almost any query.
    pub fn set_query(&mut self, query: &str) {
        query.clone_into(&mut self.query);
        let candidates: Vec<Candidate<'_>> = self
            .entries
            .iter()
            .map(|entry| Candidate {
                primary: &entry.primary,
                secondary: Some(&entry.secondary),
            })
            .collect();
        let needle = query.trim().to_lowercase();
        let entries = &self.entries;
        self.filtered = fuzzy::ranked(query, &candidates)
            .into_iter()
            .map(|(_, index)| index)
            .filter(|&index| {
                entries.get(index).is_some_and(|entry| {
                    entry.kind != EntryKind::Shell
                        || needle.is_empty()
                        || entry.primary.to_lowercase().contains(&needle)
                })
            })
            .collect();
        self.highlight = 0;
    }

    /// The query as typed.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The entries the query matches, best first.
    pub fn filtered(&self) -> impl Iterator<Item = &Entry> {
        self.filtered
            .iter()
            .filter_map(|&index| self.entries.get(index))
    }

    /// The highlighted position in [`Self::filtered`]; `None` when nothing
    /// matches.
    #[must_use]
    pub fn highlight(&self) -> Option<usize> {
        (self.highlight < self.filtered.len()).then_some(self.highlight)
    }

    /// Moves the highlight up one row, staying on the first.
    pub fn up(&mut self) {
        self.highlight = self.highlight.saturating_sub(1);
    }

    /// Moves the highlight down one row, staying on the last.
    pub fn down(&mut self) {
        if self.highlight + 1 < self.filtered.len() {
            self.highlight += 1;
        }
    }

    /// What Enter does: the highlighted entry's action; `None` when the
    /// list is empty.
    #[must_use]
    pub fn enter(&self) -> Option<JumpAction> {
        let index = *self.filtered.get(self.highlight)?;
        self.entries.get(index).map(|entry| entry.action.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(name: &str) -> HostKey {
        HostKey::Remote(name.to_owned())
    }

    fn pane(tab: &str, pane: &str, session: Option<&str>, label: &str) -> PaneInput {
        PaneInput {
            tab_id: tab.to_owned(),
            pane_id: pane.to_owned(),
            host: HostKey::Local,
            session_id: session.map(str::to_owned),
            label: label.to_owned(),
            host_name: "This PC".to_owned(),
        }
    }

    fn tab(id: &str, label: &str) -> TabInput {
        TabInput {
            id: id.to_owned(),
            label: label.to_owned(),
            host_name: "This PC".to_owned(),
        }
    }

    fn session(host: HostKey, id: &str, label: &str, can_attach: bool) -> SessionInput {
        SessionInput {
            host,
            id: id.to_owned(),
            label: label.to_owned(),
            host_name: "This PC".to_owned(),
            can_attach,
        }
    }

    fn container(kind: ContainerKind, id: &str, name: &str, host_name: &str) -> ContainerInput {
        ContainerInput {
            host: HostKey::Local,
            key: format!("{}:{id}", kind.tag()),
            id: id.to_owned(),
            kind,
            plain: false,
            name: name.to_owned(),
            host_name: host_name.to_owned(),
        }
    }

    fn host(host: HostKey, name: &str, online: bool) -> HostInput {
        HostInput {
            host,
            name: name.to_owned(),
            online,
        }
    }

    fn names(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| format!("{} {}", e.kind.tag(), e.primary))
            .collect()
    }

    fn filtered_names(jump: &QuickJump) -> Vec<String> {
        jump.filtered()
            .map(|e| format!("{} {}", e.kind.tag(), e.primary))
            .collect()
    }

    /// Two tabs of two and one panes, a session in no pane, a repo.
    fn two_tabs() -> JumpInputs {
        JumpInputs {
            panes: vec![
                pane("t1", "p1", Some("s1"), "alpha"),
                pane("t1", "p2", Some("s2"), "beta"),
                pane("t2", "p3", Some("s3"), "gamma"),
            ],
            tabs: vec![tab("t1", "One"), tab("t2", "Two")],
            sessions: vec![
                session(HostKey::Local, "s1", "alpha", true),
                session(HostKey::Local, "s4", "delta", true),
            ],
            containers: vec![container(ContainerKind::Repo, "r1", "yaat", "This PC")],
            hosts: Vec::new(),
        }
    }

    #[test]
    fn a_plain_folder_repo_reads_dir() {
        let plain = ContainerInput {
            plain: true,
            ..container(ContainerKind::Repo, "r3", "notes", "This PC")
        };
        let entries = build_entries(
            &JumpInputs {
                containers: vec![plain],
                ..JumpInputs::default()
            },
            &FocusHistory::default(),
        );
        assert_eq!(names(&entries), ["[DIR] notes"]);
    }

    #[test]
    fn empty_query_lists_panes_and_tabs_by_last_focus_then_sessions_then_containers() {
        let mut history = FocusHistory::default();
        history.record("t1", Some("p2"));
        history.record("t2", Some("p3"));
        let entries = build_entries(&two_tabs(), &history);
        assert_eq!(
            names(&entries),
            [
                "[PANE] gamma",
                "[TAB] Two",
                "[PANE] beta",
                "[TAB] One",
                "[PANE] alpha",
                "[SESSION] delta",
                "[REPO] yaat",
            ]
        );
    }

    #[test]
    fn never_focused_tabs_follow_in_tab_bar_order_with_their_panes() {
        let mut history = FocusHistory::default();
        history.record("t2", None);
        let entries = build_entries(&two_tabs(), &history);
        assert_eq!(
            names(&entries)[..5],
            [
                "[TAB] Two",
                "[TAB] One",
                "[PANE] alpha",
                "[PANE] beta",
                "[PANE] gamma",
            ]
        );
    }

    #[test]
    fn empty_panes_and_headless_sessions_are_not_listed() {
        let inputs = JumpInputs {
            panes: vec![
                pane("t1", "p1", None, "empty"),
                pane("t1", "p2", Some("s2"), "beta"),
            ],
            tabs: vec![tab("t1", "One")],
            sessions: vec![
                session(HostKey::Local, "s2", "beta", true),
                session(HostKey::Local, "s5", "headless", false),
                session(remote("box"), "s2", "beta elsewhere", true),
            ],
            containers: Vec::new(),
            hosts: Vec::new(),
        };
        let entries = build_entries(&inputs, &FocusHistory::default());
        assert_eq!(
            names(&entries),
            ["[TAB] One", "[PANE] beta", "[SESSION] beta elsewhere"],
            "another host's session of the same id is in no pane"
        );
    }

    #[test]
    fn shell_detached_tab_and_unbound_containers_are_not_listed() {
        let inputs = JumpInputs {
            containers: vec![
                container(ContainerKind::Workspace, "w1", "ws", "This PC"),
                container(ContainerKind::Shell, "sh", "shell", "This PC"),
                container(ContainerKind::Repo, "r1", "repo", "This PC"),
                container(ContainerKind::Detached, "det", "detached", "This PC"),
                container(ContainerKind::Tab, "t1", "tab", "This PC"),
                container(ContainerKind::Unbound, "unb", "unbound", "This PC"),
                container(ContainerKind::Dir, "d1", "dir", "This PC"),
            ],
            ..JumpInputs::default()
        };
        let entries = build_entries(&inputs, &FocusHistory::default());
        assert_eq!(names(&entries), ["[WS] ws", "[REPO] repo", "[DIR] dir"]);
    }

    #[test]
    fn an_online_host_is_offered_as_a_shell() {
        let inputs = JumpInputs {
            containers: vec![container(ContainerKind::Repo, "r1", "yaat", "This PC")],
            hosts: vec![
                host(HostKey::Local, "This PC", true),
                host(remote("box"), "box", true),
                host(remote("lab"), "lab", false),
            ],
            ..JumpInputs::default()
        };
        let entries = build_entries(&inputs, &FocusHistory::default());
        assert_eq!(
            names(&entries),
            [
                "[REPO] yaat",
                "[SHELL] Open shell on This PC",
                "[SHELL] Unbind This PC's sessions",
                "[SHELL] Rebind This PC's sessions…",
                "[SHELL] Open shell on box",
                "[SHELL] Unbind box's sessions",
                "[SHELL] Rebind box's sessions…",
            ],
            "the shell entries rank after the others, and an offline host is offered none"
        );
        assert_eq!(
            entries[4].action,
            JumpAction::OpenShell {
                host: remote("box")
            }
        );
    }

    /// This PC and one online remote, each with a shell entry.
    fn two_hosts() -> JumpInputs {
        JumpInputs {
            hosts: vec![
                host(HostKey::Local, "This PC", true),
                host(remote("box"), "box", true),
            ],
            ..JumpInputs::default()
        }
    }

    #[test]
    fn a_shell_entry_ignores_a_scattered_match() {
        let mut jump = QuickJump::new(build_entries(&two_hosts(), &FocusHistory::default()));
        jump.set_query("not");
        assert_eq!(
            filtered_names(&jump),
            Vec::<String>::new(),
            "a scattered match does not offer a shell"
        );
        jump.set_query("open shell");
        assert_eq!(
            filtered_names(&jump),
            ["[SHELL] Open shell on This PC", "[SHELL] Open shell on box"],
            "a plain substring still reads"
        );
    }

    #[test]
    fn a_shell_entry_matches_its_host_name() {
        let mut jump = QuickJump::new(build_entries(&two_hosts(), &FocusHistory::default()));
        jump.set_query("box");
        assert_eq!(
            filtered_names(&jump),
            [
                "[SHELL] Open shell on box",
                "[SHELL] Unbind box's sessions",
                "[SHELL] Rebind box's sessions…",
            ]
        );
        jump.set_query("SHELL");
        assert_eq!(
            filtered_names(&jump),
            ["[SHELL] Open shell on This PC", "[SHELL] Open shell on box"],
            "the match ignores case"
        );
    }

    #[test]
    fn kind_tags_are_not_matched_as_names() {
        let inputs = JumpInputs {
            containers: vec![container(ContainerKind::Repo, "r1", "yaat", "This PC")],
            ..JumpInputs::default()
        };
        let mut jump = QuickJump::new(build_entries(&inputs, &FocusHistory::default()));
        jump.set_query("repo");
        assert_eq!(filtered_names(&jump), Vec::<String>::new());
        jump.set_query("ya");
        assert_eq!(filtered_names(&jump), ["[REPO] yaat"]);
    }

    #[test]
    fn a_host_name_matches_as_the_secondary_field() {
        let inputs = JumpInputs {
            containers: vec![
                container(ContainerKind::Repo, "r1", "yaat", "buildbox"),
                container(ContainerKind::Repo, "r2", "build-tools", "This PC"),
                container(ContainerKind::Repo, "r3", "other", "This PC"),
            ],
            ..JumpInputs::default()
        };
        let mut jump = QuickJump::new(build_entries(&inputs, &FocusHistory::default()));
        jump.set_query("build");
        assert_eq!(filtered_names(&jump), ["[REPO] build-tools", "[REPO] yaat"]);
    }

    #[test]
    fn typing_ranks_by_match_class_and_keeps_order_on_ties() {
        let inputs = JumpInputs {
            containers: vec![
                container(ContainerKind::Repo, "r1", "my-api", "This PC"),
                container(ContainerKind::Repo, "r2", "rapid", "This PC"),
                container(ContainerKind::Repo, "r3", "api", "This PC"),
                container(ContainerKind::Repo, "r4", "apix", "This PC"),
                container(ContainerKind::Repo, "r5", "a-p-i", "This PC"),
            ],
            ..JumpInputs::default()
        };
        let mut jump = QuickJump::new(build_entries(&inputs, &FocusHistory::default()));
        jump.set_query("api");
        assert_eq!(
            filtered_names(&jump),
            [
                "[REPO] api",
                "[REPO] apix",
                "[REPO] my-api",
                "[REPO] rapid",
                "[REPO] a-p-i",
            ]
        );
    }

    #[test]
    fn the_highlight_stops_at_the_ends_and_resets_on_a_new_query() {
        let mut jump = QuickJump::new(build_entries(&two_tabs(), &FocusHistory::default()));
        assert_eq!(jump.highlight(), Some(0));
        jump.up();
        assert_eq!(jump.highlight(), Some(0), "no wrap at the top");
        let last = jump.filtered().count() - 1;
        for _ in 0..10 {
            jump.down();
        }
        assert_eq!(jump.highlight(), Some(last), "no wrap at the bottom");
        jump.set_query("a");
        assert_eq!(jump.highlight(), Some(0));
        assert_eq!(jump.query(), "a");
    }

    #[test]
    fn enter_on_an_empty_list_does_nothing() {
        let mut jump = QuickJump::new(build_entries(&two_tabs(), &FocusHistory::default()));
        jump.set_query("zzzz");
        assert_eq!(jump.highlight(), None);
        jump.down();
        assert_eq!(jump.enter(), None);
        assert_eq!(QuickJump::new(Vec::new()).enter(), None);
    }

    #[test]
    fn enter_returns_the_exact_pane() {
        let inputs = JumpInputs {
            panes: vec![
                pane("t1", "p1", Some("s1"), "alpha"),
                pane("t1", "p2", Some("s1"), "alpha"),
            ],
            tabs: vec![tab("t1", "One")],
            ..JumpInputs::default()
        };
        let mut history = FocusHistory::default();
        history.record("t1", Some("p2"));
        let mut jump = QuickJump::new(build_entries(&inputs, &history));
        jump.set_query("alpha");
        assert_eq!(
            jump.enter(),
            Some(JumpAction::FocusPane {
                host: HostKey::Local,
                tab_id: "t1".to_owned(),
                pane_id: "p2".to_owned(),
            })
        );
        jump.down();
        assert_eq!(
            jump.enter(),
            Some(JumpAction::FocusPane {
                host: HostKey::Local,
                tab_id: "t1".to_owned(),
                pane_id: "p1".to_owned(),
            })
        );
    }

    #[test]
    fn focus_history_moves_a_refocused_record_to_the_front_and_caps_at_64() {
        let mut history = FocusHistory::default();
        history.record("t1", Some("p1"));
        history.record("t2", None);
        history.record("t1", Some("p1"));
        assert_eq!(
            history.records().collect::<Vec<_>>(),
            [("t1", Some("p1")), ("t2", None)]
        );
        for n in 0..100 {
            history.record(&format!("t{n}"), Some("p"));
        }
        assert_eq!(history.records().count(), HISTORY_CAP);
        assert_eq!(history.records().next(), Some(("t99", Some("p"))));
    }

    #[test]
    fn records_of_closed_tabs_are_dropped() {
        let mut history = FocusHistory::default();
        history.record("t1", Some("p1"));
        history.record("gone", Some("px"));
        let entries = build_entries(&two_tabs(), &history);
        assert_eq!(names(&entries)[..2], ["[PANE] alpha", "[TAB] One"]);
        history.prune(&HashSet::from(["t1", "t2"]));
        assert_eq!(history.records().collect::<Vec<_>>(), [("t1", Some("p1"))]);
    }
}
