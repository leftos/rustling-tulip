//! The source-control model: the sections a focused session or the picked repo
//! shows, the status and stash stores keyed by (repo, worktree), the badge
//! total, the folded changes tree, and the collapse state `native-ui.json`
//! keeps.
//!
//! Plain Rust, so every rule is unit-tested; the panel view renders it and
//! requests the statuses [`ScModel::wanted_missing`] names.

use protocol::{DaemonMessage, GitFileChange, GitStash, RepoEntry, SessionMember};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

/// What a section reads: a registered repo, and the worktree under it the
/// status comes from (`None` is the repo's main tree).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScKey {
    pub repo_id: String,
    pub worktree: Option<String>,
}

impl ScKey {
    /// The key for a session member: the main tree when the member's worktree
    /// path is the path of the repo it names, compared with separators
    /// normalised, a trailing separator trimmed and ASCII case ignored; the
    /// member's worktree otherwise.
    #[must_use]
    pub fn for_member(member: &SessionMember, repos: &[RepoEntry]) -> Self {
        let main_tree = repos
            .iter()
            .find(|repo| repo.id == member.repo_id)
            .is_some_and(|repo| normalise(&repo.path) == normalise(&member.worktree_path));
        Self {
            repo_id: member.repo_id.clone(),
            worktree: if main_tree {
                None
            } else {
                Some(member.worktree_path.clone())
            },
        }
    }

    /// The key as a stable string, which the collapse state persists under:
    /// `<repo_id>::` for a main tree, `<repo_id>::<worktree>` for a worktree.
    #[must_use]
    pub fn id(&self) -> String {
        match &self.worktree {
            Some(worktree) => format!("{}::{worktree}", self.repo_id),
            None => format!("{}::", self.repo_id),
        }
    }
}

/// The path a key is compared by: forward slashes, no trailing separator, folded
/// to ASCII lowercase.
fn normalise(path: &str) -> String {
    path.replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

/// One section of the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub key: ScKey,
    pub repo_name: String,
    /// The member's branch; `None` for a repo's main tree.
    pub branch: Option<String>,
}

/// The sections the panel shows: one per focused session member, in member
/// order, or the pinned repo's main tree, or the first registered repo's.
///
/// A member whose repo is not registered has no section, and the second of two
/// members sharing a key is dropped. `pinned` applies only to the no-members
/// form.
#[must_use]
pub fn sections(
    repos: &[RepoEntry],
    focused_members: Option<&[SessionMember]>,
    pinned: Option<&str>,
) -> Vec<Section> {
    if let Some(members) = focused_members.filter(|members| !members.is_empty()) {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for member in members {
            if !repos.iter().any(|repo| repo.id == member.repo_id) {
                continue;
            }
            let key = ScKey::for_member(member, repos);
            if !seen.insert(key.id()) {
                continue;
            }
            out.push(Section {
                key,
                repo_name: member.repo_name.clone(),
                branch: Some(member.branch.clone()),
            });
        }
        return out;
    }
    pinned
        .and_then(|id| repos.iter().find(|repo| repo.id == id))
        .or_else(|| repos.first())
        .map_or_else(Vec::new, |repo| {
            vec![Section {
                key: ScKey {
                    repo_id: repo.id.clone(),
                    worktree: None,
                },
                repo_name: repo.name.clone(),
                branch: None,
            }]
        })
}

/// What the daemon last reported for one key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Index-vs-HEAD changes — the Staged bucket.
    pub staged: Vec<GitFileChange>,
    /// Worktree-vs-index changes — the Changes bucket, untracked files
    /// included with status `?`.
    pub changes: Vec<GitFileChange>,
}

/// Which of a section's two change buckets a folded tree row belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bucket {
    Staged,
    Changes,
}

/// A collapsible part of a section, as persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Part {
    Changes,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the panel draws no stash part, so none is built")
    )]
    Stashes,
    History,
}

impl Part {
    /// The suffix the part's collapse entry is stored under.
    fn suffix(self) -> &'static str {
        match self {
            Self::Changes => "changes",
            Self::Stashes => "stashes",
            Self::History => "history",
        }
    }
}

/// The prefix the daemon puts before a failed status read's message, which
/// the section's own wording replaces.
const STATUS_FAILED_PREFIX: &str = "status failed: ";

/// The prefix of every status request id this model hands out.
const STATUS_REQUEST_PREFIX: &str = "sc-status-";

/// The status and stash stores, the statuses asked for and not yet
/// answered, the ones whose request failed, and the in-memory folder
/// collapse.
#[derive(Debug, Default)]
pub struct ScModel {
    status: HashMap<ScKey, Status>,
    stashes: HashMap<ScKey, Vec<GitStash>>,
    requested: HashSet<ScKey>,
    /// The status requests handed out, by request id.
    request_ids: HashMap<String, ScKey>,
    /// The number the last request id carried.
    last_request: u64,
    /// The keys whose last status request failed, with the daemon's reason.
    failed: HashMap<ScKey, String>,
    collapsed_folders: HashSet<(String, Bucket, String)>,
}

