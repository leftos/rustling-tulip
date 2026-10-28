//! "Launch last again": the stored spawn config a repo or workspace row
//! replays, where it lands, and the wait for a fresh branch name before a
//! worktree launch. Plain Rust, so every rule is unit-tested; the view calls
//! it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use protocol::{RepoEntry, SpawnConfig, SpawnRequest, SpawnTarget, SuggestTarget, WorkspaceEntry};

use crate::spawn_form::OpenChoice;
use crate::spawns::OpenIn;

/// Two launch gestures for one target closer together than this are one
/// gesture: the second click of a double-click, or a press the pointer
/// duplicated.
pub(crate) const REPEAT_INTERVAL: Duration = Duration::from_millis(500);

/// The container row a launch comes from: the two kinds that keep a last
/// spawn config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container<'a> {
    Repo(&'a str),
    Workspace(&'a str),
}

/// Where a launch lands, as the click named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// The tab that was active when the click arrived.
    CurrentTab,
    NewTab,
    /// A tab by id.
    Tab(String),
}

impl Placement {
    /// The placement an Open-in choice names.
    #[must_use]
    pub(crate) fn from_open_in(open_in: OpenIn) -> Self {
        match open_in {
            OpenIn::CurrentTab(id) | OpenIn::Tab(id) => Self::Tab(id),
            OpenIn::NewTab | OpenIn::Pane(_) => Self::NewTab,
        }
    }

    /// Where a spawn placed by this goes. A `CurrentTab` that reached here
    /// unresolved has no tab to reclaim, so it opens one of its own.
    #[must_use]
    pub(crate) fn open_in(&self) -> OpenIn {
        match self {
            Self::CurrentTab | Self::NewTab => OpenIn::NewTab,
            Self::Tab(id) => OpenIn::Tab(id.clone()),
        }
    }

    /// The spawn dialog's Open-in choice this placement names, so a trusted
    /// launch opens the dialog with the same tab preselected.
    #[must_use]
    pub(crate) fn open_choice(&self) -> OpenChoice {
        match self {
            Self::CurrentTab => OpenChoice::CurrentTab,
            Self::NewTab => OpenChoice::NewTab,
            Self::Tab(id) => OpenChoice::Tab(id.clone()),
        }
    }
}

/// The spawn config `container` last launched with, cloned; `None` when it
/// never launched or its record left the registry.
#[must_use]
pub(crate) fn stored_config(
    container: Container<'_>,
    repos: &[RepoEntry],
    workspaces: &[WorkspaceEntry],
) -> Option<SpawnConfig> {
    match container {
        Container::Repo(id) => repos
            .iter()
            .find(|repo| repo.id == id)
            .and_then(|repo| repo.last_spawn_config.clone()),
        Container::Workspace(id) => workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .and_then(|workspace| workspace.last_spawn_config.clone()),
    }
}

/// The repo or workspace a worktree config asks a fresh branch name about;
/// `None` when the config runs in place or outside a repo.
#[must_use]
pub(crate) fn suggestion_target(config: &SpawnConfig) -> Option<SuggestTarget> {
    if !uses_worktree(&config.target) {
        return None;
    }
    config.target.suggest_target()
}

/// Whether a target creates worktrees, so a replay needs a branch name no
/// other launch holds.
fn uses_worktree(target: &SpawnTarget) -> bool {
    matches!(
        target,
        SpawnTarget::Single {
            use_worktree: true,
            ..
        } | SpawnTarget::Workspace {
            use_worktree: true,
            ..
        }
    )
}

/// The request a replay sends: the stored config as a duplicate, with the
/// fresh branch name a worktree launch waited for.
#[must_use]
pub(crate) fn replay_request(config: &SpawnConfig, fresh_branch: Option<String>) -> SpawnRequest {
    config.to_duplicate_request(fresh_branch)
}

/// A launch waiting for the daemon's fresh branch name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pending {
    pub config: SpawnConfig,
    pub placement: Placement,
    pub target: SuggestTarget,
    /// Tells this wait from the one a later click started.
    pub generation: u64,
    /// The id the request carried, so only its own reply finishes this wait.
    pub request_id: String,
}

