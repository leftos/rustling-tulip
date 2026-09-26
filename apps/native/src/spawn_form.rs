//! The spawn dialog's form: target, runtime, Open in, trusted launch,
//! worktree new or existing, branch and base branch, the "Share this
//! worktree?" confirm and the keyboard focus ring. `spawn_view` renders it
//! and forwards input; the daemon requests it needs come back as messages.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use protocol::{
    Agent, AgentOptions, ClientMessage, DaemonMessage, PinnedMemberWorktree, RepoEntry,
    RootWorktreeEntry, RootWorktreeStatus, SessionMode, SessionSnapshot, SpawnConfig, SpawnRequest,
    SpawnTarget, SuggestTarget, TabEntry, WorkspaceEntry, WorktreeInfo, WorktreeLaunchTarget,
    WorktreeReusePolicy,
};

use crate::spawns::OpenIn;

/// How long a branch-name suggestion is waited for before the field stops
/// showing that one is on the way.
pub(crate) const SUGGESTION_TIMEOUT: Duration = Duration::from_secs(10);

/// A repo or a workspace to spawn into.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Target {
    Repo(String),
    Workspace(String),
}

impl Target {
    fn suggest(&self) -> SuggestTarget {
        match self {
            Self::Repo(id) => SuggestTarget::Repo {
                repo_id: id.clone(),
            },
            Self::Workspace(id) => SuggestTarget::Workspace {
                workspace_id: id.clone(),
            },
        }
    }

    /// The target a suggestion reply names, if it is one.
    pub(crate) fn from_suggest(target: &SuggestTarget) -> Option<Self> {
        match target {
            SuggestTarget::Repo { repo_id } => Some(Self::Repo(repo_id.clone())),
            SuggestTarget::Workspace { workspace_id } => {
                Some(Self::Workspace(workspace_id.clone()))
            }
            SuggestTarget::Unknown => None,
        }
    }

    fn selector_part(&self) -> String {
        match self {
            Self::Repo(id) => format!("repo-{id}"),
            Self::Workspace(id) => format!("workspace-{id}"),
        }
    }
}

/// Branch names the daemon suggested, per target, for the life of the
/// process; a manual edit, Random and a submit drop the target's entry.
#[derive(Debug, Default)]
pub(crate) struct BranchCache(HashMap<Target, String>);

impl BranchCache {
    fn get(&self, target: &Target) -> Option<&str> {
        self.0.get(target).map(String::as_str)
    }

    pub(crate) fn insert(&mut self, target: Target, name: String) {
        self.0.insert(target, name);
    }

    fn remove(&mut self, target: &Target) {
        self.0.remove(target);
    }
}

/// What runs in the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Runtime {
    Agent(Agent),
    PlainShell,
}

impl Runtime {
    pub(crate) const ALL: [Self; 4] = [
        Self::Agent(Agent::Claude),
        Self::Agent(Agent::Codex),
        Self::Agent(Agent::Cursor),
        Self::PlainShell,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Agent(agent) => agent.as_label(),
            Self::PlainShell => "Plain shell",
        }
    }

    fn selector_part(self) -> &'static str {
        match self {
            Self::Agent(agent) => agent.as_label(),
            Self::PlainShell => "plain-shell",
        }
    }
}

/// Where the session opens, as the dialog offers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenChoice {
    CurrentTab,
    NewTab,
    Tab(String),
}

/// A tab the dialog can name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabChoice {
    pub id: String,
    pub name: String,
}

/// The tabs Open in offers: the active tab when it can hold panes, and
/// every other tab that can.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TabChoices {
    pub current: Option<TabChoice>,
    pub others: Vec<TabChoice>,
}

impl TabChoices {
    pub(crate) fn from_tabs(tabs: &[TabEntry], active: Option<&str>) -> Self {
        let choice = |tab: &TabEntry| TabChoice {
            id: tab.id.clone(),
            name: tab.name.clone(),
        };
        let grids = tabs.iter().filter(|tab| tab.grid().is_some());
        let (current, others): (Vec<&TabEntry>, Vec<&TabEntry>) =
            grids.partition(|tab| Some(tab.id.as_str()) == active);
        Self {
            current: current.first().map(|tab| choice(tab)),
            others: others.into_iter().map(choice).collect(),
        }
    }
}

/// New worktree, or one already on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorktreeMode {
    New,
    Existing,
}

/// A focusable control of the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Control {
    Close,
    Target(Target),
    Runtime(Runtime),
    OpenIn(OpenChoice),
    Trusted,
    UseWorktree,
    Mode(WorktreeMode),
    Existing(String),
    Branch,
    Random,
    Base,
    Cancel,
    Submit,
}

impl Control {
    /// The debug selector the control's element carries.
    pub(crate) fn selector(&self) -> String {
        match self {
            Self::Close => "spawn-close".to_owned(),
            Self::Target(target) => format!("spawn-target-{}", target.selector_part()),
            Self::Runtime(runtime) => format!("spawn-agent-{}", runtime.selector_part()),
            Self::OpenIn(OpenChoice::CurrentTab) => "spawn-placement-current-tab".to_owned(),
            Self::OpenIn(OpenChoice::NewTab) => "spawn-placement-new-tab".to_owned(),
            Self::OpenIn(OpenChoice::Tab(id)) => format!("spawn-placement-tab-{id}"),
            Self::Trusted => "spawn-skip-perms".to_owned(),
            Self::UseWorktree => "spawn-worktree".to_owned(),
            Self::Mode(WorktreeMode::New) => "spawn-worktree-mode-new".to_owned(),
            Self::Mode(WorktreeMode::Existing) => "spawn-worktree-mode-existing".to_owned(),
            Self::Existing(key) => format!("spawn-existing-{key}"),
            Self::Branch => "spawn-branch".to_owned(),
            Self::Random => "spawn-branch-random".to_owned(),
            Self::Base => "spawn-base-branch".to_owned(),
            Self::Cancel => "spawn-cancel".to_owned(),
            Self::Submit => "spawn-submit".to_owned(),
        }
    }
}

/// The two buttons of the "Share this worktree?" confirm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShareButton {
    Cancel,
    Launch,
}

impl ShareButton {
    pub(crate) fn selector(self) -> &'static str {
        match self {
            Self::Cancel => "spawn-share-worktree-cancel",
            Self::Launch => "spawn-share-worktree-ok",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Cancel => "Cancel",
            Self::Launch => "Launch anyway",
        }
    }
}

/// What a press or a key did to the dialog.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The dialog stays open; these go to the daemon.
    Stay(Vec<ClientMessage>),
    Close,
    /// The picked worktree is in use: the confirm opened.
    ConfirmShare,
    /// Spawn this and close.
    Spawn(Box<Submission>),
}

/// A submitted spawn: the request, where it opens, and the worktree
/// default to save when the checkbox changed.
#[derive(Debug)]
pub(crate) struct Submission {
    pub request: SpawnRequest,
    pub open_in: OpenIn,
    pub default_change: Option<ClientMessage>,
}

/// How an existing worktree or group pins the spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pin {
    Worktree(String),
    Group(Vec<PinnedMemberWorktree>),
}

/// An existing worktree (single repo) or worktree group (workspace).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExistingOption {
    pub key: String,
    name: String,
    status: RootWorktreeStatus,
    /// "stale" means something only for a worktree this app manages.
    stale_shown: bool,
    size_bytes: Option<u64>,
    modified_unix: Option<i64>,
    /// The branch the session is labelled with.
    branch_name: String,
    pin: Pin,
}

impl ExistingOption {
    fn from_worktree(info: &WorktreeInfo) -> Self {
        let named = !info.branch.is_empty();
        Self {
            key: info.path.clone(),
            name: if named {
                info.branch.clone()
            } else {
                "(detached HEAD)".to_owned()
            },
            status: info.status,
            stale_shown: info.group_path.is_some(),
            size_bytes: info.size_bytes,
            modified_unix: info.last_modified_unix,
            branch_name: if named {
                info.branch.clone()
            } else {
                branch_from_worktree_path(info.group_path.as_deref().unwrap_or(&info.path))
            },
            pin: Pin::Worktree(info.path.clone()),
        }
    }

    fn from_group(entry: &RootWorktreeEntry, workspace_id: &str) -> Option<Self> {
        let Some(WorktreeLaunchTarget::Workspace {
            workspace_id: id,
            branch,
            members,
        }) = &entry.launch
        else {
            return None;
        };
        if id != workspace_id {
            return None;
        }
        let name = branch.clone().unwrap_or_else(|| entry.branch_slug.clone());
        Some(Self {
            key: members
                .iter()
                .map(|m| m.path.as_str())
                .collect::<Vec<_>>()
                .join("|"),
            name: name.clone(),
            status: entry.status,
            stale_shown: true,
            size_bytes: entry.size_bytes,
            modified_unix: entry.last_modified_unix,
            branch_name: name,
            pin: Pin::Group(members.clone()),
        })
    }