impl ScModel {
    /// Fold a daemon message into the stores; `true` when one of them changed.
    /// A new connection also forgets every request and failure, and a status
    /// clears its key's request and failure; a request alone does not count
    /// as a change, since the view does not show it.
    pub fn apply(&mut self, msg: &DaemonMessage) -> bool {
        match msg {
            DaemonMessage::Welcome { .. } => {
                let changed =
                    !self.status.is_empty() || !self.stashes.is_empty() || !self.failed.is_empty();
                self.status.clear();
                self.stashes.clear();
                self.requested.clear();
                self.request_ids.clear();
                self.failed.clear();
                changed
            }
            DaemonMessage::RepoStatus {
                repo_id,
                index_changes,
                worktree_changes,
                worktree_path,
            } => {
                let key = ScKey {
                    repo_id: repo_id.clone(),
                    worktree: worktree_path.clone(),
                };
                let status = Status {
                    staged: index_changes.clone(),
                    changes: worktree_changes.clone(),
                };
                self.requested.remove(&key);
                self.request_ids.retain(|_, requested| *requested != key);
                let recovered = self.failed.remove(&key).is_some();
                let changed = self.status.get(&key) != Some(&status);
                self.status.insert(key, status);
                changed || recovered
            }
            DaemonMessage::Stashes {
                repo_id,
                stashes,
                worktree_path,
            } => {
                let key = ScKey {
                    repo_id: repo_id.clone(),
                    worktree: worktree_path.clone(),
                };
                let changed = self.stashes.get(&key) != Some(stashes);
                self.stashes.insert(key, stashes.clone());
                changed
            }
            // A registry snapshot that no longer lists a repo is the only
            // removal signal the protocol carries, so its keys go with it.
            DaemonMessage::Repos { repos } => self.retain_repos(repos),
            _ => false,
        }
    }

    /// Drop every key whose repo is not in `repos`; `true` when any went.
    fn retain_repos(&mut self, repos: &[RepoEntry]) -> bool {
        let ids: HashSet<&str> = repos.iter().map(|repo| repo.id.as_str()).collect();
        let before = self.status.len() + self.stashes.len() + self.failed.len();
        self.status
            .retain(|key, _| ids.contains(key.repo_id.as_str()));
        self.stashes
            .retain(|key, _| ids.contains(key.repo_id.as_str()));
        self.requested
            .retain(|key| ids.contains(key.repo_id.as_str()));
        self.request_ids
            .retain(|_, key| ids.contains(key.repo_id.as_str()));
        self.failed
            .retain(|key, _| ids.contains(key.repo_id.as_str()));
        before != self.status.len() + self.stashes.len() + self.failed.len()
    }

    /// Record that a status for `key` is being asked for, so it is not asked
    /// for again until it arrives, fails or the connection is replaced, and
    /// hand out the request id to send with it: `sc-status-<n>`, `n`
    /// counting up from 1. The key's earlier id, if any, is forgotten, so
    /// only its latest request can mark it failed.
    pub fn request(&mut self, key: ScKey) -> String {
        self.last_request += 1;
        let id = format!("{STATUS_REQUEST_PREFIX}{}", self.last_request);
        self.request_ids.retain(|_, requested| *requested != key);
        self.requested.insert(key.clone());
        self.request_ids.insert(id.clone(), key);
        id
    }

    /// A daemon `Error` answering `request_id`: when the id is this model's
    /// outstanding request for a key, the key is marked failed with
    /// `message` (less the daemon's `status failed: ` prefix) and is no
    /// longer asked for. Any other `sc-status-` id is a superseded or
    /// already answered request of this model's, and is dropped. Returns
    /// whether the id was this model's.
    pub fn fail_request(&mut self, request_id: &str, message: &str) -> bool {
        let Some(key) = self.request_ids.remove(request_id) else {
            let ours = request_id.starts_with(STATUS_REQUEST_PREFIX);
            if ours {
                tracing::debug!(
                    request_id,
                    message,
                    "dropping an error for a stale status request"
                );
            }
            return ours;
        };
        self.requested.remove(&key);
        let reason = message
            .strip_prefix(STATUS_FAILED_PREFIX)
            .unwrap_or(message);
        self.failed.insert(key, reason.to_owned());
        true
    }

    /// Why the last status request for `key` failed, until a status arrives.
    #[must_use]
    pub fn failure(&self, key: &ScKey) -> Option<&str> {
        self.failed.get(key).map(String::as_str)
    }

    /// The keys of `wanted` with no status yet, no request out and no failed
    /// request, in order and each once, for the view to request. A failed
    /// key waits for Refresh.
    #[must_use]
    pub fn wanted_missing(&self, wanted: &[ScKey]) -> Vec<ScKey> {
        let mut seen = HashSet::new();
        wanted
            .iter()
            .filter(|key| {
                !self.status.contains_key(key)
                    && !self.requested.contains(key)
                    && !self.failed.contains_key(key)
            })
            .filter(|key| seen.insert(*key))
            .cloned()
            .collect()
    }

