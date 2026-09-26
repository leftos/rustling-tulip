//! The stash lists, one per repo since git keeps a single list for each
//! repository, the list requests out and the ones that failed, the stash
//! operation each repo has out, and the confirm a drop opens.
//!
//! Plain Rust, so every rule is unit-tested; the stash view renders it.

use protocol::{DaemonMessage, GitStash, RepoEntry};
use std::collections::{HashMap, HashSet};

use crate::source_control::ScKey;

/// The prefix the daemon puts before a failed list read's message, which
/// the part's own wording replaces.
const LIST_FAILED_PREFIX: &str = "stash list failed: ";

/// The prefix of every stash-list request id this model hands out.
const LIST_REQUEST_PREFIX: &str = "sc-stashes-";

/// The prefix of every stash write's `GitWriteError` operation.
const STASH_OPERATION_PREFIX: &str = "stash_";

/// A stash write the view sends for a repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StashOp {
    Push,
    Pop,
    Apply,
    Drop,
}

/// The stash lists by repo, the list requests out, the failed ones and the
/// writes out.
#[derive(Debug, Default)]
pub struct StashModel {
    lists: HashMap<String, Vec<GitStash>>,
    requested: HashSet<String>,
    /// The list requests handed out: request id to repo.
    request_ids: HashMap<String, String>,
    /// The number the last request id carried.
    last_request: u64,
    /// The repos whose last list request failed, with the daemon's reason.
    failed: HashMap<String, String>,
    pending: HashMap<String, StashOp>,
}

impl StashModel {
    /// Fold a daemon message in; `true` when what the view shows changed. A
    /// list replaces its repo's list, whichever tree it came from, and ends
    /// the repo's request, failure and pending write; a `GitWriteError` for
    /// a stash operation ends the repo's pending write; a new connection
    /// forgets everything.
    pub fn apply(&mut self, msg: &DaemonMessage) -> bool {
        match msg {
            DaemonMessage::Welcome { .. } => {
                let changed =
                    !self.lists.is_empty() || !self.failed.is_empty() || !self.pending.is_empty();
                self.lists.clear();
                self.requested.clear();
                self.request_ids.clear();
                self.failed.clear();
                self.pending.clear();
                changed
            }
            DaemonMessage::Stashes {
                repo_id, stashes, ..
            } => {
                self.requested.remove(repo_id);
                self.request_ids.retain(|_, repo| repo != repo_id);
                let recovered = self.failed.remove(repo_id).is_some();
                let ended = self.pending.remove(repo_id).is_some();
                let changed = self.lists.get(repo_id) != Some(stashes);
                self.lists.insert(repo_id.clone(), stashes.clone());
                changed || recovered || ended
            }
            DaemonMessage::GitWriteError {
                repo_id, operation, ..
            } if operation.starts_with(STASH_OPERATION_PREFIX) => {
                self.pending.remove(repo_id).is_some()
            }
            DaemonMessage::Repos { repos } => self.retain_repos(repos),
            _ => false,
        }
    }

    /// Drop every repo not in `repos`; `true` when a shown one went.
    fn retain_repos(&mut self, repos: &[RepoEntry]) -> bool {
        let ids: HashSet<&str> = repos.iter().map(|repo| repo.id.as_str()).collect();
        let before = self.lists.len() + self.failed.len() + self.pending.len();
        self.lists.retain(|repo, _| ids.contains(repo.as_str()));
        self.requested.retain(|repo| ids.contains(repo.as_str()));
        self.request_ids
            .retain(|_, repo| ids.contains(repo.as_str()));
        self.failed.retain(|repo, _| ids.contains(repo.as_str()));
        self.pending.retain(|repo, _| ids.contains(repo.as_str()));
        before != self.lists.len() + self.failed.len() + self.pending.len()
    }

    /// Record that `repo_id`'s list is being asked for, so it is not asked
    /// for again until it arrives, fails or the connection is replaced, and
    /// hand out the request id to send with it: `sc-stashes-<n>`, `n`
    /// counting up from 1. The repo's earlier id, if any, is forgotten, so
    /// only its latest request can mark it failed.
    pub fn request(&mut self, repo_id: &str) -> String {
        self.last_request += 1;
        let id = format!("{LIST_REQUEST_PREFIX}{}", self.last_request);
        self.request_ids.retain(|_, repo| repo != repo_id);
        self.requested.insert(repo_id.to_owned());
        self.request_ids.insert(id.clone(), repo_id.to_owned());
        id
    }