    /// The name, then whether it is in use, its size and its age.
    pub(crate) fn label(&self, now_unix: i64) -> String {
        let mut meta: Vec<String> = Vec::new();
        match self.status {
            RootWorktreeStatus::Active => meta.push("in use".to_owned()),
            RootWorktreeStatus::Detached => meta.push("stopped session".to_owned()),
            RootWorktreeStatus::Stale if self.stale_shown => meta.push("stale".to_owned()),
            RootWorktreeStatus::Stale | RootWorktreeStatus::Unknown => {}
        }
        meta.extend(self.size_bytes.map(human_size));
        meta.extend(self.modified_unix.map(|t| human_relative_time(now_unix, t)));
        if meta.is_empty() {
            self.name.clone()
        } else {
            format!("{} — {}", self.name, meta.join(", "))
        }
    }

    fn is_active(&self) -> bool {
        self.status == RootWorktreeStatus::Active
    }
}

/// The runtime and whether the user has chosen it.
#[derive(Debug, Clone, Copy)]
struct RuntimeChoice {
    agent: Agent,
    plain_shell: bool,
    touched: Touched,
}

/// Which runtime defaults the user has overridden.
#[derive(Debug, Clone, Copy, Default)]
struct Touched {
    agent: bool,
    run_mode: bool,
}

/// The branch name field.
#[derive(Debug, Default)]
struct BranchField {
    value: String,
    /// The value still follows suggestions and the worktree toggle.
    auto: bool,
    /// A suggestion is on the way until then.
    pending_until: Option<Instant>,
}

/// The base branch field.
#[derive(Debug, Default)]
struct BaseField {
    value: String,
    touched: bool,
}

/// The branches of the repo that drives the defaults.
#[derive(Debug, Default)]
struct Refs {
    known: Vec<String>,
    remote: Vec<String>,
    current: Option<String>,
    fetch_failed: bool,
}

/// The existing worktrees on offer and the one picked.
#[derive(Debug, Default)]
struct Existing {
    options: Vec<ExistingOption>,
    loaded: bool,
    selected: Option<String>,
}

/// The spawn dialog's state.
#[derive(Debug)]
pub(crate) struct SpawnForm {
    repos: Vec<RepoEntry>,
    workspaces: Vec<WorkspaceEntry>,
    target: Target,
    runtime: RuntimeChoice,
    tabs: TabChoices,
    open_in: OpenChoice,
    trusted: bool,
    use_worktree: bool,
    mode: WorktreeMode,
    branch: BranchField,
    base: BaseField,
    refs: Refs,
    existing: Existing,
    focus: Control,
    share: Option<ShareButton>,
    submitted: bool,
}

/// What the dialog opens over.
pub(crate) struct FormInputs<'a> {
    pub repos: &'a [RepoEntry],
    pub workspaces: &'a [WorkspaceEntry],
    /// The session in the focused pane.
    pub focused: Option<&'a SessionSnapshot>,
    pub tabs: TabChoices,
}

impl SpawnForm {
    /// The form over `inputs` and the requests its target needs, or `None`
    /// when there is nothing to spawn into.
    pub(crate) fn open(
        inputs: FormInputs<'_>,
        cache: &mut BranchCache,
        now: Instant,
    ) -> Option<(Self, Vec<ClientMessage>)> {
        let target = initial_target(inputs.focused, inputs.repos, inputs.workspaces)?;
        let open_in = if inputs.tabs.current.is_some() {
            OpenChoice::CurrentTab
        } else {
            OpenChoice::NewTab
        };
        let mut form = Self {
            repos: inputs.repos.to_vec(),
            workspaces: inputs.workspaces.to_vec(),
            target: target.clone(),
            runtime: RuntimeChoice {
                agent: Agent::Claude,
                plain_shell: false,
                touched: Touched::default(),
            },
            tabs: inputs.tabs,
            open_in,
            trusted: false,
            use_worktree: true,
            mode: WorktreeMode::New,
            branch: BranchField::default(),
            base: BaseField::default(),
            refs: Refs::default(),
            existing: Existing::default(),
            focus: Control::Branch,
            share: None,
            submitted: false,
        };
        let messages = form.load_target(target, cache, now);
        form.focus = if form.controls().contains(&Control::Branch) {
            Control::Branch
        } else {
            Control::Target(form.target.clone())
        };
        Some((form, messages))
    }

    /// Picks `target` and resets everything that belongs to the one before.
    fn load_target(
        &mut self,
        target: Target,
        cache: &mut BranchCache,
        now: Instant,
    ) -> Vec<ClientMessage> {
        self.target = target;
        self.apply_runtime_defaults();
        self.use_worktree = self.entry_default_use_worktree();
        self.mode = WorktreeMode::New;
        self.existing = Existing::default();
        self.refs = Refs::default();
        self.base = BaseField {
            value: self.default_base(),
            touched: false,
        };
        self.branch = BranchField {
            auto: true,
            ..BranchField::default()
        };
        let mut messages = Vec::new();
        if let Some(repo_id) = self.branches_repo_id() {
            messages.push(ClientMessage::ListBranches { repo_id });
        }
        if self.use_worktree {
            messages.extend(self.seed_suggestion(cache, now));
        } else {
            self.branch.value = self.in_place_default();
        }
        messages.extend(
            self.fetch_repo_ids()
                .into_iter()
                .map(|repo_id| ClientMessage::FetchRepo { repo_id }),
        );
        messages
    }

    /// The cached suggestion, else an empty field and a request for one.
    fn seed_suggestion(&mut self, cache: &BranchCache, now: Instant) -> Option<ClientMessage> {
        if let Some(name) = cache.get(&self.target) {
            name.clone_into(&mut self.branch.value);
            None
        } else {
            self.branch.value.clear();
            self.request_suggestion(false, now)
        }
    }

    fn request_suggestion(&mut self, force: bool, now: Instant) -> Option<ClientMessage> {
        if !force && self.suggestion_pending(now) {
            return None;
        }
        self.branch.pending_until = Some(now + SUGGESTION_TIMEOUT);
        Some(ClientMessage::SuggestBranchName {
            target: self.target.suggest(),
        })
    }

    /// Whether a suggestion is still on the way at `now`.
    pub(crate) fn suggestion_pending(&self, now: Instant) -> bool {
        self.branch.pending_until.is_some_and(|until| now < until)
    }

    /// When the wait for a suggestion runs out.
    pub(crate) fn suggestion_deadline(&self) -> Option<Instant> {
        self.branch.pending_until
    }

    /// The target's defaults for the runtime, unless the user chose.
    fn apply_runtime_defaults(&mut self) {
        let (config, fallback) = match &self.target {
            Target::Repo(id) => {
                let repo = self.repo(id);
                (
                    repo.and_then(|r| r.last_spawn_config.as_ref()),
                    repo.and_then(|r| r.last_agent),
                )
            }
            Target::Workspace(id) => {
                let workspace = self.workspace(id);
                (
                    workspace.and_then(|w| w.last_spawn_config.as_ref()),
                    workspace
                        .and_then(|w| self.first_member(w))
                        .and_then(|r| r.last_agent),
                )
            }
        };
        let agent = config
            .map(|c| c.agent_options.agent())
            .or(fallback)
            .unwrap_or(Agent::Claude);
        let plain_shell = is_plain_shell(config);
        let touched = self.runtime.touched;
        if !touched.agent {
            self.runtime.agent = agent;
        }
        if !touched.run_mode {
            self.runtime.plain_shell = plain_shell;
        }
    }

    fn repo(&self, id: &str) -> Option<&RepoEntry> {
        self.repos.iter().find(|r| r.id == id)
    }

    fn workspace(&self, id: &str) -> Option<&WorkspaceEntry> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    /// A workspace's first registered member.
    fn first_member(&self, workspace: &WorkspaceEntry) -> Option<&RepoEntry> {
        workspace
            .member_repo_ids
            .iter()
            .find_map(|id| self.repo(id))
    }

    /// The repo whose branches seed the fields: the repo, or a workspace's
    /// first member.
    fn defaults_repo(&self) -> Option<&RepoEntry> {
        match &self.target {
            Target::Repo(id) => self.repo(id),
            Target::Workspace(id) => self.workspace(id).and_then(|w| self.first_member(w)),
        }
    }

    fn branches_repo_id(&self) -> Option<String> {
        self.defaults_repo().map(|r| r.id.clone())
    }

    /// The repos fetched on open: the repo, or every workspace member.
    fn fetch_repo_ids(&self) -> Vec<String> {
        match &self.target {
            Target::Repo(id) => vec![id.clone()],
            Target::Workspace(id) => self
                .workspace(id)
                .map(|w| w.member_repo_ids.clone())
                .unwrap_or_default(),
        }
    }