    /// The status of a key, when one has arrived.
    #[must_use]
    pub fn status(&self, key: &ScKey) -> Option<&Status> {
        self.status.get(key)
    }

    /// The stashes of a key, when a list has arrived.
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the panel draws no stash part to read it")
    )]
    pub fn stashes(&self, key: &ScKey) -> Option<&[GitStash]> {
        self.stashes.get(key).map(Vec::as_slice)
    }

    /// Whether a status has arrived for a key.
    #[must_use]
    pub fn is_loaded(&self, key: &ScKey) -> bool {
        self.status.contains_key(key)
    }

    /// The badge total over `keys`: the distinct file paths across both
    /// buckets, so a file both staged and modified counts once and untracked
    /// files count.
    #[must_use]
    pub fn badge_total(&self, keys: &[ScKey]) -> usize {
        keys.iter()
            .filter_map(|key| self.status.get(key))
            .map(|status| {
                status
                    .staged
                    .iter()
                    .chain(&status.changes)
                    .map(|change| change.path.as_str())
                    .collect::<HashSet<_>>()
                    .len()
            })
            .sum()
    }

    /// Flip whether `full_path` in `bucket` is collapsed.
    pub fn toggle_folder(&mut self, key: &ScKey, bucket: Bucket, full_path: &str) {
        let entry = (key.id(), bucket, full_path.to_owned());
        if !self.collapsed_folders.remove(&entry) {
            self.collapsed_folders.insert(entry);
        }
    }

    /// Whether `full_path` in `bucket` is collapsed.
    #[must_use]
    pub fn is_folder_collapsed(&self, key: &ScKey, bucket: Bucket, full_path: &str) -> bool {
        self.collapsed_folders
            .contains(&(key.id(), bucket, full_path.to_owned()))
    }
}

/// One row of the folded changes tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// The display label — the deepest segment plus any folded ancestors, so
    /// `src/a/b/c` is one row.
    pub label: String,
    /// The repo-relative path of the deepest folder the row stands for, which
    /// keys the row's collapse state.
    pub full_path: String,
    pub folders: Vec<Folder>,
    pub files: Vec<GitFileChange>,
}

/// An unfolded folder while the tree is built.
struct MutFolder {
    name: String,
    full_path: String,
    children: BTreeMap<String, Self>,
    files: Vec<GitFileChange>,
}

impl MutFolder {
    /// The child folder named `name`, created with its path if new.
    fn child(&mut self, name: &str) -> &mut Self {
        let full_path = if self.full_path.is_empty() {
            name.to_owned()
        } else {
            format!("{}/{name}", self.full_path)
        };
        self.children
            .entry(name.to_owned())
            .or_insert_with(|| Self {
                name: name.to_owned(),
                full_path,
                children: BTreeMap::new(),
                files: Vec::new(),
            })
    }

    /// Fold runs of single-child folders into one row, then sort.
    fn collapse(node: Self) -> Folder {
        let Self {
            mut name,
            mut full_path,
            mut children,
            mut files,
        } = node;
        loop {
            // The root (empty `full_path`) never folds: its children are the
            // rows the view spreads out.
            if !files.is_empty() || children.len() != 1 || full_path.is_empty() {
                break;
            }
            let Some((_, child)) = children.pop_first() else {
                break;
            };
            let Self {
                name: child_name,
                full_path: child_path,
                children: child_children,
                files: child_files,
            } = child;
            name = format!("{name}/{child_name}");
            full_path = child_path;
            children = child_children;
            files = child_files;
        }
        let mut folders: Vec<Folder> = children.into_values().map(Self::collapse).collect();
        folders.sort_by(|a, b| label_order(&a.label, &b.label));
        files.sort_by(|a, b| label_order(&a.path, &b.path));
        Folder {
            label: name,
            full_path,
            folders,
            files,
        }
    }
}

/// Compare folder labels and file paths case-insensitively, a byte-order tie
/// keeping the order total.
fn label_order(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

/// The folded tree for a flat change list.
///
/// Each path is split on `/` and `\` and its file lands in the deepest folder
/// above it; a path with no folder above it lands in the root, whose label and
/// path are empty. Runs of folders with no files and one child merge into a
/// single row (`a/b/c/x.rs` becomes the row `a/b/c`).
#[must_use]
pub fn build_tree(changes: &[GitFileChange]) -> Folder {
    let mut root = MutFolder {
        name: String::new(),
        full_path: String::new(),
        children: BTreeMap::new(),
        files: Vec::new(),
    };
    for change in changes {
        let segments: Vec<&str> = change
            .path
            .split(['/', '\\'])
            .filter(|segment| !segment.is_empty())
            .collect();
        match segments.split_last() {
            Some((_, folders)) if !folders.is_empty() => {
                let mut cursor = &mut root;
                for folder in folders.iter().copied() {
                    cursor = cursor.child(folder);
                }
                cursor.files.push(change.clone());
            }
            _ => root.files.push(change.clone()),
        }
    }
    MutFolder::collapse(root)
}

/// The source-control state `native-ui.json` keeps.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScUiState {
    /// The repo the panel is pinned to; `None` follows the active pane.
    pub pinned_repo: Option<String>,
    /// The collapsed parts, keyed `<section key id>|<part>`.
    pub collapsed: BTreeMap<String, bool>,
    /// The changes area's height above the History, in pixels; `None` is
    /// the default.
    #[serde(default)]
    pub changes_height: Option<f32>,
    /// Each section's commit-list height above its detail pane, in pixels,
    /// by section key id.
    #[serde(default)]
    pub history_list_height: BTreeMap<String, f32>,
}