    /// A daemon `Error` answering `request_id`: when the id is this model's
    /// outstanding request for a repo, the repo is marked failed with
    /// `message` (less the daemon's `stash list failed: ` prefix) and is no
    /// longer asked for. Any other `sc-stashes-` id is a superseded or
    /// already answered request of this model's, and is dropped. Returns
    /// whether the id was this model's.
    pub fn fail_request(&mut self, request_id: &str, message: &str) -> bool {
        let Some(repo) = self.request_ids.remove(request_id) else {
            let ours = request_id.starts_with(LIST_REQUEST_PREFIX);
            if ours {
                tracing::debug!(
                    request_id,
                    message,
                    "dropping an error for a stale stash-list request"
                );
            }
            return ours;
        };
        self.requested.remove(&repo);
        let reason = message.strip_prefix(LIST_FAILED_PREFIX).unwrap_or(message);
        self.failed.insert(repo, reason.to_owned());
        true
    }

    /// Why the last list request for `repo_id` failed, until a list arrives.
    #[must_use]
    pub fn failure(&self, repo_id: &str) -> Option<&str> {
        self.failed.get(repo_id).map(String::as_str)
    }

    /// The repo's stashes, newest first, once a list has arrived.
    #[must_use]
    pub fn list(&self, repo_id: &str) -> Option<&[GitStash]> {
        self.lists.get(repo_id).map(Vec::as_slice)
    }

    /// Whether `repo_id`'s list holds `stash` exactly: the same id, subject
    /// and date. Ids are positions, so a list that moved can give another
    /// stash the id.
    #[must_use]
    pub fn holds(&self, repo_id: &str, stash: &GitStash) -> bool {
        self.list(repo_id).is_some_and(|list| list.contains(stash))
    }

    /// The keys of `wanted` whose repo has no list, no request out and no
    /// failed request, the first key of each such repo, in order. A failed
    /// repo waits for Refresh.
    #[must_use]
    pub fn wanted_missing(&self, wanted: &[ScKey]) -> Vec<ScKey> {
        let mut seen = HashSet::new();
        wanted
            .iter()
            .filter(|key| {
                let repo = key.repo_id.as_str();
                !self.lists.contains_key(repo)
                    && !self.requested.contains(repo)
                    && !self.failed.contains_key(repo)
            })
            .filter(|key| seen.insert(key.repo_id.as_str()))
            .cloned()
            .collect()
    }

    /// Record that `op` was sent for `repo_id`.
    pub fn start(&mut self, repo_id: &str, op: StashOp) {
        self.pending.insert(repo_id.to_owned(), op);
    }

    /// The stash write out for `repo_id`, if any.
    #[must_use]
    pub fn pending(&self, repo_id: &str) -> Option<StashOp> {
        self.pending.get(repo_id).copied()
    }

    /// Forget the write out for `repo_id`, as Refresh does; returns whether
    /// there was one.
    pub fn clear_pending(&mut self, repo_id: &str) -> bool {
        self.pending.remove(repo_id).is_some()
    }
}

/// A stash's `created_at` as `YYYY-MM-DD HH:MM`, cut from the ISO string;
/// the string unchanged when it does not start that way.
#[must_use]
pub fn stash_date(created_at: &str) -> String {
    let bytes = created_at.as_bytes();
    let digits = |range: std::ops::Range<usize>| {
        bytes
            .get(range)
            .is_some_and(|part| part.iter().all(u8::is_ascii_digit))
    };
    let at = |index: usize, allowed: &[u8]| bytes.get(index).is_some_and(|b| allowed.contains(b));
    let shaped = digits(0..4)
        && at(4, b"-")
        && digits(5..7)
        && at(7, b"-")
        && digits(8..10)
        && at(10, b"T ")
        && digits(11..13)
        && at(13, b":")
        && digits(14..16);
    match (created_at.get(..10), created_at.get(11..16)) {
        (Some(day), Some(time)) if shaped => format!("{day} {time}"),
        _ => created_at.to_owned(),
    }
}