    fn entry_default_use_worktree(&self) -> bool {
        match &self.target {
            Target::Repo(id) => self.repo(id).is_none_or(|r| r.default_use_worktree),
            Target::Workspace(id) => self.workspace(id).is_none_or(|w| w.default_use_worktree),
        }
    }

    /// The branch an in-place spawn runs on: the one checked out, else the
    /// repo's default. A workspace uses its first member's default.
    pub(crate) fn in_place_default(&self) -> String {
        let repo_default = self.defaults_repo().and_then(|r| r.default_branch.clone());
        let found = match self.target {
            Target::Repo(_) => self
                .refs
                .current
                .clone()
                .or(repo_default)
                .or_else(|| self.refs.known.first().cloned()),
            Target::Workspace(_) => repo_default,
        };
        found.unwrap_or_else(|| "main".to_owned())
    }

    /// The local branch a new worktree would fork from.
    fn local_default(&self) -> String {
        let repo_default = self.defaults_repo().and_then(|r| r.default_branch.clone());
        let found = match self.target {
            Target::Repo(_) => repo_default
                .or_else(|| self.refs.current.clone())
                .or_else(|| self.refs.known.first().cloned()),
            Target::Workspace(_) => repo_default,
        };
        found.unwrap_or_else(|| "main".to_owned())
    }

    /// The base branch the field starts on: the remote counterpart of the
    /// local default when there is one.
    pub(crate) fn default_base(&self) -> String {
        prefer_remote_base(&self.local_default(), &self.refs.remote)
    }

    /// Folds in a daemon reply; returns what to send in answer, or `None`
    /// when the message is not one the form follows and nothing changed.
    pub(crate) fn on_message(&mut self, msg: &DaemonMessage) -> Option<Vec<ClientMessage>> {
        match msg {
            DaemonMessage::Branches {
                repo_id,
                branches,
                current,
                remote_branches,
            } if Some(repo_id) == self.branches_repo_id().as_ref() => {
                self.refs.known.clone_from(branches);
                self.refs.remote.clone_from(remote_branches);
                self.refs.current.clone_from(current);
                self.reseed_from_refs();
            }
            DaemonMessage::RepoFetched { repo_id, error }
                if Some(repo_id) == self.branches_repo_id().as_ref() =>
            {
                self.refs.fetch_failed = error.is_some();
                if error.is_none() {
                    return Some(vec![ClientMessage::ListBranches {
                        repo_id: repo_id.clone(),
                    }]);
                }
            }
            DaemonMessage::Worktrees { repo_id, worktrees }
                if self.listing_existing() && self.target == Target::Repo(repo_id.clone()) =>
            {
                let options = worktrees
                    .iter()
                    .map(ExistingOption::from_worktree)
                    .collect();
                self.set_existing(options);
            }
            DaemonMessage::WorktreesRootSnapshot { entries, .. }
                if self.listing_existing() && self.is_workspace() =>
            {
                let id = match &self.target {
                    Target::Workspace(id) => id.clone(),
                    Target::Repo(_) => return None,
                };
                let options = entries
                    .iter()
                    .filter_map(|entry| ExistingOption::from_group(entry, &id))
                    .collect();
                self.set_existing(options);
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    /// New refs move the base (until edited) and an untouched in-place
    /// branch.
    fn reseed_from_refs(&mut self) {
        if !self.base.touched {
            self.base.value = self.default_base();
        }
        if self.branch.auto && !self.use_worktree {
            self.branch.value = self.in_place_default();
        }
    }

    fn listing_existing(&self) -> bool {
        self.use_worktree && self.mode == WorktreeMode::Existing
    }

    fn set_existing(&mut self, options: Vec<ExistingOption>) {
        let selected = self.existing.selected.take();
        self.existing.selected = selected.filter(|key| options.iter().any(|o| &o.key == key));
        self.existing.options = options;
        self.existing.loaded = true;
    }

    /// A suggestion for `target` arrived: it fills the field while the
    /// field still follows suggestions.
    pub(crate) fn on_suggestion(&mut self, target: &Target, name: &str) {
        if *target != self.target {
            return;
        }
        self.branch.pending_until = None;
        if self.branch.auto && self.use_worktree {
            name.clone_into(&mut self.branch.value);
        }
    }

    /// The user typed in the branch field.
    pub(crate) fn edit_branch(&mut self, text: &str, cache: &mut BranchCache) {
        if text == self.branch.value {
            return;
        }
        self.branch.auto = false;
        cache.remove(&self.target);
        text.clone_into(&mut self.branch.value);
    }

    /// The user typed in the base branch field.
    pub(crate) fn edit_base(&mut self, text: &str) {
        if text == self.base.value {
            return;
        }
        self.base.touched = true;
        text.clone_into(&mut self.base.value);
    }

    /// Replaces the registry. A target that went away snaps to the first
    /// target; `None` means none is left and the dialog must close.
    pub(crate) fn set_registry(
        &mut self,
        repos: &[RepoEntry],
        workspaces: &[WorkspaceEntry],
        cache: &mut BranchCache,
        now: Instant,
    ) -> Option<Vec<ClientMessage>> {
        repos.clone_into(&mut self.repos);
        workspaces.clone_into(&mut self.workspaces);
        let targets = self.targets();
        if targets.contains(&self.target) {
            return Some(Vec::new());
        }
        let first = targets.into_iter().next()?;
        Some(self.load_target(first, cache, now))
    }

    /// Replaces the tabs; a choice whose tab went away falls back to the
    /// current tab, else a new one.
    pub(crate) fn set_tabs(&mut self, tabs: TabChoices) {
        self.tabs = tabs;
        let still_there = match &self.open_in {
            OpenChoice::CurrentTab => self.tabs.current.is_some(),
            OpenChoice::NewTab => true,
            OpenChoice::Tab(id) => self.tabs.others.iter().any(|t| &t.id == id),
        };
        if !still_there {
            self.open_in = if self.tabs.current.is_some() {
                OpenChoice::CurrentTab
            } else {
                OpenChoice::NewTab
            };
        }
    }

    /// Every target, repos then workspaces, as the picker lists them.
    pub(crate) fn targets(&self) -> Vec<Target> {
        let repos = self.repos.iter().map(|r| Target::Repo(r.id.clone()));
        let workspaces = self
            .workspaces
            .iter()
            .map(|w| Target::Workspace(w.id.clone()));
        repos.chain(workspaces).collect()
    }

    pub(crate) fn target(&self) -> &Target {
        &self.target
    }

    /// A target's picker label.
    pub(crate) fn target_label(&self, target: &Target) -> String {
        match target {
            Target::Repo(id) => {
                let name = self.repo(id).map_or(id.as_str(), |r| r.name.as_str());
                format!("[REPO]  {name}")
            }
            Target::Workspace(id) => match self.workspace(id) {
                Some(w) => format!("[WS]    {} ({} repos)", w.name, w.member_repo_ids.len()),
                None => format!("[WS]    {id}"),
            },
        }
    }

    pub(crate) fn runtime(&self) -> Runtime {
        if self.runtime.plain_shell {
            Runtime::PlainShell
        } else {
            Runtime::Agent(self.runtime.agent)
        }
    }

    fn select_runtime(&mut self, runtime: Runtime) {
        match runtime {
            Runtime::Agent(agent) => {
                self.runtime.agent = agent;
                self.runtime.touched.agent = true;
                if self.runtime.plain_shell {
                    self.runtime.plain_shell = false;
                    self.runtime.touched.run_mode = true;
                }
            }
            Runtime::PlainShell => {
                self.runtime.plain_shell = true;
                self.runtime.touched.run_mode = true;
                self.trusted = false;
            }
        }
    }

    pub(crate) fn tabs(&self) -> &TabChoices {
        &self.tabs
    }

    pub(crate) fn open_in(&self) -> &OpenChoice {
        &self.open_in
    }

    /// Whether the trusted-launch checkbox shows (not for a plain shell).
    pub(crate) fn trusted_shown(&self) -> bool {
        !self.runtime.plain_shell
    }

    pub(crate) fn trusted(&self) -> bool {
        self.trusted && self.trusted_shown()
    }

    /// The flag a trusted launch passes the agent.
    pub(crate) fn trusted_flag(&self) -> &'static str {
        match self.runtime.agent {
            Agent::Codex | Agent::Cursor => "--yolo",
            Agent::Claude => "--dangerously-skip-permissions",
        }
    }

    /// The trusted-launch warning's text before "Uses <flag>.".
    pub(crate) fn trusted_detail(&self) -> String {
        match self.runtime.agent {
            Agent::Codex | Agent::Cursor => format!(
                "{} approvals and sandboxing are bypassed for this session.",
                capitalised(self.runtime.agent.as_label())
            ),
            Agent::Claude => "Claude approval prompts are bypassed for this session.".to_owned(),
        }
    }

    pub(crate) fn is_workspace(&self) -> bool {
        matches!(self.target, Target::Workspace(_))
    }

    pub(crate) fn use_worktree(&self) -> bool {
        self.use_worktree
    }

    fn toggle_use_worktree(&mut self, cache: &BranchCache, now: Instant) -> Vec<ClientMessage> {
        self.use_worktree = !self.use_worktree;
        let mut messages = Vec::new();
        let in_place = self.in_place_default();
        if self.branch.auto {
            if self.use_worktree && self.branch.value == in_place {
                messages.extend(self.seed_suggestion(cache, now));
            } else if !self.use_worktree
                && (self.branch.value.starts_with("wt/") || self.branch.value.is_empty())
            {
                self.branch.value = in_place;
            }
        }
        if self.listing_existing() {
            messages.extend(self.list_existing());
        }
        messages
    }

    pub(crate) fn mode(&self) -> WorktreeMode {
        self.mode
    }

    fn set_mode(&mut self, mode: WorktreeMode) -> Vec<ClientMessage> {
        if mode == self.mode {
            return Vec::new();
        }
        self.mode = mode;
        if self.listing_existing() {
            return self.list_existing();
        }
        Vec::new()
    }

    /// Asks for the existing worktrees afresh.
    fn list_existing(&mut self) -> Vec<ClientMessage> {
        self.existing.options.clear();
        self.existing.loaded = false;
        match &self.target {
            Target::Repo(id) => vec![ClientMessage::ListWorktrees {
                repo_id: id.clone(),
            }],
            Target::Workspace(_) => vec![ClientMessage::InspectWorktreesRoot],
        }
    }

    /// Whether the spawn pins an existing worktree.
    pub(crate) fn pinning(&self) -> bool {
        self.listing_existing()
    }

    pub(crate) fn existing_options(&self) -> &[ExistingOption] {
        &self.existing.options
    }

    pub(crate) fn existing_selected(&self) -> Option<&str> {
        self.existing.selected.as_deref()
    }

    fn selected_option(&self) -> Option<&ExistingOption> {
        let key = self.existing.selected.as_deref()?;
        self.existing.options.iter().find(|o| o.key == key)
    }

    /// The line shown in place of a pick while nothing is picked.
    pub(crate) fn existing_placeholder(&self) -> &'static str {
        let empty = self.existing.options.is_empty();
        match (self.is_workspace(), empty, self.existing.loaded) {
            (true, true, _) => "No worktree groups for this workspace",
            (true, false, _) => "Choose a worktree group…",
            (false, false, _) => "Choose a worktree…",
            (false, true, true) => "This repo has no worktrees",
            (false, true, false) => "Loading…",
        }
    }

    /// What the pick binds: the directory, or how many members it binds.
    pub(crate) fn existing_note(&self) -> Option<String> {
        let option = self.selected_option()?;
        match &option.pin {
            Pin::Worktree(path) => Some(format!("Runs in {path}")),
            Pin::Group(members) => {
                let total = match &self.target {
                    Target::Workspace(id) => {
                        self.workspace(id).map_or(0, |w| w.member_repo_ids.len())
                    }
                    Target::Repo(_) => 0,
                };
                let bound = members.len();
                let plural = if bound == 1 { "" } else { "s" };
                let created = total.saturating_sub(bound);
                Some(format!(
                    "{bound} member{plural} bound; {created} to be created"
                ))
            }
        }
    }

    /// The warning for a pick a session is running in.
    pub(crate) fn existing_warning(&self) -> Option<&'static str> {
        self.selected_option().filter(|o| o.is_active())?;
        Some(if self.is_workspace() {
            "A session is already running in this group. Launching puts a second agent in the same working trees."
        } else {
            "A session is already running in this worktree. Launching puts a second agent in the same working tree."
        })
    }