impl ScUiState {
    /// Whether a part of a section is collapsed. A stored value wins; the
    /// defaults are Changes collapsed only when loaded and empty, Stashes
    /// collapsed, and History as [`Self::is_history_collapsed`] answers with
    /// no session focused.
    #[must_use]
    pub fn is_collapsed(&self, key: &ScKey, part: Part, loaded_count: Option<usize>) -> bool {
        if let Some(stored) = self.collapsed.get(&part_key(key, part)) {
            return *stored;
        }
        match part {
            Part::Changes => loaded_count == Some(0),
            Part::Stashes => true,
            Part::History => self.is_history_collapsed(key, false),
        }
    }

    /// Whether a section's History is collapsed. A stored value wins;
    /// otherwise it is collapsed for a focused session's members
    /// (`session_focused`) and expanded for a repo browsed with no session
    /// focused.
    #[must_use]
    pub fn is_history_collapsed(&self, key: &ScKey, session_focused: bool) -> bool {
        self.collapsed
            .get(&part_key(key, Part::History))
            .copied()
            .unwrap_or(session_focused)
    }

    /// Record whether a part of a section is collapsed; `true` when the
    /// stored value changed.
    pub fn set_collapsed(&mut self, key: &ScKey, part: Part, collapsed: bool) -> bool {
        self.collapsed.insert(part_key(key, part), collapsed) != Some(collapsed)
    }

    /// Drop the collapse entries, the list heights and the pin of repos no
    /// longer registered; `true` when any went.
    pub fn prune(&mut self, repos: &[RepoEntry]) -> bool {
        let ids: HashSet<&str> = repos.iter().map(|repo| repo.id.as_str()).collect();
        let registered = |entry: &str| entry.split("::").next().is_some_and(|id| ids.contains(id));
        let before = self.collapsed.len() + self.history_list_height.len();
        self.collapsed.retain(|entry, _| registered(entry));
        self.history_list_height
            .retain(|entry, _| registered(entry));
        let mut changed = self.collapsed.len() + self.history_list_height.len() != before;
        if self
            .pinned_repo
            .as_deref()
            .is_some_and(|pinned| !ids.contains(pinned))
        {
            self.pinned_repo = None;
            changed = true;
        }
        changed
    }
}