/// The waits a client has in flight, one per target it asked about, and the
/// last gesture for the repeat guard.
#[derive(Debug, Default)]
pub(crate) struct State {
    pending: HashMap<SuggestTarget, Pending>,
    next_generation: u64,
    last_activation: Option<(SuggestTarget, Instant)>,
}

impl State {
    /// Starts a wait for `target`, replacing that target's earlier wait and
    /// leaving every other target's alone, and returns its generation and the
    /// `request_id` its reply must carry.
    pub(crate) fn begin(
        &mut self,
        config: SpawnConfig,
        placement: Placement,
        target: SuggestTarget,
    ) -> (u64, String) {
        let generation = self.next_generation;
        self.next_generation += 1;
        let request_id = format!("launch-last-{generation}");
        self.pending.insert(
            target.clone(),
            Pending {
                config,
                placement,
                target,
                generation,
                request_id: request_id.clone(),
            },
        );
        (generation, request_id)
    }

    /// The wait for `target` whose request `request_id` names, taken; a reply
    /// for another target or another request, and one carrying no id, leave
    /// the waits in place.
    pub(crate) fn take_match(
        &mut self,
        target: &SuggestTarget,
        request_id: Option<&str>,
    ) -> Option<Pending> {
        let request_id = request_id?;
        if self.pending.get(target)?.request_id != request_id {
            return None;
        }
        self.pending.remove(target)
    }

    /// The wait whose request `request_id` names, taken; a later wait for the
    /// same target has an id of its own and stays.
    pub(crate) fn take_request(&mut self, request_id: &str) -> Option<Pending> {
        let target = self.pending.iter().find_map(|(target, pending)| {
            (pending.request_id == request_id).then(|| target.clone())
        })?;
        self.pending.remove(&target)
    }

    /// The wait `generation` names, taken; a timer of a replaced wait takes
    /// nothing, and every other target's wait stays.
    pub(crate) fn expire(&mut self, generation: u64) -> Option<Pending> {
        let target = self.pending.iter().find_map(|(target, pending)| {
            (pending.generation == generation).then(|| target.clone())
        })?;
        self.pending.remove(&target)
    }

    /// Notes a launch gesture for `target` at `now`; false when it repeats
    /// the previous gesture for the same target inside [`REPEAT_INTERVAL`],
    /// so one double-click or duplicated press launches once.
    pub(crate) fn note_activation(&mut self, target: &SuggestTarget, now: Instant) -> bool {
        if let Some((last, at)) = &self.last_activation
            && last == target
            && now.saturating_duration_since(*at) < REPEAT_INTERVAL
        {
            return false;
        }
        self.last_activation = Some((target.clone(), now));
        true
    }