    pub(crate) fn branch(&self) -> &str {
        &self.branch.value
    }

    pub(crate) fn base(&self) -> &str {
        &self.base.value
    }

    /// The branch field's placeholder.
    pub(crate) fn branch_placeholder(&self, now: Instant) -> String {
        if !self.use_worktree {
            self.in_place_default()
        } else if self.suggestion_pending(now) {
            "Picking a branch name…".to_owned()
        } else {
            "Type a branch name".to_owned()
        }
    }

    /// Whether the remote could not be fetched.
    pub(crate) fn fetch_failed(&self) -> bool {
        self.refs.fetch_failed
    }

    /// Whether Spawn can go: a branch name, or a picked worktree.
    pub(crate) fn can_submit(&self) -> bool {
        if self.pinning() {
            self.selected_option().is_some()
        } else {
            !self.branch.value.trim().is_empty()
        }
    }

    /// The focusable controls, in the order they are drawn.
    pub(crate) fn controls(&self) -> Vec<Control> {
        let mut ring = vec![Control::Close];
        ring.extend(self.targets().into_iter().map(Control::Target));
        ring.extend(Runtime::ALL.into_iter().map(Control::Runtime));
        if self.tabs.current.is_some() {
            ring.push(Control::OpenIn(OpenChoice::CurrentTab));
        }
        ring.push(Control::OpenIn(OpenChoice::NewTab));
        ring.extend(
            self.tabs
                .others
                .iter()
                .map(|t| Control::OpenIn(OpenChoice::Tab(t.id.clone()))),
        );
        if self.trusted_shown() {
            ring.push(Control::Trusted);
        }
        ring.extend(self.worktree_controls());
        ring.push(Control::Cancel);
        if self.can_submit() {
            ring.push(Control::Submit);
        }
        ring
    }

    /// The worktree and branch controls: a repo's checkbox leads, a
    /// workspace's branch field does.
    fn worktree_controls(&self) -> Vec<Control> {
        let mut branch = Vec::new();
        if !self.pinning() {
            branch.push(Control::Branch);
            if self.use_worktree {
                branch.push(Control::Random);
            }
        }
        let mut worktree = vec![Control::UseWorktree];
        if self.use_worktree {
            worktree.push(Control::Mode(WorktreeMode::New));
            worktree.push(Control::Mode(WorktreeMode::Existing));
        }
        if self.pinning() {
            worktree.extend(
                self.existing
                    .options
                    .iter()
                    .map(|o| Control::Existing(o.key.clone())),
            );
        }
        let base = (self.use_worktree && !self.pinning()).then_some(Control::Base);
        if self.is_workspace() {
            branch.into_iter().chain(worktree).chain(base).collect()
        } else {
            worktree.into_iter().chain(branch).chain(base).collect()
        }
    }

    /// The focused control; one that is gone gives way to the first.
    pub(crate) fn focused(&self) -> Control {
        let ring = self.controls();
        if ring.contains(&self.focus) {
            self.focus.clone()
        } else {
            ring.into_iter().next().unwrap_or(Control::Close)
        }
    }

    pub(crate) fn set_focus(&mut self, control: Control) {
        self.focus = control;
    }

    /// Tab (`forward`) or Shift+Tab: the next or previous control, wrapping.
    pub(crate) fn move_focus(&mut self, forward: bool) {
        let ring = self.controls();
        let at = ring.iter().position(|c| *c == self.focus).unwrap_or(0);
        let len = ring.len();
        let next = if forward {
            (at + 1) % len
        } else {
            (at + len - 1) % len
        };
        if let Some(control) = ring.into_iter().nth(next) {
            self.focus = control;
        }
    }

    /// A click on `control`, or Space or Enter while it has the focus.
    pub(crate) fn press(
        &mut self,
        control: &Control,
        cache: &mut BranchCache,
        now: Instant,
    ) -> Outcome {
        self.focus = control.clone();
        let messages = match control {
            Control::Close | Control::Cancel => return Outcome::Close,
            Control::Submit => return self.submit(cache),
            Control::Target(target) if *target != self.target => {
                self.load_target(target.clone(), cache, now)
            }
            Control::Runtime(runtime) => {
                self.select_runtime(*runtime);
                Vec::new()
            }
            Control::OpenIn(choice) => {
                self.open_in = choice.clone();
                Vec::new()
            }
            Control::Trusted => {
                self.trusted = !self.trusted && self.trusted_shown();
                Vec::new()
            }
            Control::UseWorktree => self.toggle_use_worktree(cache, now),
            Control::Mode(mode) => self.set_mode(*mode),
            Control::Existing(key) => {
                self.existing.selected = Some(key.clone());
                Vec::new()
            }
            Control::Random => self.random(cache, now),
            Control::Branch | Control::Base | Control::Target(_) => Vec::new(),
        };
        Outcome::Stay(messages)
    }

    /// Random: forget the cached name and ask for another; the field keeps
    /// its value until the reply.
    fn random(&mut self, cache: &mut BranchCache, now: Instant) -> Vec<ClientMessage> {
        cache.remove(&self.target);
        self.branch.auto = true;
        self.request_suggestion(true, now).into_iter().collect()
    }

