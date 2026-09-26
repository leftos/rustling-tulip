//! The source-control writes in flight and the errors they leave: which
//! trees have a stage, unstage or discard out, the banner a failed git
//! write leaves on its section, and the paths a changes-tree row sends.
//!
//! Plain Rust, so every rule is unit-tested; the changes view renders it.

use protocol::{DaemonMessage, GitFileChange};
use std::collections::{HashMap, HashSet};

use crate::source_control::ScKey;

/// A git write the changes view sends for one tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOp {
    Stage,
    Unstage,
    Discard,
}

/// The writes out per tree and the banners failed writes left.
#[derive(Debug, Default)]
pub struct ScWrites {
    pending: HashMap<ScKey, WriteOp>,
    banners: HashMap<ScKey, String>,
}

impl ScWrites {
    /// Fold a daemon message in; `true` when what the view shows changed. A
    /// status for a tree ends its pending write; a `GitWriteError` ends it
    /// too and sets the tree's banner to `<operation>: <error>`, replacing
    /// any; a new connection forgets both.
    pub fn apply(&mut self, msg: &DaemonMessage) -> bool {
        match msg {
            DaemonMessage::Welcome { .. } => {
                let changed = !self.pending.is_empty() || !self.banners.is_empty();
                self.pending.clear();
                self.banners.clear();
                changed
            }
            DaemonMessage::RepoStatus {
                repo_id,
                worktree_path,
                ..
            } => self
                .pending
                .remove(&tree_key(repo_id, worktree_path.as_ref()))
                .is_some(),
            DaemonMessage::GitWriteError {
                repo_id,
                operation,
                error,
                worktree_path,
            } => {
                let key = tree_key(repo_id, worktree_path.as_ref());
                self.pending.remove(&key);
                self.banners.insert(key, format!("{operation}: {error}"));
                true
            }
            _ => false,
        }
    }

    /// Record that `op` was sent for `key`.
    pub fn start(&mut self, key: ScKey, op: WriteOp) {
        self.pending.insert(key, op);
    }

    /// The write out for `key`, if any.
    #[must_use]
    pub fn pending(&self, key: &ScKey) -> Option<WriteOp> {
        self.pending.get(key).copied()
    }

    /// Forget the write out for `key`, as Refresh does; returns whether
    /// there was one.
    pub fn clear_pending(&mut self, key: &ScKey) -> bool {
        self.pending.remove(key).is_some()
    }

    /// The banner a failed write left on `key`'s section.
    #[must_use]
    pub fn banner(&self, key: &ScKey) -> Option<&str> {
        self.banners.get(key).map(String::as_str)
    }

    /// The banner's ✕; returns whether there was a banner.
    pub fn dismiss(&mut self, key: &ScKey) -> bool {
        self.banners.remove(key).is_some()
    }

    /// Drop the banners of trees that are no longer sections; returns
    /// whether any went.
    pub fn retain_banners(&mut self, sections: &[ScKey]) -> bool {
        let before = self.banners.len();
        self.banners.retain(|key, _| sections.contains(key));
        before != self.banners.len()
    }
}

/// The key a git message names.
fn tree_key(repo_id: &str, worktree_path: Option<&String>) -> ScKey {
    ScKey {
        repo_id: repo_id.to_owned(),
        worktree: worktree_path.cloned(),
    }
}

/// The paths a row of the changes tree sends: its path, preceded by the
/// path it was renamed or copied from, so both sides move together.
#[must_use]
pub fn row_paths(change: &GitFileChange) -> Vec<String> {
    match &change.from_path {
        Some(from) => vec![from.clone(), change.path.clone()],
        None => vec![change.path.clone()],
    }
}