/// The key a part's collapse state is stored under.
fn part_key(key: &ScKey, part: Part) -> String {
    format!("{}|{}", key.id(), part.suffix())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::sidebar::UiState;
    use serde_json::json;

    fn repo(id: &str, path: &str) -> RepoEntry {
        serde_json::from_value(json!({ "id": id, "name": id, "path": path })).expect("repo fixture")
    }

    fn member(repo_id: &str, worktree_path: &str) -> SessionMember {
        serde_json::from_value(json!({
            "repo_id": repo_id,
            "repo_name": repo_id,
            "branch": "main",
            "worktree_path": worktree_path,
        }))
        .expect("member fixture")
    }

    fn change(path: &str, status: &str) -> GitFileChange {
        serde_json::from_value(json!({ "path": path, "status": status, "from_path": null }))
            .expect("change fixture")
    }

    fn changes(paths: &[&str]) -> Vec<GitFileChange> {
        paths.iter().map(|path| change(path, "M")).collect()
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "the fixtures read plainly at their call sites, and json! only borrows"
    )]
    fn status_message(
        repo_id: &str,
        worktree: Option<&str>,
        staged: Vec<GitFileChange>,
        unstaged: Vec<GitFileChange>,
    ) -> DaemonMessage {
        serde_json::from_value(json!({
            "type": "repo_status",
            "repo_id": repo_id,
            "index_changes": staged,
            "worktree_changes": unstaged,
            "worktree_path": worktree,
        }))
        .expect("status fixture")
    }

    fn stashes_message(repo_id: &str, worktree: Option<&str>, count: usize) -> DaemonMessage {
        let stashes: Vec<serde_json::Value> = (0..count)
            .map(|i| json!({ "id": format!("stash@{i}"), "subject": "WIP", "created_at": "2026-01-01T00:00:00Z" }))
            .collect();
        serde_json::from_value(json!({
            "type": "stashes",
            "repo_id": repo_id,
            "stashes": stashes,
            "worktree_path": worktree,
        }))
        .expect("stashes fixture")
    }

    fn main_tree(repo_id: &str) -> ScKey {
        ScKey {
            repo_id: repo_id.to_owned(),
            worktree: None,
        }
    }

    fn worktree(repo_id: &str, path: &str) -> ScKey {
        ScKey {
            repo_id: repo_id.to_owned(),
            worktree: Some(path.to_owned()),
        }
    }

    fn labels(folders: &[Folder]) -> Vec<&str> {
        folders.iter().map(|folder| folder.label.as_str()).collect()
    }

    #[test]
    fn member_key_matches_its_registered_repo_path() {
        let repos = vec![repo("r1", "D:\\repos\\r1")];
        for path in ["D:\\repos\\r1", "D:/repos/r1", "d:\\repos\\R1\\"] {
            let key = ScKey::for_member(&member("r1", path), &repos);
            assert_eq!(key.worktree, None, "{path} is the main tree");
            assert_eq!(key.id(), "r1::");
        }

        let key = ScKey::for_member(&member("r1", "D:\\repos\\r1-wt"), &repos);
        assert_eq!(key.worktree.as_deref(), Some("D:\\repos\\r1-wt"));
        assert_eq!(key.id(), "r1::D:\\repos\\r1-wt");

        let unknown = ScKey::for_member(&member("r9", "D:\\repos\\r9"), &repos);
        assert_eq!(
            unknown.worktree.as_deref(),
            Some("D:\\repos\\r9"),
            "an unregistered repo has no main tree to match"
        );
    }

    #[test]
    fn sections_follow_members_and_drop_unknown_and_duplicate_keys() {
        let repos = vec![repo("r1", "D:\\r1"), repo("r2", "D:\\r2")];
        let members = vec![
            member("r1", "D:\\r1"),
            member("ghost", "D:\\ghost"),
            member("r2", "D:\\r2-wt"),
            member("r2", "D:\\r2-wt"),
        ];
        let sections = sections(&repos, Some(&members), Some("r2"));
        let keys: Vec<String> = sections.iter().map(|section| section.key.id()).collect();
        assert!(
            !keys.iter().any(|key| key == "r2::"),
            "the pin names a registered repo that is not a member, so members win"
        );
        assert_eq!(
            keys,
            ["r1::", "r2::D:\\r2-wt"],
            "member order, the unregistered member skipped and the duplicate dropped"
        );
        assert!(
            sections
                .iter()
                .all(|section| section.branch.as_deref() == Some("main")),
            "a member section carries its branch"
        );
        assert_eq!(sections[0].repo_name, "r1");
    }

    #[test]
    fn sections_without_members_use_the_pin_and_fall_back_to_the_first_repo() {
        let repos = vec![repo("r1", "D:\\r1"), repo("r2", "D:\\r2")];
        let pinned = sections(&repos, None, Some("r2"));
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0].key.id(), "r2::");
        assert_eq!(pinned[0].repo_name, "r2");
        assert!(pinned[0].branch.is_none(), "a main tree has no branch");

        let stale = sections(&repos, None, Some("gone"));
        assert_eq!(stale[0].key.id(), "r1::", "a stale pin falls back");

        let empty_roster = sections(&repos, Some(&[]), Some("r2"));
        assert_eq!(
            empty_roster[0].key.id(),
            "r2::",
            "no members is the pinned path"
        );

        assert!(sections(&[], None, None).is_empty());
        assert!(sections(&[], None, Some("r1")).is_empty());
    }

    #[test]
    fn status_and_stashes_are_stored_per_repo_and_worktree() {
        let mut model = ScModel::default();
        assert!(
            !model.apply(&DaemonMessage::Sessions {
                sessions: Vec::new()
            }),
            "an untracked message changes nothing"
        );

        let main = main_tree("r1");
        let worktree_key = worktree("r1", "D:\\r1-wt");
        assert!(model.apply(&status_message("r1", None, changes(&["a.rs"]), Vec::new())));
        assert!(model.apply(&status_message(
            "r1",
            Some("D:\\r1-wt"),
            Vec::new(),
            changes(&["b.rs"])
        )));
        assert!(model.is_loaded(&main) && model.is_loaded(&worktree_key));
        assert_eq!(
            model.status(&main).expect("main status").staged.len(),
            1,
            "the worktree status did not overwrite the main tree's"
        );
        assert_eq!(
            model
                .status(&worktree_key)
                .expect("worktree status")
                .changes[0]
                .path,
            "b.rs"
        );
        assert!(
            !model.apply(&status_message("r1", None, changes(&["a.rs"]), Vec::new())),
            "an identical broadcast is no change"
        );

        assert!(model.apply(&stashes_message("r1", None, 2)));
        assert!(model.apply(&stashes_message("r1", Some("D:\\r1-wt"), 0)));
        assert_eq!(model.stashes(&main).expect("main stashes").len(), 2);
        assert_eq!(
            model
                .stashes(&worktree_key)
                .expect("worktree stashes")
                .len(),
            0
        );
        assert!(model.stashes(&main_tree("r9")).is_none());
    }

    #[test]
    fn welcome_clears_and_a_registry_snapshot_drops_unregistered_keys() {
        let mut model = ScModel::default();
        model.apply(&status_message("r1", None, changes(&["a.rs"]), Vec::new()));
        model.apply(&status_message("r2", None, Vec::new(), changes(&["b.rs"])));
        model.apply(&stashes_message("r1", None, 1));
        assert!(
            model.apply(&DaemonMessage::Welcome {
                protocol_version: 22,
                supported_versions: vec![22],
            }),
            "clearing a populated store is a change"
        );
        assert!(!model.is_loaded(&main_tree("r1")));
        assert!(model.stashes(&main_tree("r1")).is_none());
        assert!(
            !model.apply(&DaemonMessage::Welcome {
                protocol_version: 22,
                supported_versions: vec![22],
            }),
            "clearing an empty store is no change"
        );

        model.apply(&status_message("r1", None, Vec::new(), changes(&["a.rs"])));
        model.apply(&status_message("r2", None, Vec::new(), Vec::new()));
        assert!(
            model.apply(&DaemonMessage::Repos {
                repos: vec![repo("r2", "D:\\r2")],
            }),
            "r1 is gone, so its keys go with it"
        );
        assert!(!model.is_loaded(&main_tree("r1")));
        assert!(model.is_loaded(&main_tree("r2")));
        assert!(
            !model.apply(&DaemonMessage::Repos {
                repos: vec![repo("r2", "D:\\r2")],
            }),
            "a snapshot that drops nothing changes nothing"
        );
    }

    #[test]
    fn wanted_missing_skips_loaded_and_requested_keys() {
        let mut model = ScModel::default();
        let wanted = vec![
            main_tree("r1"),
            main_tree("r2"),
            worktree("r1", "D:\\r1-wt"),
            main_tree("r1"),
        ];
        assert_eq!(
            model.wanted_missing(&wanted),
            [
                main_tree("r1"),
                main_tree("r2"),
                worktree("r1", "D:\\r1-wt")
            ],
            "in order, each key once"
        );

        model.apply(&status_message("r1", None, Vec::new(), Vec::new()));
        model.request(main_tree("r2"));
        assert_eq!(
            model.wanted_missing(&wanted),
            [worktree("r1", "D:\\r1-wt")],
            "r1's main tree is loaded and r2's is asked for"
        );
    }

    #[test]
    fn a_status_or_a_welcome_clears_requests() {
        let mut model = ScModel::default();
        let wanted = vec![main_tree("r1"), worktree("r1", "D:\\r1-wt")];
        model.request(main_tree("r1"));
        model.request(worktree("r1", "D:\\r1-wt"));
        assert!(model.wanted_missing(&wanted).is_empty());

        model.apply(&status_message("r1", None, Vec::new(), Vec::new()));
        model.apply(&DaemonMessage::Welcome {
            protocol_version: 22,
            supported_versions: vec![22],
        });
        assert_eq!(
            model.wanted_missing(&wanted),
            wanted,
            "the welcome dropped the status and every request"
        );

        model.request(main_tree("r1"));
        model.apply(&status_message("r1", None, Vec::new(), Vec::new()));
        model.apply(&DaemonMessage::Repos {
            repos: vec![repo("r1", "D:\\r1")],
        });
        model.apply(&stashes_message("r1", None, 0));
        assert!(
            !model.requested.contains(&main_tree("r1")),
            "its status arrived, so the request is answered"
        );
    }

    #[test]
    fn a_failed_status_request_marks_its_key_until_a_status_arrives() {
        let mut model = ScModel::default();
        let main = main_tree("r1");
        let worktree_key = worktree("r1", "D:\\r1-wt");
        let first = model.request(main.clone());
        let second = model.request(worktree_key.clone());
        assert_eq!(
            (first.as_str(), second.as_str()),
            ("sc-status-1", "sc-status-2")
        );

        assert!(
            !model.fail_request("spawn-7", "spawn failed"),
            "an id it never handed out is not its"
        );
        assert!(model.fail_request(&first, "status failed: not a git repository"));
        assert_eq!(model.failure(&main), Some("not a git repository"));
        assert_eq!(model.failure(&worktree_key), None);
        assert!(
            model.fail_request(&first, "status failed: again"),
            "an answered id is still this model's, so no toast shows it"
        );
        assert_eq!(
            model.failure(&main),
            Some("not a git repository"),
            "an answered id records no second failure"
        );
        assert!(
            model.wanted_missing(std::slice::from_ref(&main)).is_empty(),
            "a failed key is not asked for again by itself"
        );

        assert_eq!(
            model.request(main.clone()),
            "sc-status-3",
            "a retry takes a new id"
        );
        assert!(
            model.failure(&main).is_some(),
            "the failure stays until the answer"
        );
        assert!(model.apply(&status_message("r1", None, Vec::new(), Vec::new())));
        assert_eq!(model.failure(&main), None, "a status clears it");
        assert!(model.is_loaded(&main));
    }

    #[test]
    fn a_welcome_forgets_request_ids_and_failures() {
        let mut model = ScModel::default();
        let main = main_tree("r1");
        let failed = model.request(main.clone());
        model.fail_request(&failed, "status failed: boom");
        let out = model.request(worktree("r1", "D:\\r1-wt"));
        assert!(
            model.apply(&DaemonMessage::Welcome {
                protocol_version: 22,
                supported_versions: vec![22],
            }),
            "dropping a failure is a change"
        );
        assert_eq!(model.failure(&main), None);
        assert!(
            model.fail_request(&out, "status failed: late"),
            "an old connection's id is still this model's, so no toast shows it"
        );
        assert_eq!(
            model.failure(&worktree("r1", "D:\\r1-wt")),
            None,
            "the old connection's id records no failure"
        );
        assert_eq!(
            model.wanted_missing(std::slice::from_ref(&main)),
            [main],
            "asked for again on the new connection"
        );
    }

    #[test]
    fn request_ids_hold_one_entry_per_key_until_its_status_arrives() {
        let mut model = ScModel::default();
        let main = main_tree("r1");
        let superseded = model.request(main.clone());
        let current = model.request(main.clone());
        model.request(worktree("r1", "D:\\r1-wt"));
        assert_eq!(model.request_ids.len(), 2, "one id per key");
        assert!(
            model.fail_request(&superseded, "status failed: stale"),
            "a superseded id is still this model's"
        );
        assert_eq!(
            model.failure(&main),
            None,
            "a superseded id records no failure"
        );

        model.apply(&status_message("r1", None, Vec::new(), Vec::new()));
        assert_eq!(
            model.request_ids.len(),
            1,
            "the status dropped its key's id"
        );
        assert!(
            model.fail_request(&current, "status failed: late"),
            "an answered id is still this model's"
        );
        assert_eq!(model.failure(&main), None, "and records no failure");
    }

    #[test]
    fn a_status_clears_a_failure_even_with_the_same_contents() {
        let mut model = ScModel::default();
        let main = main_tree("r1");
        model.apply(&status_message("r1", None, Vec::new(), Vec::new()));
        let id = model.request(main.clone());
        model.fail_request(&id, "status failed: boom");
        assert!(
            model.apply(&status_message("r1", None, Vec::new(), Vec::new())),
            "an identical status still clears the failure, which is a change"
        );
        assert_eq!(model.failure(&main), None);
    }

    #[test]
    fn set_collapsed_reports_whether_the_stored_value_moved() {
        let mut state = ScUiState::default();
        let key = main_tree("r1");
        assert!(
            state.set_collapsed(&key, Part::Changes, false),
            "a first value is stored"
        );
        assert!(
            !state.set_collapsed(&key, Part::Changes, false),
            "the same value again"
        );
        assert!(state.set_collapsed(&key, Part::Changes, true));
        assert!(state.is_collapsed(&key, Part::Changes, Some(3)));
    }

    #[test]
    fn badge_counts_distinct_paths_over_the_given_keys() {
        let mut model = ScModel::default();
        let main = main_tree("r1");
        let worktree_key = worktree("r1", "D:\\r1-wt");
        model.apply(&status_message(
            "r1",
            None,
            changes(&["a.rs", "b.rs"]),
            vec![change("b.rs", "M"), change("c.rs", "?")],
        ));
        model.apply(&status_message(
            "r1",
            Some("D:\\r1-wt"),
            Vec::new(),
            vec![change("a.rs", "?")],
        ));

        assert_eq!(
            model.badge_total(std::slice::from_ref(&main)),
            3,
            "b.rs is staged and modified, and counts once"
        );
        assert_eq!(model.badge_total(&[main, worktree_key]), 4);
        assert_eq!(model.badge_total(&[main_tree("r9")]), 0);
        assert_eq!(model.badge_total(&[]), 0);
    }

    #[test]
    fn folder_collapse_is_per_key_bucket_and_path() {
        let mut model = ScModel::default();
        let main = main_tree("r1");
        let worktree_key = worktree("r1", "D:\\r1-wt");
        assert!(!model.is_folder_collapsed(&main, Bucket::Changes, "src"));

        model.toggle_folder(&main, Bucket::Changes, "src");
        assert!(model.is_folder_collapsed(&main, Bucket::Changes, "src"));
        assert!(!model.is_folder_collapsed(&main, Bucket::Staged, "src"));
        assert!(!model.is_folder_collapsed(&worktree_key, Bucket::Changes, "src"));
        assert!(!model.is_folder_collapsed(&main, Bucket::Changes, "src/a"));

        model.toggle_folder(&main, Bucket::Changes, "src");
        assert!(!model.is_folder_collapsed(&main, Bucket::Changes, "src"));
    }

    #[test]
    fn tree_folds_single_child_chains_below_a_stable_root() {
        let tree = build_tree(&changes(&["src/a/b/c/x.rs"]));
        assert_eq!(tree.label, "", "the root never folds");
        assert_eq!(tree.full_path, "");
        assert_eq!(labels(&tree.folders), ["src/a/b/c"]);
        assert_eq!(tree.folders[0].files.len(), 1);
        assert_eq!(tree.folders[0].files[0].path, "src/a/b/c/x.rs");
        assert_eq!(
            tree.folders[0].full_path, "src/a/b/c",
            "the row keys its collapse state by the deepest folder"
        );
    }

    #[test]
    fn tree_stops_folding_where_a_folder_holds_a_file() {
        let tree = build_tree(&changes(&["a/x.rs", "a/b/y.rs"]));
        assert_eq!(labels(&tree.folders), ["a"]);
        assert_eq!(tree.folders[0].files.len(), 1);
        assert_eq!(labels(&tree.folders[0].folders), ["b"]);
        assert_eq!(tree.folders[0].folders[0].full_path, "a/b");
    }

    #[test]
    fn tree_splits_both_separators_and_keeps_root_files_in_the_root() {
        let tree = build_tree(&changes(&["src\\lib\\mod.rs", "README.md"]));
        assert_eq!(labels(&tree.folders), ["src/lib"]);
        assert_eq!(tree.folders[0].files[0].path, "src\\lib\\mod.rs");
        let files: Vec<&str> = tree.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            files,
            ["README.md"],
            "a path with no folder lands in the root"
        );
    }

    #[test]
    fn tree_sorts_case_insensitively() {
        let tree = build_tree(&changes(&[
            "C/z.rs", "b/y.rs", "A/x.rs", "z.rs", "B.rs", "a.rs",
        ]));
        assert_eq!(labels(&tree.folders), ["A", "b", "C"]);
        let files: Vec<&str> = tree.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(files, ["a.rs", "B.rs", "z.rs"]);
    }

    #[test]
    fn collapse_defaults_follow_the_part_and_the_loaded_count() {
        let state = ScUiState::default();
        let key = main_tree("r1");
        assert!(!state.is_collapsed(&key, Part::Changes, Some(2)));
        assert!(state.is_collapsed(&key, Part::Changes, Some(0)));
        assert!(
            !state.is_collapsed(&key, Part::Changes, None),
            "not loaded is not collapsed"
        );
        assert!(state.is_collapsed(&key, Part::Stashes, None));
        assert!(!state.is_collapsed(&key, Part::History, None));
        assert!(
            state.is_collapsed(&worktree("r1", "D:\\r1-wt"), Part::Stashes, None),
            "the default does not depend on the key"
        );
    }

    #[test]
    fn a_stored_collapse_wins_and_prune_drops_unregistered_repos() {
        let mut state = ScUiState::default();
        let r1 = main_tree("r1");
        let r2 = worktree("r2", "D:\\r2-wt");
        state.set_collapsed(&r1, Part::Changes, true);
        state.set_collapsed(&r2, Part::History, true);
        state.pinned_repo = Some("r2".to_owned());
        assert!(
            state.is_collapsed(&r1, Part::Changes, Some(4)),
            "a stored value beats the default"
        );
        assert_eq!(state.collapsed.get("r2::D:\\r2-wt|history"), Some(&true));

        state.set_collapsed(&r1, Part::Stashes, false);
        assert!(
            !state.is_collapsed(&r1, Part::Stashes, None),
            "a stored false beats the collapsed default"
        );

        assert!(state.prune(&[repo("r1", "D:\\r1")]), "r2's entries went");
        assert!(
            state.is_collapsed(&r1, Part::Changes, Some(4)),
            "a registered repo stays"
        );
        assert!(!state.collapsed.contains_key("r2::D:\\r2-wt|history"));
        assert_eq!(state.pinned_repo, None);

        let mut kept = ScUiState {
            pinned_repo: Some("r2".to_owned()),
            ..ScUiState::default()
        };
        assert!(
            !kept.prune(&[repo("r1", "D:\\r1"), repo("r2", "D:\\r2")]),
            "nothing to drop"
        );
        assert_eq!(kept.pinned_repo.as_deref(), Some("r2"));

        let mut pin_only = ScUiState {
            pinned_repo: Some("r2".to_owned()),
            ..ScUiState::default()
        };
        assert!(
            pin_only.prune(&[repo("r1", "D:\\r1")]),
            "the pin alone counts"
        );
        assert_eq!(pin_only.pinned_repo, None);
    }

    #[test]
    fn ui_state_round_trips_with_and_without_source_control() {
        let mut state = UiState::default();
        assert_eq!(state.source_control, ScUiState::default());
        state.source_control.pinned_repo = Some("r1".to_owned());
        state
            .source_control
            .set_collapsed(&main_tree("r1"), Part::Stashes, false);
        let json = serde_json::to_string(&state).expect("serializing the layout");
        assert_eq!(
            serde_json::from_str::<UiState>(&json).expect("reading it back"),
            state
        );

        let old = serde_json::from_str::<UiState>(r#"{"sidebar_width": 300.0}"#)
            .expect("an old file without the field loads");
        assert_eq!(old.source_control, ScUiState::default());
        assert!((old.sidebar_width - 300.0).abs() < f32::EPSILON);
    }
}