    /// Spawn, once: a picked worktree a session is running in asks first.
    pub(crate) fn submit(&mut self, cache: &mut BranchCache) -> Outcome {
        if self.submitted || !self.can_submit() {
            return Outcome::Stay(Vec::new());
        }
        if self.pinning()
            && self
                .selected_option()
                .is_some_and(ExistingOption::is_active)
        {
            self.share = Some(ShareButton::Cancel);
            return Outcome::ConfirmShare;
        }
        self.send(cache)
    }

    fn send(&mut self, cache: &mut BranchCache) -> Outcome {
        self.submitted = true;
        cache.remove(&self.target);
        let (request, open_in, default_change) = self.build_request();
        Outcome::Spawn(Box::new(Submission {
            request,
            open_in,
            default_change,
        }))
    }

    /// The "Share this worktree?" confirm's focused button, while open.
    pub(crate) fn share_confirm(&self) -> Option<ShareButton> {
        self.share
    }

    pub(crate) fn toggle_share_focus(&mut self) {
        self.share = self.share.map(|button| match button {
            ShareButton::Cancel => ShareButton::Launch,
            ShareButton::Launch => ShareButton::Cancel,
        });
    }

    /// The confirm's answer: Launch anyway spawns, Cancel returns to the
    /// form.
    pub(crate) fn answer_share(&mut self, button: ShareButton, cache: &mut BranchCache) -> Outcome {
        if self.share.take().is_none() {
            return Outcome::Stay(Vec::new());
        }
        match button {
            ShareButton::Launch => self.send(cache),
            ShareButton::Cancel => Outcome::Stay(Vec::new()),
        }
    }

    /// The spawn the form describes, where it opens, and the worktree
    /// default to save when the checkbox differs from it.
    pub(crate) fn build_request(&self) -> (SpawnRequest, OpenIn, Option<ClientMessage>) {
        let plain = self.runtime.plain_shell;
        let request = SpawnRequest {
            label: None,
            target: self.spawn_target(),
            mode: if plain {
                SessionMode::PlainShell
            } else {
                SessionMode::Interactive
            },
            initial_prompt: None,
            dangerously_skip_permissions: self.trusted(),
            agent_options: agent_options(self.runtime.agent),
            model: None,
            extra_env: Vec::new(),
            prompt_injector: None,
            request_id: None,
        };
        (request, self.placement(), self.worktree_default_change())
    }

    fn spawn_target(&self) -> SpawnTarget {
        let pick = self.selected_option().filter(|_| self.pinning());
        let branch_name = pick.map_or_else(
            || self.branch.value.trim().to_owned(),
            |o| o.branch_name.clone(),
        );
        let base = self.base.value.trim();
        let base_branch =
            (self.use_worktree && pick.is_none() && !base.is_empty()).then(|| base.to_owned());
        match &self.target {
            Target::Repo(id) => SpawnTarget::Single {
                repo_id: id.clone(),
                branch_name,
                base_branch,
                use_worktree: self.use_worktree,
                checkout_strategy: None,
                worktree_reuse: WorktreeReusePolicy::default(),
                existing_worktree: pick.and_then(|o| match &o.pin {
                    Pin::Worktree(path) => Some(path.clone()),
                    Pin::Group(_) => None,
                }),
            },
            Target::Workspace(id) => SpawnTarget::Workspace {
                workspace_id: id.clone(),
                branch_name,
                base_branch,
                use_worktree: self.use_worktree,
                worktree_reuse: WorktreeReusePolicy::default(),
                existing_worktrees: pick
                    .and_then(|o| match &o.pin {
                        Pin::Group(members) => Some(members.clone()),
                        Pin::Worktree(_) => None,
                    })
                    .unwrap_or_default(),
            },
        }
    }

    fn placement(&self) -> OpenIn {
        match &self.open_in {
            OpenChoice::CurrentTab => current_tab_open_in(&self.tabs),
            OpenChoice::NewTab => OpenIn::NewTab,
            OpenChoice::Tab(id) => OpenIn::Tab(id.clone()),
        }
    }

    fn worktree_default_change(&self) -> Option<ClientMessage> {
        let value = self.use_worktree;
        match &self.target {
            Target::Repo(id) => self
                .repo(id)
                .filter(|r| r.default_use_worktree != value)
                .map(|r| ClientMessage::SetRepoWorktreeDefault {
                    repo_id: r.id.clone(),
                    value,
                }),
            Target::Workspace(id) => self
                .workspace(id)
                .filter(|w| w.default_use_worktree != value)
                .map(|w| ClientMessage::SetWorkspaceWorktreeDefault {
                    workspace_id: w.id.clone(),
                    value,
                }),
        }
    }
}

/// The dialog's first target: the focused session's repo or workspace,
/// else the first workspace, else the first repo.
pub(crate) fn initial_target(
    focused: Option<&SessionSnapshot>,
    repos: &[RepoEntry],
    workspaces: &[WorkspaceEntry],
) -> Option<Target> {
    let has_repo = |id: &str| repos.iter().any(|r| r.id == id);
    let has_workspace = |id: &str| workspaces.iter().any(|w| w.id == id);
    let from_session = focused.and_then(|session| {
        if let Some(id) = session
            .workspace_id
            .as_deref()
            .filter(|id| has_workspace(id))
        {
            return Some(Target::Workspace(id.to_owned()));
        }
        let repo_id = &session.members.first()?.repo_id;
        has_repo(repo_id).then(|| Target::Repo(repo_id.clone()))
    });
    from_session
        .or_else(|| workspaces.first().map(|w| Target::Workspace(w.id.clone())))
        .or_else(|| repos.first().map(|r| Target::Repo(r.id.clone())))
}

/// `word` with its first letter in upper case, to open a sentence.
fn capitalised(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

fn is_plain_shell(config: Option<&SpawnConfig>) -> bool {
    config.is_some_and(|c| c.mode == SessionMode::PlainShell)
}

/// The agent's options with nothing chosen beyond the agent itself.
fn agent_options(agent: Agent) -> AgentOptions {
    match agent {
        Agent::Claude => AgentOptions::Claude {
            permission_mode: None,
        },
        Agent::Codex => AgentOptions::Codex { sandbox: None },
        Agent::Cursor => AgentOptions::Cursor {
            plan_mode: false,
            sandbox: None,
        },
    }
}

/// Where a spawn asked for the current tab opens: the tab on screen when it
/// can hold panes, else a tab of its own.
pub(crate) fn current_tab_open_in(tabs: &TabChoices) -> OpenIn {
    match &tabs.current {
        Some(current) => OpenIn::CurrentTab(current.id.clone()),
        None => OpenIn::NewTab,
    }
}

/// `origin/<default>` when the remote carries the local default (another
/// remote's copy when only that one does), else the local default.
pub(crate) fn prefer_remote_base(local_default: &str, remote_branches: &[String]) -> String {
    let candidates: Vec<&String> = remote_branches
        .iter()
        .filter(|r| r.split_once('/').map_or(r.as_str(), |(_, rest)| rest) == local_default)
        .collect();
    candidates
        .iter()
        .find(|r| r.starts_with("origin/"))
        .or_else(|| candidates.first())
        .map_or_else(|| local_default.to_owned(), |r| (*r).clone())
}

/// A label for a worktree whose branch is unknown: the group directory
/// without its `wt.` prefix, else the directory's name.
fn branch_from_worktree_path(path: &str) -> String {
    let leaf = path
        .split(['/', '\\'])
        .rfind(|part| !part.is_empty())
        .unwrap_or_default();
    match leaf.strip_prefix("wt.") {
        Some(rest) => rest.to_owned(),
        None if leaf.is_empty() => "worktree".to_owned(),
        None => leaf.to_owned(),
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a size shown to one decimal place"
)]
fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let kb = bytes as f64 / 1024.0;
    if kb < 1024.0 {
        return format!("{kb:.1} KB");
    }
    let mb = kb / 1024.0;
    if mb < 1024.0 {
        return format!("{mb:.1} MB");
    }
    format!("{:.2} GB", mb / 1024.0)
}