/// Every path a bucket's rows send, each once, in row order.
#[must_use]
pub fn bucket_paths(changes: &[GitFileChange]) -> Vec<String> {
    let mut seen = HashSet::new();
    changes
        .iter()
        .flat_map(row_paths)
        .filter(|path| seen.insert(path.clone()))
        .collect()
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

    fn status(repo_id: &str, worktree: Option<&str>) -> DaemonMessage {
        serde_json::from_value(json!({
            "type": "repo_status",
            "repo_id": repo_id,
            "index_changes": [],
            "worktree_changes": [],
            "worktree_path": worktree,
        }))
        .expect("status fixture")
    }

    fn write_error(
        repo_id: &str,
        worktree: Option<&str>,
        operation: &str,
        error: &str,
    ) -> DaemonMessage {
        DaemonMessage::GitWriteError {
            repo_id: repo_id.to_owned(),
            operation: operation.to_owned(),
            error: error.to_owned(),
            worktree_path: worktree.map(str::to_owned),
        }
    }

    fn welcome() -> DaemonMessage {
        DaemonMessage::Welcome {
            protocol_version: 22,
            supported_versions: vec![22],
        }
    }

    fn change(path: &str, status: &str, from_path: Option<&str>) -> GitFileChange {
        serde_json::from_value(json!({ "path": path, "status": status, "from_path": from_path }))
            .expect("change fixture")
    }

    #[test]
    fn pending_is_per_key_and_a_status_for_the_key_clears_it() {
        let mut writes = ScWrites::default();
        let main = key("r1", None);
        let wt = key("r1", Some("D:\\r1-wt"));
        writes.start(main.clone(), WriteOp::Stage);
        writes.start(wt.clone(), WriteOp::Discard);
        assert_eq!(writes.pending(&main), Some(WriteOp::Stage));
        assert_eq!(writes.pending(&wt), Some(WriteOp::Discard));

        assert!(!writes.apply(&status("r2", None)), "another repo's status");
        assert!(writes.apply(&status("r1", None)));
        assert_eq!(writes.pending(&main), None);
        assert_eq!(
            writes.pending(&wt),
            Some(WriteOp::Discard),
            "the worktree is its own key"
        );
        assert!(!writes.apply(&status("r1", None)), "nothing left to clear");
    }

    #[test]
    fn a_write_error_for_the_key_clears_pending() {
        let mut writes = ScWrites::default();
        let wt = key("r1", Some("D:\\r1-wt"));
        writes.start(wt.clone(), WriteOp::Unstage);
        assert!(writes.apply(&write_error("r1", Some("D:\\r1-wt"), "unstage", "locked")));
        assert_eq!(writes.pending(&wt), None);
    }

    #[test]
    fn refresh_and_a_welcome_clear_pending() {
        let mut writes = ScWrites::default();
        let main = key("r1", None);
        writes.start(main.clone(), WriteOp::Stage);
        assert!(writes.clear_pending(&main), "Refresh clears it");
        assert!(!writes.clear_pending(&main), "and there is nothing left");

        writes.start(main.clone(), WriteOp::Discard);
        assert!(writes.apply(&welcome()));
        assert_eq!(writes.pending(&main), None);
        assert!(!writes.apply(&welcome()), "an empty store is no change");
    }

    #[test]
    fn a_write_error_sets_and_replaces_the_banner() {
        let mut writes = ScWrites::default();
        let main = key("r1", None);
        writes.apply(&write_error("r1", None, "stage", "index.lock exists"));
        assert_eq!(writes.banner(&main), Some("stage: index.lock exists"));
        assert_eq!(writes.banner(&key("r1", Some("D:\\r1-wt"))), None);

        writes.apply(&write_error("r1", None, "discard", "permission denied"));
        assert_eq!(
            writes.banner(&main),
            Some("discard: permission denied"),
            "a new error replaces the banner"
        );
    }

    #[test]
    fn a_banner_clears_on_dismiss_welcome_and_when_its_key_is_no_section() {
        let mut writes = ScWrites::default();
        let main = key("r1", None);
        let wt = key("r1", Some("D:\\r1-wt"));
        writes.apply(&write_error("r1", None, "stage", "boom"));
        assert!(writes.dismiss(&main));
        assert_eq!(writes.banner(&main), None);
        assert!(!writes.dismiss(&main), "nothing left to dismiss");

        writes.apply(&write_error("r1", None, "stage", "boom"));
        assert!(writes.apply(&welcome()));
        assert_eq!(writes.banner(&main), None);

        writes.apply(&write_error("r1", None, "stage", "boom"));
        writes.apply(&write_error("r1", Some("D:\\r1-wt"), "stage", "boom"));
        assert!(
            !writes.retain_banners(&[main.clone(), wt.clone()]),
            "both still sections"
        );
        assert!(writes.retain_banners(std::slice::from_ref(&wt)));
        assert_eq!(
            writes.banner(&main),
            None,
            "its key stopped being a section"
        );
        assert!(writes.banner(&wt).is_some());
    }

    #[test]
    fn a_rename_row_sends_both_sides() {
        assert_eq!(row_paths(&change("src/a.rs", "M", None)), ["src/a.rs"]);
        assert_eq!(
            row_paths(&change("src/new.rs", "R", Some("src/old.rs"))),
            ["src/old.rs", "src/new.rs"],
            "the old path first, so both sides move together"
        );
        assert_eq!(
            bucket_paths(&[
                change("b.rs", "R", Some("a.rs")),
                change("a.rs", "M", None),
                change("c.rs", "?", None),
            ]),
            ["a.rs", "b.rs", "c.rs"],
            "every path of the bucket, each once, in order"
        );
    }
}