/// A button of the drop confirm, in Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DropButton {
    Cancel,
    Drop,
}

impl DropButton {
    pub(crate) const ALL: [Self; 2] = [Self::Cancel, Self::Drop];

    pub(crate) fn selector(self) -> &'static str {
        match self {
            Self::Cancel => "stash-drop-confirm-cancel",
            Self::Drop => "stash-drop-confirm-drop",
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Cancel => Self::Drop,
            Self::Drop => Self::Cancel,
        }
    }
}

/// The open drop confirm: the tree it was opened from, the stash it would
/// drop, and the focused button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DropConfirm {
    key: ScKey,
    stash: GitStash,
    focused: DropButton,
}

impl DropConfirm {
    pub(crate) fn new(key: ScKey, stash: GitStash) -> Self {
        Self {
            key,
            stash,
            focused: DropButton::Cancel,
        }
    }

    pub(crate) fn tree(&self) -> &ScKey {
        &self.key
    }

    pub(crate) fn stash(&self) -> &GitStash {
        &self.stash
    }

    /// `Drop stash@{N}?`.
    pub(crate) fn title(&self) -> String {
        format!("Drop {}?", self.stash.id)
    }

    /// What dropping costs.
    pub(crate) fn body(&self) -> String {
        format!(
            "\"{}\" is deleted from the stash list. Getting it back afterwards needs git fsck.",
            self.stash.subject
        )
    }

    pub(crate) fn focused(&self) -> DropButton {
        self.focused
    }