fn human_relative_time(now_unix: i64, then_unix: i64) -> String {
    let delta = now_unix - then_unix;
    if delta < 60 {
        "just now".to_owned()
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86400)
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect and panic; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wire(msg: &ClientMessage) -> serde_json::Value {
        serde_json::to_value(msg).expect("a message encodes")
    }

    fn has(sent: &[ClientMessage], msg: &ClientMessage) -> bool {
        sent.iter().any(|m| wire(m) == wire(msg))
    }

    fn same(sent: &[ClientMessage], expected: &[ClientMessage]) -> bool {
        sent.iter().map(wire).eq(expected.iter().map(wire))
    }

    fn repo(id: &str) -> RepoEntry {
        serde_json::from_value(json!({ "id": id, "name": id, "path": format!("C:/{id}") }))
            .expect("repo fixture")
    }

    fn workspace(id: &str, members: &[&str]) -> WorkspaceEntry {
        serde_json::from_value(json!({ "id": id, "name": id, "member_repo_ids": members }))
            .expect("workspace fixture")
    }

    fn config(kind: &str, mode: &str) -> SpawnConfig {
        serde_json::from_value(json!({
            "target": { "kind": "standalone" },
            "mode": mode,
            "dangerously_skip_permissions": false,
            "agent_options": { "kind": kind },
            "model": null,
        }))
        .expect("spawn config fixture")
    }

    fn session_in(repo_id: Option<&str>, workspace_id: Option<&str>) -> SessionSnapshot {
        let members: Vec<_> = repo_id
            .map(|id| json!({ "repo_id": id, "repo_name": id, "branch": "main", "worktree_path": "" }))
            .into_iter()
            .collect();
        serde_json::from_value(json!({
            "id": "s1", "label": "s1", "kind": "single", "members": members,
            "workspace_id": workspace_id, "status": "idle", "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z", "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": null },
            "recent_actions": [], "agent": "claude",
        }))
        .expect("session fixture")
    }

    fn tabs(current: Option<&str>, others: &[&str]) -> TabChoices {
        let choice = |id: &str| TabChoice {
            id: id.to_owned(),
            name: format!("{id} name"),
        };
        TabChoices {
            current: current.map(choice),
            others: others.iter().map(|id| choice(id)).collect(),
        }
    }

    struct Setup {
        repos: Vec<RepoEntry>,
        workspaces: Vec<WorkspaceEntry>,
        focused: Option<SessionSnapshot>,
        tabs: TabChoices,
    }

    impl Setup {
        fn repos(repos: Vec<RepoEntry>) -> Self {
            Self {
                repos,
                workspaces: Vec::new(),
                focused: None,
                tabs: tabs(Some("t1"), &[]),
            }
        }

        fn open(&self, cache: &mut BranchCache) -> (SpawnForm, Vec<ClientMessage>) {
            SpawnForm::open(
                FormInputs {
                    repos: &self.repos,
                    workspaces: &self.workspaces,
                    focused: self.focused.as_ref(),
                    tabs: self.tabs.clone(),
                },
                cache,
                Instant::now(),
            )
            .expect("something to spawn into")
        }
    }

    fn open_repo(repo: RepoEntry) -> SpawnForm {
        Setup::repos(vec![repo]).open(&mut BranchCache::default()).0
    }

    fn press(form: &mut SpawnForm, control: &Control) -> Outcome {
        form.press(control, &mut BranchCache::default(), Instant::now())
    }

    fn branches(
        repo_id: &str,
        current: Option<&str>,
        local: &[&str],
        remote: &[&str],
    ) -> DaemonMessage {
        DaemonMessage::Branches {
            repo_id: repo_id.to_owned(),
            branches: local.iter().map(|b| (*b).to_owned()).collect(),
            current: current.map(str::to_owned),
            remote_branches: remote.iter().map(|b| (*b).to_owned()).collect(),
        }
    }

    fn submission(outcome: Outcome) -> Submission {
        match outcome {
            Outcome::Spawn(submission) => *submission,
            other => panic!("expected a spawn, got {other:?}"),
        }
    }

    fn worktree(path: &str, branch: &str, status: RootWorktreeStatus) -> WorktreeInfo {
        WorktreeInfo {
            branch: branch.to_owned(),
            path: path.to_owned(),
            status,
            group_path: None,
            size_bytes: None,
            last_modified_unix: None,
        }
    }

    #[test]
    fn initial_target_is_focused_panes_repo() {
        let repos = vec![repo("r1"), repo("r2")];
        let workspaces = vec![workspace("w1", &["r1", "r2"])];
        let focused = session_in(Some("r2"), None);
        assert_eq!(
            initial_target(Some(&focused), &repos, &workspaces),
            Some(Target::Repo("r2".to_owned()))
        );
        let in_workspace = session_in(Some("r1"), Some("w1"));
        assert_eq!(
            initial_target(Some(&in_workspace), &repos, &workspaces),
            Some(Target::Workspace("w1".to_owned())),
            "a workspace session preselects its workspace"
        );
    }

    #[test]
    fn initial_target_falls_back_to_first_workspace_then_repo() {
        let repos = vec![repo("r1"), repo("r2")];
        let workspaces = vec![workspace("w1", &["r1"]), workspace("w2", &["r2"])];
        let stranger = session_in(Some("gone"), None);
        assert_eq!(
            initial_target(Some(&stranger), &repos, &workspaces),
            Some(Target::Workspace("w1".to_owned())),
            "an unregistered repo falls back"
        );
        assert_eq!(
            initial_target(None, &repos, &[]),
            Some(Target::Repo("r1".to_owned()))
        );
        assert_eq!(initial_target(None, &[], &[]), None);
    }

    #[test]
    fn runtime_defaults_to_last_spawn_then_last_agent_then_claude() {
        let mut from_config = repo("r1");
        from_config.last_spawn_config = Some(config("codex", "interactive"));
        from_config.last_agent = Some(Agent::Cursor);
        assert_eq!(
            open_repo(from_config).runtime(),
            Runtime::Agent(Agent::Codex)
        );

        let mut from_agent = repo("r1");
        from_agent.last_agent = Some(Agent::Cursor);
        assert_eq!(
            open_repo(from_agent).runtime(),
            Runtime::Agent(Agent::Cursor)
        );
        assert_eq!(
            open_repo(repo("r1")).runtime(),
            Runtime::Agent(Agent::Claude)
        );

        let mut shell = repo("r1");
        shell.last_spawn_config = Some(config("claude", "plain_shell"));
        assert_eq!(open_repo(shell).runtime(), Runtime::PlainShell);
        let mut headless = repo("r1");
        headless.last_spawn_config = Some(config("claude", "headless"));
        assert_eq!(
            open_repo(headless).runtime(),
            Runtime::Agent(Agent::Claude),
            "only a plain shell sticks as the run mode"
        );
    }

    #[test]
    fn touched_runtime_ignores_later_defaults() {
        let mut codex = repo("r2");
        codex.last_spawn_config = Some(config("codex", "plain_shell"));
        let mut form = Setup::repos(vec![repo("r1"), codex])
            .open(&mut BranchCache::default())
            .0;
        press(&mut form, &Control::Runtime(Runtime::Agent(Agent::Cursor)));
        press(&mut form, &Control::Target(Target::Repo("r2".to_owned())));
        assert_eq!(
            form.runtime(),
            Runtime::PlainShell,
            "the untouched run mode still follows r2"
        );
        press(&mut form, &Control::Runtime(Runtime::Agent(Agent::Cursor)));
        press(&mut form, &Control::Target(Target::Repo("r1".to_owned())));
        assert_eq!(
            form.runtime(),
            Runtime::Agent(Agent::Cursor),
            "both touched now"
        );
    }

    #[test]
    fn plain_shell_hides_and_clears_trusted() {
        let mut form = open_repo(repo("r1"));
        press(&mut form, &Control::Trusted);
        assert!(form.trusted());
        assert!(form.controls().contains(&Control::Trusted));
        press(&mut form, &Control::Runtime(Runtime::PlainShell));
        assert!(!form.trusted_shown());
        assert!(!form.controls().contains(&Control::Trusted));
        press(&mut form, &Control::Runtime(Runtime::Agent(Agent::Claude)));
        assert!(!form.trusted(), "cleared, not just hidden");
        press(&mut form, &Control::Trusted);
        press(&mut form, &Control::Runtime(Runtime::PlainShell));
        form.branch.value = "b".to_owned();
        let (request, _, _) = form.build_request();
        assert!(!request.dangerously_skip_permissions);
        assert_eq!(request.mode, SessionMode::PlainShell);
    }

    #[test]
    fn trusted_flag_label_follows_runtime() {
        let mut form = open_repo(repo("r1"));
        assert_eq!(form.trusted_flag(), "--dangerously-skip-permissions");
        assert_eq!(
            form.trusted_detail(),
            "Claude approval prompts are bypassed for this session."
        );
        press(&mut form, &Control::Runtime(Runtime::Agent(Agent::Codex)));
        assert_eq!(form.trusted_flag(), "--yolo");
        assert_eq!(
            form.trusted_detail(),
            "Codex approvals and sandboxing are bypassed for this session."
        );
        press(&mut form, &Control::Runtime(Runtime::Agent(Agent::Cursor)));
        assert_eq!(form.trusted_flag(), "--yolo");
        assert!(!form.trusted(), "trusted launch starts off");
    }

    #[test]
    fn trusted_detail_opens_with_a_capital() {
        let mut form = open_repo(repo("r1"));
        press(&mut form, &Control::Runtime(Runtime::Agent(Agent::Cursor)));
        assert_eq!(
            form.trusted_detail(),
            "Cursor approvals and sandboxing are bypassed for this session."
        );
        assert_eq!(
            Runtime::Agent(Agent::Cursor).label(),
            "cursor",
            "the button stays"
        );
        assert_eq!(capitalised(""), "");
    }

    #[test]
    fn unrelated_message_changes_nothing() {
        let mut form = open_repo(repo("r1"));
        let unrelated = [
            DaemonMessage::PtyOutput {
                session_id: "s1".to_owned(),
                data_b64: String::new(),
            },
            DaemonMessage::SessionUpdated {
                session: session_in(Some("r1"), None),
                request_id: None,
            },
            branches("other", Some("main"), &["main"], &[]),
            DaemonMessage::WorktreesRootSnapshot {
                root: "C:/wt".to_owned(),
                is_override: false,
                entries: Vec::new(),
            },
        ];
        for msg in &unrelated {
            assert!(form.on_message(msg).is_none(), "{msg:?} is not the form's");
        }
        assert!(
            form.on_message(&branches("r1", None, &[], &[])).is_some(),
            "its own repo's branches are"
        );
    }

    #[test]
    fn use_worktree_seeded_from_repo_default() {
        let mut in_place = repo("r1");
        in_place.default_use_worktree = false;
        let (form, sent) = Setup::repos(vec![in_place]).open(&mut BranchCache::default());
        assert!(!form.use_worktree());
        assert!(
            !sent
                .iter()
                .any(|m| matches!(m, ClientMessage::SuggestBranchName { .. })),
            "no suggestion for an in-place spawn"
        );
        assert!(open_repo(repo("r1")).use_worktree());
    }

    #[test]
    fn base_branch_prefers_origin_default_until_edited() {
        let mut main = repo("r1");
        main.default_branch = Some("main".to_owned());
        let mut form = open_repo(main);
        assert_eq!(form.base(), "main");
        form.on_message(&branches(
            "r1",
            Some("main"),
            &["main"],
            &["upstream/main", "origin/main"],
        ));
        assert_eq!(form.base(), "origin/main");
        form.edit_base("dev");
        form.on_message(&branches(
            "r1",
            Some("main"),
            &["main"],
            &["origin/main", "origin/x"],
        ));
        assert_eq!(form.base(), "dev", "an edit sticks");
        assert_eq!(
            prefer_remote_base("main", &["upstream/main".to_owned()]),
            "upstream/main"
        );
        assert_eq!(prefer_remote_base("main", &[]), "main");
        assert_eq!(
            prefer_remote_base("main", &["origin/mainline".to_owned()]),
            "main"
        );
    }

    #[test]
    fn suggestion_fills_branch_until_edited() {
        let mut cache = BranchCache::default();
        let setup = Setup::repos(vec![repo("r1")]);
        let (mut form, sent) = setup.open(&mut cache);
        assert!(has(
            &sent,
            &ClientMessage::SuggestBranchName {
                target: SuggestTarget::Repo {
                    repo_id: "r1".to_owned()
                },
            }
        ));
        assert!(form.suggestion_pending(Instant::now()));
        let r1 = Target::Repo("r1".to_owned());
        form.on_suggestion(&r1, "wt/brave-fox");
        assert_eq!(form.branch(), "wt/brave-fox");
        assert!(!form.suggestion_pending(Instant::now()));

        cache.insert(r1.clone(), "wt/brave-fox".to_owned());
        let (reopened, sent) = setup.open(&mut cache);
        assert_eq!(reopened.branch(), "wt/brave-fox", "cached per target");
        assert!(
            !sent
                .iter()
                .any(|m| matches!(m, ClientMessage::SuggestBranchName { .. }))
        );

        form.edit_branch("mine", &mut cache);
        form.on_suggestion(&r1, "wt/late-owl");
        assert_eq!(form.branch(), "mine", "an edit stops the suggestions");
        assert!(cache.get(&r1).is_none(), "an edit drops the cached name");

        let Outcome::Stay(sent) = press(&mut form, &Control::Random) else {
            panic!("Random keeps the dialog open");
        };
        assert_eq!(sent.len(), 1, "Random asks again: {sent:?}");
        assert_eq!(form.branch(), "mine", "kept until the reply");
        form.on_suggestion(&r1, "wt/new-name");
        assert_eq!(form.branch(), "wt/new-name");
    }

    #[test]
    fn suggestion_gives_up_after_ten_seconds() {
        let start = Instant::now();
        let (mut form, _) = SpawnForm::open(
            FormInputs {
                repos: &[repo("r1")],
                workspaces: &[],
                focused: None,
                tabs: TabChoices::default(),
            },
            &mut BranchCache::default(),
            start,
        )
        .expect("a form");
        assert!(form.suggestion_pending(start + Duration::from_secs(9)));
        assert_eq!(form.branch_placeholder(start), "Picking a branch name…");
        let later = start + SUGGESTION_TIMEOUT;
        assert!(!form.suggestion_pending(later));
        assert_eq!(form.branch_placeholder(later), "Type a branch name");
        assert!(
            form.request_suggestion(false, later).is_some(),
            "a new request may go once the wait is over"
        );
    }

    #[test]
    fn worktree_off_shows_current_branch() {
        let mut main = repo("r1");
        main.default_branch = Some("main".to_owned());
        let mut form = open_repo(main);
        press(&mut form, &Control::UseWorktree);
        assert_eq!(form.branch(), "main", "the default before the refs arrive");
        form.on_message(&branches("r1", Some("feature"), &["feature", "main"], &[]));
        assert_eq!(form.branch(), "feature", "the branch checked out");
        assert_eq!(form.branch_placeholder(Instant::now()), "feature");
        let Outcome::Stay(sent) = press(&mut form, &Control::UseWorktree) else {
            panic!("the toggle keeps the dialog open");
        };
        assert_eq!(form.branch(), "", "back on: waits for a suggestion");
        assert!(
            sent.is_empty(),
            "the request sent on open is still on the way"
        );
    }

    #[test]
    fn existing_worktree_pins_existing_worktree() {
        let mut form = open_repo(repo("r1"));
        let Outcome::Stay(sent) = press(&mut form, &Control::Mode(WorktreeMode::Existing)) else {
            panic!("the mode keeps the dialog open");
        };
        assert!(same(
            &sent,
            &[ClientMessage::ListWorktrees {
                repo_id: "r1".to_owned()
            }]
        ));
        assert_eq!(form.existing_placeholder(), "Loading…");
        assert!(!form.can_submit(), "nothing picked yet");
        form.on_message(&DaemonMessage::Worktrees {
            repo_id: "r1".to_owned(),
            worktrees: vec![
                worktree("C:/wt/wt.old/r1", "", RootWorktreeStatus::Detached),
                worktree("C:/wt/x", "feat", RootWorktreeStatus::Stale),
            ],
        });
        assert_eq!(form.existing_options().len(), 2);
        press(&mut form, &Control::Existing("C:/wt/wt.old/r1".to_owned()));
        assert_eq!(
            form.existing_note().as_deref(),
            Some("Runs in C:/wt/wt.old/r1")
        );
        let spawn = submission(form.submit(&mut BranchCache::default()));
        let SpawnTarget::Single {
            existing_worktree,
            branch_name,
            base_branch,
            use_worktree,
            ..
        } = spawn.request.target
        else {
            panic!("a single-repo spawn");
        };
        assert_eq!(existing_worktree.as_deref(), Some("C:/wt/wt.old/r1"));
        assert_eq!(
            branch_name, "r1",
            "a detached worktree is named after its directory"
        );
        assert_eq!(base_branch, None);
        assert!(use_worktree);
    }

    #[test]
    fn active_existing_worktree_asks_before_sharing() {
        let mut form = open_repo(repo("r1"));
        press(&mut form, &Control::Mode(WorktreeMode::Existing));
        form.on_message(&DaemonMessage::Worktrees {
            repo_id: "r1".to_owned(),
            worktrees: vec![worktree("C:/wt/x", "feat", RootWorktreeStatus::Active)],
        });
        press(&mut form, &Control::Existing("C:/wt/x".to_owned()));
        assert!(form.existing_warning().is_some());
        let mut cache = BranchCache::default();
        assert!(matches!(form.submit(&mut cache), Outcome::ConfirmShare));
        assert_eq!(
            form.share_confirm(),
            Some(ShareButton::Cancel),
            "the safe button first"
        );
        assert!(matches!(
            form.answer_share(ShareButton::Cancel, &mut cache),
            Outcome::Stay(_)
        ));
        form.submit(&mut cache);
        let spawn = submission(form.answer_share(ShareButton::Launch, &mut cache));
        assert!(matches!(
            spawn.request.target,
            SpawnTarget::Single {
                existing_worktree: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn workspace_request_uses_one_branch_for_all_members() {
        let mut first = repo("r1");
        first.default_branch = Some("main".to_owned());
        let setup = Setup {
            repos: vec![first, repo("r2")],
            workspaces: vec![workspace("w1", &["r1", "r2"])],
            focused: None,
            tabs: tabs(Some("t1"), &[]),
        };
        let (mut form, sent) = setup.open(&mut BranchCache::default());
        assert_eq!(form.target(), &Target::Workspace("w1".to_owned()));
        assert!(has(
            &sent,
            &ClientMessage::ListBranches {
                repo_id: "r1".to_owned()
            }
        ));
        assert!(has(
            &sent,
            &ClientMessage::FetchRepo {
                repo_id: "r2".to_owned()
            }
        ));
        form.on_message(&branches("r1", Some("main"), &["main"], &["origin/main"]));
        form.edit_branch("feature", &mut BranchCache::default());
        let spawn = submission(form.submit(&mut BranchCache::default()));
        assert_eq!(
            spawn.request.target,
            SpawnTarget::Workspace {
                workspace_id: "w1".to_owned(),
                branch_name: "feature".to_owned(),
                base_branch: Some("origin/main".to_owned()),
                use_worktree: true,
                worktree_reuse: WorktreeReusePolicy::default(),
                existing_worktrees: Vec::new(),
            }
        );
    }

    #[test]
    fn workspace_existing_group_pins_its_members() {
        let setup = Setup {
            repos: vec![repo("r1"), repo("r2")],
            workspaces: vec![workspace("w1", &["r1", "r2"])],
            focused: None,
            tabs: tabs(None, &[]),
        };
        let mut form = setup.open(&mut BranchCache::default()).0;
        let Outcome::Stay(sent) = press(&mut form, &Control::Mode(WorktreeMode::Existing)) else {
            panic!("stays open");
        };
        assert!(same(&sent, &[ClientMessage::InspectWorktreesRoot]));
        let entry = |workspace_id: &str, path: &str| -> RootWorktreeEntry {
            serde_json::from_value(json!({
                "path": format!("C:/wt/wt.{path}"), "anchor": "a", "branch_slug": path,
                "members": [], "status": { "kind": "stale" }, "session_id": null,
                "size_bytes": 2048, "last_modified_unix": 1000,
                "launch": { "kind": "workspace", "workspace_id": workspace_id, "branch": null,
                    "members": [{ "repo_id": "r1", "path": format!("C:/wt/wt.{path}/r1") }] },
            }))
            .expect("root entry fixture")
        };
        form.on_message(&DaemonMessage::WorktreesRootSnapshot {
            root: "C:/wt".to_owned(),
            is_override: false,
            entries: vec![entry("w1", "ours"), entry("w9", "theirs")],
        });
        let options = form.existing_options();
        assert_eq!(options.len(), 1, "only this workspace's groups");
        assert_eq!(
            options[0].label(1000 + 7200),
            "ours — stale, 2.0 KB, 2h ago"
        );
        let key = options[0].key.clone();
        press(&mut form, &Control::Existing(key));
        assert_eq!(
            form.existing_note().as_deref(),
            Some("1 member bound; 1 to be created")
        );
        let spawn = submission(form.submit(&mut BranchCache::default()));
        let SpawnTarget::Workspace {
            branch_name,
            existing_worktrees,
            base_branch,
            ..
        } = spawn.request.target
        else {
            panic!("a workspace spawn");
        };
        assert_eq!(branch_name, "ours");
        assert_eq!(existing_worktrees.len(), 1);
        assert_eq!(base_branch, None);
        assert_eq!(spawn.open_in, OpenIn::NewTab, "no current tab to open in");
    }

    #[test]
    fn worktree_default_message_only_when_changed() {
        let mut form = open_repo(repo("r1"));
        form.edit_branch("feature", &mut BranchCache::default());
        assert!(form.build_request().2.is_none(), "unchanged");
        press(&mut form, &Control::UseWorktree);
        assert!(same(
            &form.build_request().2.into_iter().collect::<Vec<_>>(),
            &[ClientMessage::SetRepoWorktreeDefault {
                repo_id: "r1".to_owned(),
                value: false,
            }]
        ));
    }

    #[test]
    fn open_in_defaults_to_current_tab() {
        let mut setup = Setup::repos(vec![repo("r1")]);
        setup.tabs = tabs(Some("t1"), &["t2"]);
        let mut form = setup.open(&mut BranchCache::default()).0;
        form.edit_branch("b", &mut BranchCache::default());
        assert_eq!(form.open_in(), &OpenChoice::CurrentTab);
        assert_eq!(form.build_request().1, OpenIn::CurrentTab("t1".to_owned()));
        press(
            &mut form,
            &Control::OpenIn(OpenChoice::Tab("t2".to_owned())),
        );
        assert_eq!(form.build_request().1, OpenIn::Tab("t2".to_owned()));
        form.set_tabs(tabs(Some("t1"), &[]));
        assert_eq!(
            form.open_in(),
            &OpenChoice::CurrentTab,
            "a closed tab falls back"
        );
        form.set_tabs(tabs(None, &[]));
        assert_eq!(form.build_request().1, OpenIn::NewTab);
    }

    #[test]
    fn submit_blocks_a_second_submit_and_empty_branch() {
        let mut form = open_repo(repo("r1"));
        let mut cache = BranchCache::default();
        assert!(
            matches!(form.submit(&mut cache), Outcome::Stay(_)),
            "no branch yet"
        );
        assert!(!form.controls().contains(&Control::Submit));
        let r1 = Target::Repo("r1".to_owned());
        form.on_suggestion(&r1, "wt/x");
        cache.insert(r1.clone(), "wt/x".to_owned());
        assert!(matches!(form.submit(&mut cache), Outcome::Spawn(_)));
        assert!(
            cache.get(&r1).is_none(),
            "a submit consumes the cached name"
        );
        assert!(matches!(form.submit(&mut cache), Outcome::Stay(_)), "once");
    }

    #[test]
    fn registry_change_snaps_to_first_target() {
        let (mut form, _) =
            Setup::repos(vec![repo("r1"), repo("r2")]).open(&mut BranchCache::default());
        press(&mut form, &Control::Target(Target::Repo("r2".to_owned())));
        let sent = form
            .set_registry(
                &[repo("r1")],
                &[],
                &mut BranchCache::default(),
                Instant::now(),
            )
            .expect("r1 is left");
        assert_eq!(form.target(), &Target::Repo("r1".to_owned()));
        assert!(has(
            &sent,
            &ClientMessage::ListBranches {
                repo_id: "r1".to_owned()
            }
        ));
        assert!(
            form.set_registry(&[], &[], &mut BranchCache::default(), Instant::now())
                .is_none()
        );
    }

    #[test]
    fn focus_ring_wraps_both_ways() {
        let mut form = open_repo(repo("r1"));
        assert_eq!(form.focused(), Control::Branch, "the branch field first");
        let ring = form.controls();
        assert_eq!(
            ring,
            [
                Control::Close,
                Control::Target(Target::Repo("r1".to_owned())),
                Control::Runtime(Runtime::Agent(Agent::Claude)),
                Control::Runtime(Runtime::Agent(Agent::Codex)),
                Control::Runtime(Runtime::Agent(Agent::Cursor)),
                Control::Runtime(Runtime::PlainShell),
                Control::OpenIn(OpenChoice::CurrentTab),
                Control::OpenIn(OpenChoice::NewTab),
                Control::Trusted,
                Control::UseWorktree,
                Control::Mode(WorktreeMode::New),
                Control::Mode(WorktreeMode::Existing),
                Control::Branch,
                Control::Random,
                Control::Base,
                Control::Cancel,
            ]
        );
        form.move_focus(true);
        assert_eq!(form.focused(), Control::Random);
        form.set_focus(Control::Cancel);
        form.move_focus(true);
        assert_eq!(form.focused(), Control::Close, "wraps forward");
        form.move_focus(false);
        assert_eq!(form.focused(), Control::Cancel, "and back");
    }

    #[test]
    fn labels_follow_the_tauri_wording() {
        assert_eq!(
            branch_from_worktree_path("C:\\wt\\wt.brave-fox\\"),
            "brave-fox"
        );
        assert_eq!(branch_from_worktree_path(""), "worktree");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.00 GB");
        assert_eq!(human_relative_time(100, 90), "just now");
        assert_eq!(human_relative_time(90_000, 0), "1d ago");
        let form = Setup {
            repos: vec![repo("r1")],
            workspaces: vec![workspace("w1", &["r1", "r2"])],
            focused: None,
            tabs: TabChoices::default(),
        }
        .open(&mut BranchCache::default())
        .0;
        assert_eq!(
            form.target_label(&Target::Repo("r1".to_owned())),
            "[REPO]  r1"
        );
        assert_eq!(
            form.target_label(&Target::Workspace("w1".to_owned())),
            "[WS]    w1 (2 repos)"
        );
    }
}