    /// Drops the waits and the last gesture, as a new connection or a lost
    /// one does.
    pub(crate) fn clear(&mut self) {
        self.pending.clear();
        self.last_activation = None;
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::expect_used,
        clippy::panic,
        reason = "a test fails with the message of the precondition it lost"
    )]

    use super::*;
    use protocol::{AppearanceOverrides, SessionMode, WorktreeReusePolicy};

    fn single(repo_id: &str, use_worktree: bool) -> SpawnTarget {
        SpawnTarget::Single {
            repo_id: repo_id.to_owned(),
            branch_name: "feat/x".to_owned(),
            base_branch: Some("main".to_owned()),
            use_worktree,
            checkout_strategy: None,
            worktree_reuse: WorktreeReusePolicy::default(),
            existing_worktree: None,
        }
    }

    fn workspace_target(use_worktree: bool) -> SpawnTarget {
        SpawnTarget::Workspace {
            workspace_id: "w1".to_owned(),
            branch_name: "feat/x".to_owned(),
            base_branch: Some("main".to_owned()),
            use_worktree,
            worktree_reuse: WorktreeReusePolicy::default(),
            existing_worktrees: Vec::new(),
        }
    }

    fn config(target: SpawnTarget) -> SpawnConfig {
        SpawnConfig {
            target,
            mode: SessionMode::Interactive,
            dangerously_skip_permissions: false,
            agent: Some(crate::agent_options::test_selection("claude", &[])),
            extra_env: Vec::new(),
        }
    }

    fn repo_entry(id: &str, last: Option<SpawnConfig>) -> RepoEntry {
        RepoEntry {
            id: id.to_owned(),
            name: id.to_owned(),
            path: format!("D:/src/{id}"),
            default_branch: None,
            default_use_worktree: true,
            appearance: AppearanceOverrides::default(),
            last_agent: None,
            last_spawn_config: last,
        }
    }

    fn workspace_entry(id: &str, last: Option<SpawnConfig>) -> WorkspaceEntry {
        WorkspaceEntry {
            id: id.to_owned(),
            name: id.to_owned(),
            member_repo_ids: vec!["r1".to_owned()],
            linked_vscode_workspace: None,
            watch_vscode_workspace: false,
            default_use_worktree: true,
            appearance: AppearanceOverrides::default(),
            last_spawn_config: last,
        }
    }

    fn suggest_repo(id: &str) -> SuggestTarget {
        SuggestTarget::Repo {
            repo_id: id.to_owned(),
        }
    }

    #[test]
    fn a_worktree_target_asks_for_a_branch_and_an_in_place_one_does_not() {
        assert_eq!(
            suggestion_target(&config(single("r1", true))),
            Some(suggest_repo("r1"))
        );
        assert_eq!(suggestion_target(&config(single("r1", false))), None);
        assert_eq!(
            suggestion_target(&config(workspace_target(true))),
            Some(SuggestTarget::Workspace {
                workspace_id: "w1".to_owned()
            })
        );
        assert_eq!(suggestion_target(&config(workspace_target(false))), None);
    }

    #[test]
    fn stored_config_reads_the_repo_or_workspace_record() {
        let stored = config(single("r1", false));
        let repos = vec![repo_entry("r1", Some(stored.clone()))];
        let workspaces = vec![workspace_entry("w1", Some(stored.clone()))];
        assert_eq!(
            stored_config(Container::Repo("r1"), &repos, &workspaces),
            Some(stored.clone())
        );
        assert_eq!(
            stored_config(Container::Workspace("w1"), &repos, &workspaces),
            Some(stored)
        );
    }

    #[test]
    fn stored_config_is_none_without_a_launch_or_a_record() {
        let repos = vec![repo_entry("r1", None)];
        let workspaces = vec![workspace_entry("w1", None)];
        assert_eq!(
            stored_config(Container::Repo("r1"), &repos, &workspaces),
            None
        );
        assert_eq!(
            stored_config(Container::Repo("r9"), &repos, &workspaces),
            None
        );
        assert_eq!(
            stored_config(Container::Workspace("w1"), &repos, &workspaces),
            None
        );
    }

    #[test]
    fn waits_for_two_targets_both_survive() {
        let repo = config(single("r1", true));
        let workspace = config(workspace_target(true));
        let mut state = State::default();
        let (repo_generation, repo_request) =
            state.begin(repo.clone(), Placement::NewTab, suggest_repo("r1"));
        let (workspace_generation, _) = state.begin(
            workspace.clone(),
            Placement::NewTab,
            SuggestTarget::Workspace {
                workspace_id: "w1".to_owned(),
            },
        );
        assert_eq!(
            state
                .take_match(&suggest_repo("r1"), Some(&repo_request))
                .expect("the repo wait"),
            Pending {
                config: repo,
                placement: Placement::NewTab,
                target: suggest_repo("r1"),
                generation: repo_generation,
                request_id: repo_request,
            },
            "the repo's reply takes its own wait"
        );
        assert_eq!(
            state
                .expire(workspace_generation)
                .expect("the workspace wait")
                .config,
            workspace,
            "and the workspace's timer still reaches its wait"
        );
    }

    #[test]
    fn a_second_wait_for_one_target_replaces_only_that_target() {
        let first = config(single("r1", true));
        let second = config(single("r1", false));
        let workspace = config(workspace_target(true));
        let mut state = State::default();
        let (retired, _) = state.begin(first, Placement::NewTab, suggest_repo("r1"));
        let (_, workspace_request) = state.begin(
            workspace,
            Placement::NewTab,
            SuggestTarget::Workspace {
                workspace_id: "w1".to_owned(),
            },
        );
        let (current, _) = state.begin(second.clone(), Placement::NewTab, suggest_repo("r1"));

        assert!(
            state.expire(retired).is_none(),
            "the replaced wait's timer reaches nothing"
        );
        assert_eq!(
            state.expire(current).expect("the current wait").config,
            second
        );
        assert!(
            state
                .take_match(
                    &SuggestTarget::Workspace {
                        workspace_id: "w1".to_owned(),
                    },
                    Some(&workspace_request),
                )
                .is_some(),
            "the other target's wait is untouched"
        );
    }

    #[test]
    fn expiring_one_wait_leaves_the_other() {
        let mut state = State::default();
        let (repo_generation, _) = state.begin(
            config(single("r1", true)),
            Placement::NewTab,
            suggest_repo("r1"),
        );
        let (_, workspace_request) = state.begin(
            config(workspace_target(true)),
            Placement::NewTab,
            SuggestTarget::Workspace {
                workspace_id: "w1".to_owned(),
            },
        );
        assert!(
            state.expire(repo_generation).is_some(),
            "the timer's own wait"
        );
        assert!(
            state
                .take_match(
                    &SuggestTarget::Workspace {
                        workspace_id: "w1".to_owned(),
                    },
                    Some(&workspace_request),
                )
                .is_some(),
            "and only that one"
        );
    }

    #[test]
    fn a_repeat_activation_of_one_target_inside_the_interval_is_dropped() {
        let mut state = State::default();
        let target = suggest_repo("r1");
        let start = Instant::now();
        assert!(state.note_activation(&target, start));
        assert!(
            !state.note_activation(&target, start + Duration::from_millis(499)),
            "the second click of a gesture launches nothing"
        );
        assert!(
            state.note_activation(&target, start + REPEAT_INTERVAL),
            "past the interval a new gesture launches"
        );
    }

    #[test]
    fn another_target_or_a_new_connection_starts_over() {
        let mut state = State::default();
        let start = Instant::now();
        assert!(state.note_activation(&suggest_repo("r1"), start));
        assert!(
            state.note_activation(&suggest_repo("r2"), start + Duration::from_millis(10)),
            "another target is another gesture"
        );
        state.clear();
        assert!(
            state.note_activation(&suggest_repo("r1"), start + Duration::from_millis(20)),
            "a new connection forgets the last gesture"
        );
    }

    #[test]
    fn a_reply_for_another_target_leaves_the_wait_in_place() {
        let mut state = State::default();
        let (generation, request_id) = state.begin(
            config(single("r1", true)),
            Placement::NewTab,
            suggest_repo("r1"),
        );
        assert!(
            state
                .take_match(&suggest_repo("r9"), Some(&request_id))
                .is_none()
        );
        assert!(
            state.expire(generation).is_some(),
            "the wait was still armed"
        );
    }

    #[test]
    fn a_matching_reply_takes_the_wait_once() {
        let mut state = State::default();
        let config = config(single("r1", true));
        let (generation, request_id) = state.begin(
            config.clone(),
            Placement::Tab("t2".to_owned()),
            suggest_repo("r1"),
        );
        let pending = state
            .take_match(&suggest_repo("r1"), Some(&request_id))
            .expect("the wait");
        assert_eq!(pending.config, config);
        assert_eq!(pending.generation, generation);
        assert_eq!(pending.request_id, request_id);
        assert_eq!(pending.placement, Placement::Tab("t2".to_owned()));
        assert!(
            state
                .take_match(&suggest_repo("r1"), Some(&request_id))
                .is_none(),
            "once only"
        );
        assert!(
            state.expire(generation).is_none(),
            "and the timer finds nothing"
        );
    }

    #[test]
    fn a_reply_carrying_another_waits_id_leaves_the_wait_in_place() {
        let mut state = State::default();
        let (generation, request_id) = state.begin(
            config(single("r1", true)),
            Placement::NewTab,
            suggest_repo("r1"),
        );
        let (_, other_request) = state.begin(
            config(single("r2", true)),
            Placement::NewTab,
            suggest_repo("r2"),
        );
        assert!(
            state
                .take_match(&suggest_repo("r1"), Some(&other_request))
                .is_none(),
            "another wait's id finishes nothing"
        );
        assert_eq!(request_id, format!("launch-last-{generation}"));
        assert!(
            state.expire(generation).is_some(),
            "the wait was still armed"
        );
    }

    #[test]
    fn a_reply_carrying_its_own_id_takes_the_wait() {
        let mut state = State::default();
        let (_, request_id) = state.begin(
            config(single("r1", true)),
            Placement::NewTab,
            suggest_repo("r1"),
        );
        assert!(
            state.take_match(&suggest_repo("r1"), None).is_none(),
            "an id-less reply finishes nothing"
        );
        assert!(
            state
                .take_match(&suggest_repo("r1"), Some(&request_id))
                .is_some(),
            "its own id does"
        );
    }

    #[test]
    fn take_request_takes_only_the_wait_with_that_id() {
        let mut state = State::default();
        let (generation, request_id) = state.begin(
            config(single("r1", true)),
            Placement::NewTab,
            suggest_repo("r1"),
        );
        let (other_generation, _) = state.begin(
            config(workspace_target(true)),
            Placement::NewTab,
            SuggestTarget::Workspace {
                workspace_id: "w1".to_owned(),
            },
        );
        assert!(
            state.take_request("launch-last-999").is_none(),
            "an unknown id takes nothing"
        );
        assert_eq!(
            state
                .take_request(&request_id)
                .expect("the wait its id names")
                .generation,
            generation
        );
        assert!(
            state.take_request(&request_id).is_none(),
            "and takes it once"
        );
        assert!(
            state.expire(generation).is_none(),
            "its timer finds nothing"
        );
        assert!(
            state.expire(other_generation).is_some(),
            "the other wait is untouched"
        );
    }

    #[test]
    fn a_replay_without_a_fresh_branch_keeps_the_stored_target() {
        let stored = config(single("r1", false));
        assert_eq!(
            replay_request(&stored, None).target,
            stored.target,
            "the in-place target replays verbatim"
        );
    }

    #[test]
    fn a_replay_with_a_fresh_branch_refuses_leftovers() {
        let mut stored = config(single("r1", true));
        if let SpawnTarget::Single {
            worktree_reuse,
            existing_worktree,
            ..
        } = &mut stored.target
        {
            *worktree_reuse = WorktreeReusePolicy::Reuse;
            *existing_worktree = Some("D:/wt/old".to_owned());
        }
        let request = replay_request(&stored, Some("feat/fresh".to_owned()));
        let SpawnTarget::Single {
            branch_name,
            worktree_reuse,
            existing_worktree,
            ..
        } = &request.target
        else {
            panic!("expected a single-repo target");
        };
        assert_eq!(branch_name, "feat/fresh");
        assert_eq!(*worktree_reuse, WorktreeReusePolicy::RefuseLeftover);
        assert_eq!(*existing_worktree, None, "the leftover pin is cleared");
    }

    #[test]
    fn placement_maps_to_and_from_the_open_in_choice() {
        assert_eq!(Placement::NewTab.open_in(), OpenIn::NewTab);
        assert_eq!(
            Placement::Tab("t1".to_owned()).open_in(),
            OpenIn::Tab("t1".to_owned())
        );
        assert_eq!(
            Placement::from_open_in(OpenIn::CurrentTab("t1".to_owned())),
            Placement::Tab("t1".to_owned())
        );
        assert_eq!(Placement::from_open_in(OpenIn::NewTab), Placement::NewTab);
    }
}