    /// A key while the confirm is open; returns the button it presses. Esc
    /// presses Cancel, Enter the focused button, and Tab and Shift+Tab move
    /// the focus to the other button.
    pub(crate) fn key(&mut self, key: &str) -> Option<DropButton> {
        match key {
            "escape" => Some(DropButton::Cancel),
            "enter" => Some(self.focused),
            "tab" => {
                self.focused = self.focused.other();
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(repo_id: &str, worktree: Option<&str>) -> ScKey {
        ScKey {
            repo_id: repo_id.to_owned(),
            worktree: worktree.map(str::to_owned),
        }
    }

    fn stashes(repo_id: &str, worktree: Option<&str>, count: usize) -> DaemonMessage {
        let stashes: Vec<serde_json::Value> = (0..count)
            .map(|i| json!({ "id": format!("stash@{{{i}}}"), "subject": "WIP", "created_at": "2026-01-01T00:00:00Z" }))
            .collect();
        serde_json::from_value(json!({
            "type": "stashes",
            "repo_id": repo_id,
            "stashes": stashes,
            "worktree_path": worktree,
        }))
        .expect("stashes fixture")
    }

    fn write_error(repo_id: &str, operation: &str) -> DaemonMessage {
        DaemonMessage::GitWriteError {
            repo_id: repo_id.to_owned(),
            operation: operation.to_owned(),
            error: "boom".to_owned(),
            worktree_path: None,
        }
    }

    fn welcome() -> DaemonMessage {
        DaemonMessage::Welcome {
            protocol_version: 22,
            supported_versions: vec![22],
        }
    }

    fn repos(ids: &[&str]) -> DaemonMessage {
        let repos: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| json!({ "id": id, "name": id, "path": format!("D:/{id}") }))
            .collect();
        serde_json::from_value(json!({ "type": "repos", "repos": repos })).expect("repos fixture")
    }

    #[test]
    fn a_list_from_any_tree_is_the_repos_list() {
        let mut model = StashModel::default();
        assert!(model.list("r1").is_none());
        assert!(model.apply(&stashes("r1", Some("D:\\r1-wt"), 2)));
        assert_eq!(
            model.list("r1").expect("r1's list").len(),
            2,
            "a worktree's list is the main tree's too"
        );
        assert!(model.apply(&stashes("r1", None, 1)));
        assert_eq!(model.list("r1").expect("r1's list").len(), 1, "replaced");
        assert!(
            !model.apply(&stashes("r1", Some("D:\\r1-wt"), 1)),
            "the same list"
        );
        assert!(model.list("r2").is_none());

        assert!(model.apply(&repos(&["r2"])));
        assert!(
            model.list("r1").is_none(),
            "an unregistered repo's list goes"
        );
        assert!(model.apply(&stashes("r2", None, 0)));
        assert!(model.apply(&welcome()));
        assert!(model.list("r2").is_none(), "a new connection forgets them");
    }

    #[test]
    fn wanted_missing_names_each_repo_once_until_it_is_asked_for() {
        let mut model = StashModel::default();
        let main = key("r1", None);
        let wt = key("r1", Some("D:\\r1-wt"));
        let other = key("r2", None);
        let wanted = [wt.clone(), main.clone(), other.clone()];
        assert_eq!(
            model.wanted_missing(&wanted),
            [wt.clone(), other.clone()],
            "the first tree of each repo"
        );
        assert_eq!(model.request("r1"), "sc-stashes-1");
        assert_eq!(model.wanted_missing(&wanted), std::slice::from_ref(&other));
        model.apply(&stashes("r2", None, 0));
        assert!(model.wanted_missing(&wanted).is_empty());
        model.apply(&welcome());
        assert_eq!(model.wanted_missing(&wanted), [wt, other]);
    }

    #[test]
    fn a_failed_list_request_is_set_and_cleared() {
        let mut model = StashModel::default();
        let first = model.request("r1");
        let second = model.request("r1");
        assert!(
            model.fail_request(&first, "stash list failed: stale"),
            "a superseded id is still this model's"
        );
        assert_eq!(model.failure("r1"), None, "and marks nothing");
        assert!(!model.fail_request("sc-status-9", "status failed: x"));
        assert!(!model.fail_request("other", "x"));

        assert!(model.fail_request(&second, "stash list failed: not a git repository"));
        assert_eq!(model.failure("r1"), Some("not a git repository"));
        assert!(
            model.wanted_missing(&[key("r1", None)]).is_empty(),
            "a failed repo waits for Refresh"
        );

        assert!(model.apply(&stashes("r1", None, 0)));
        assert_eq!(model.failure("r1"), None, "a list clears it");

        let third = model.request("r1");
        model.fail_request(&third, "locked");
        assert_eq!(model.failure("r1"), Some("locked"), "no prefix to strip");
        assert!(model.apply(&welcome()));
        assert_eq!(model.failure("r1"), None, "a new connection clears it");
    }

    #[test]
    fn pending_is_per_repo_and_each_event_clears_it() {
        let mut model = StashModel::default();
        model.start("r1", StashOp::Push);
        model.start("r2", StashOp::Drop);
        assert_eq!(model.pending("r1"), Some(StashOp::Push));
        assert_eq!(model.pending("r2"), Some(StashOp::Drop));

        assert!(model.apply(&stashes("r1", Some("D:\\r1-wt"), 0)));
        assert_eq!(
            model.pending("r1"),
            None,
            "a list for any of the repo's trees"
        );
        assert_eq!(model.pending("r2"), Some(StashOp::Drop));

        model.start("r1", StashOp::Pop);
        assert!(
            !model.apply(&write_error("r1", "stage")),
            "not a stash write"
        );
        assert_eq!(model.pending("r1"), Some(StashOp::Pop));
        assert!(model.apply(&write_error("r1", "stash_pop")));
        assert_eq!(model.pending("r1"), None);

        model.start("r1", StashOp::Apply);
        assert!(model.clear_pending("r1"), "Refresh clears it");
        assert!(!model.clear_pending("r1"));

        model.start("r1", StashOp::Push);
        assert!(model.apply(&welcome()));
        assert_eq!(model.pending("r1"), None);
        assert_eq!(model.pending("r2"), None);
    }

    #[test]
    fn the_date_is_cut_to_minutes_or_left_raw() {
        assert_eq!(stash_date("2024-05-01T12:34:56Z"), "2024-05-01 12:34");
        assert_eq!(stash_date("2024-05-01T12:34:56+02:00"), "2024-05-01 12:34");
        assert_eq!(stash_date("2024-05-01 12:34"), "2024-05-01 12:34");
        assert_eq!(stash_date("yesterday"), "yesterday");
        assert_eq!(stash_date("2024-05-01"), "2024-05-01");
        assert_eq!(stash_date("2024-05-01T1a:34:56Z"), "2024-05-01T1a:34:56Z");
        assert_eq!(stash_date(""), "");
    }
}
