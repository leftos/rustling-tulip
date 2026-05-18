//! Disk-scan + cross-reference + delete for the worktrees root, behind
//! `ClientMessage::InspectWorktreesRoot` and `ClientMessage::DeleteWorktreeAt`.
//!
//! Layout assumption (see `crates/daemon/src/git.rs::workspace_worktree_paths`
//! and CLAUDE.md "Where things live on disk"): every per-session worktree
//! lives at `<root>/<sanitized-anchor>/wt.<branch-slug>/<rel-to-anchor>`.
//! Each `wt.<branch-slug>/` directory is one *group* — one row in the
//! management modal — and may contain one member (single-repo session) or
//! several (workspace session). The functions here always reason at the
//! group level and let the caller turn each group into one `RootWorktreeEntry`.

use anyhow::{Context as _, anyhow};
use protocol::{
    RootWorktreeEntry, RootWorktreeMember, RootWorktreeStatus, SessionSnapshot, SessionStatus,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

use crate::session::SessionRegistry;

/// Walk the worktrees root and return one [`RootWorktreeEntry`] per
/// `wt.<branch>/` group found, cross-referenced against the live and
/// abandoned session registries. Best-effort: I/O failures inside the
/// walk are logged and skipped, never propagated.
pub fn scan_root(root: &Path, sessions: &SessionRegistry) -> Vec<RootWorktreeEntry> {
    let snapshots = sessions.snapshots();
    let xref = build_session_xref(&snapshots);

    let mut entries: Vec<RootWorktreeEntry> = Vec::new();
    let anchor_dirs = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(err) => {
            // Root doesn't exist yet (fresh install, user just changed it,
            // etc.). Empty snapshot is the right answer.
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!(?err, root = %root.display(), "scan_root: read root failed");
            }
            return entries;
        }
    };

    for anchor_ent in anchor_dirs.flatten() {
        let anchor_path = anchor_ent.path();
        if !is_dir(&anchor_ent) {
            continue;
        }
        let anchor_name = anchor_ent.file_name().to_string_lossy().into_owned();
        let wt_dirs = match std::fs::read_dir(&anchor_path) {
            Ok(rd) => rd,
            Err(err) => {
                warn!(?err, anchor = %anchor_path.display(), "scan_root: read anchor failed");
                continue;
            }
        };
        for wt_ent in wt_dirs.flatten() {
            if !is_dir(&wt_ent) {
                continue;
            }
            let wt_name = wt_ent.file_name().to_string_lossy().into_owned();
            let Some(branch_slug) = wt_name.strip_prefix("wt.") else {
                // Not a managed worktree dir; skip. (Other tooling may
                // have left foreign content under the anchor.)
                continue;
            };
            let wt_path = wt_ent.path();
            let entry = build_entry(
                &wt_path,
                &anchor_name,
                branch_slug,
                &xref,
            );
            entries.push(entry);
        }
    }

    // Stable ordering: anchor asc, then branch asc — so the modal renders
    // deterministically across consecutive scans.
    entries.sort_by(|a, b| {
        a.anchor
            .cmp(&b.anchor)
            .then_with(|| a.branch_slug.cmp(&b.branch_slug))
    });
    entries
}

/// Delete a `wt.<branch>/` group from disk. Refuses if any member is
/// referenced by a non-stopped, non-abandoned session (the user must
/// stop the session first). Each member is run through
/// [`crate::worktree_cleanup::remove_member`] which tries
/// `git -C <repo> worktree remove --force`, retries once after a brief
/// delay (covers transient Windows file locks), falls back to
/// `fs::remove_dir_all`, and runs `git worktree prune` on success.
/// After every member is processed, the wrapper dir itself is removed.
pub async fn delete_group(
    root: &Path,
    target: &Path,
    sessions: &SessionRegistry,
) -> anyhow::Result<()> {
    let target = validate_target(root, target)?;
    assert_no_live_session(&target, sessions)?;
    delete_members(&target).await?;
    finalize_group_dir(&target)?;
    Ok(())
}

/// Canonicalize the target, verify it's under the worktrees root and
/// that its leaf name starts with `wt.`. Returns the canonical target
/// path. The two early-bail checks block accidental deletion of arbitrary
/// directories should a bad path arrive over the wire.
fn validate_target(root: &Path, target: &Path) -> anyhow::Result<PathBuf> {
    let target = target
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", target.display()))?;
    let root_canon = root
        .canonicalize()
        .with_context(|| format!("canonicalizing root {}", root.display()))?;
    if !target.starts_with(&root_canon) {
        return Err(anyhow!(
            "refusing to delete {}: not under worktrees root {}",
            target.display(),
            root_canon.display()
        ));
    }
    let wt_name = target
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("target path has no file name: {}", target.display()))?;
    if !wt_name.starts_with("wt.") {
        return Err(anyhow!(
            "refusing to delete {}: not a wt.<branch> directory",
            target.display()
        ));
    }
    Ok(target)
}

/// Reject the delete if any live (non-stopped, non-abandoned) session has
/// a member path whose parent is the canonical target.
fn assert_no_live_session(target: &Path, sessions: &SessionRegistry) -> anyhow::Result<()> {
    let snapshots = sessions.snapshots();
    for snap in &snapshots {
        if !is_session_live(snap) {
            continue;
        }
        for member_path in &snap.worktree_paths {
            if let Some(parent) = Path::new(member_path).parent()
                && let Ok(parent_canon) = parent.canonicalize()
                && parent_canon == target
            {
                return Err(anyhow!(
                    "refusing to delete {}: live session {} ({}) is using it",
                    target.display(),
                    snap.id,
                    snap.label
                ));
            }
        }
    }
    Ok(())
}

/// Walk every member dir under the group and hand each to the shared
/// robust cleanup helper. The originating repo (read from the member's
/// `.git` gitfile) is looked up per member so that members from
/// different repos in a workspace session each get their own
/// `git worktree prune` after deletion.
async fn delete_members(target: &Path) -> anyhow::Result<()> {
    let members = std::fs::read_dir(target)
        .with_context(|| format!("reading {}", target.display()))?
        .flatten()
        .collect::<Vec<_>>();
    for member_ent in members {
        if !is_dir(&member_ent) {
            continue;
        }
        let member_path = member_ent.path();
        let repo_path = repo_path_for_worktree(&member_path);
        if let Some(repo) = repo_path.as_deref() {
            match crate::worktree_cleanup::remove_member(repo, &member_path).await {
                crate::worktree_cleanup::CleanupOutcome::Removed => {}
                crate::worktree_cleanup::CleanupOutcome::StillOnDisk { reason } => {
                    return Err(anyhow!(
                        "could not remove member {}: {reason}",
                        member_path.display()
                    ));
                }
            }
        } else {
            // No reachable originating repo (stale .git gitfile, repo
            // moved, etc.) — skip the git remove attempt and go straight
            // to the filesystem delete. This is the "stale wt entry left
            // over from a deleted repo" case the management modal exists
            // to clean up.
            std::fs::remove_dir_all(&member_path)
                .or_else(|err| match err.kind() {
                    std::io::ErrorKind::NotFound => Ok(()),
                    _ => Err(err),
                })
                .with_context(|| format!("removing member dir {}", member_path.display()))?;
        }
    }
    Ok(())
}

/// Drop the `wt.*` dir itself. If it's not empty (foreign content left
/// behind), promote to a recursive delete so the user doesn't have to
/// chase a leftover dir manually.
fn finalize_group_dir(target: &Path) -> anyhow::Result<()> {
    match std::fs::remove_dir(target) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => std::fs::remove_dir_all(target).with_context(|| {
            format!(
                "removing wt dir {} (after non-empty remove_dir: {err})",
                target.display()
            )
        }),
    }
}

/// Build a per-wt-group cross-reference from `worktree_path.parent()` to
/// `(session_id, status_kind)`. `status_kind` is `(is_live, snapshot)`
/// so the caller can pick Active vs Detached without re-deriving.
fn build_session_xref(
    snapshots: &[SessionSnapshot],
) -> HashMap<PathBuf, (String, bool)> {
    let mut map: HashMap<PathBuf, (String, bool)> = HashMap::new();
    for snap in snapshots {
        let live = is_session_live(snap);
        for member_path in &snap.worktree_paths {
            if let Some(parent) = Path::new(member_path).parent() {
                let key = std::fs::canonicalize(parent)
                    .unwrap_or_else(|_| parent.to_path_buf());
                // Keep the most-active record per group: if any session
                // member of this group is live, the group is Active.
                map.entry(key)
                    .and_modify(|cur| {
                        if live && !cur.1 {
                            *cur = (snap.id.clone(), true);
                        }
                    })
                    .or_insert_with(|| (snap.id.clone(), live));
            }
        }
    }
    map
}

fn build_entry(
    wt_path: &Path,
    anchor_name: &str,
    branch_slug: &str,
    xref: &HashMap<PathBuf, (String, bool)>,
) -> RootWorktreeEntry {
    let key = std::fs::canonicalize(wt_path).unwrap_or_else(|_| wt_path.to_path_buf());
    let (session_id, status) = match xref.get(&key) {
        Some((sid, true)) => (Some(sid.clone()), RootWorktreeStatus::Active),
        Some((sid, false)) => (Some(sid.clone()), RootWorktreeStatus::Detached),
        None => (None, RootWorktreeStatus::Stale),
    };

    let mut members: Vec<RootWorktreeMember> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(wt_path) {
        for ent in rd.flatten() {
            if !is_dir(&ent) {
                continue;
            }
            let member_path = ent.path();
            let repo_path = repo_path_for_worktree(&member_path);
            let repo_name_hint = repo_path
                .as_ref()
                .and_then(|p| p.file_name())
                .or_else(|| member_path.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            members.push(RootWorktreeMember {
                worktree_path: member_path.to_string_lossy().into_owned(),
                repo_path: repo_path.map(|p| p.to_string_lossy().into_owned()),
                repo_name_hint,
            });
        }
    }
    members.sort_by(|a, b| a.repo_name_hint.cmp(&b.repo_name_hint));

    let (size_bytes, last_modified_unix) = group_size_and_mtime(wt_path);

    RootWorktreeEntry {
        path: wt_path.to_string_lossy().into_owned(),
        anchor: anchor_name.to_owned(),
        branch_slug: branch_slug.to_owned(),
        members,
        status,
        session_id,
        size_bytes,
        last_modified_unix,
    }
}

/// True iff this session is currently running a process — i.e., deleting
/// its worktree would yank the rug out from under live work. Orphan
/// (detached but process still alive) counts as live for safety.
fn is_session_live(snap: &SessionSnapshot) -> bool {
    if snap.is_abandoned {
        return false;
    }
    match snap.status {
        SessionStatus::Stopped | SessionStatus::Error => false,
        SessionStatus::Spawning
        | SessionStatus::Idle
        | SessionStatus::Working
        | SessionStatus::AwaitingInput => true,
    }
}

/// Read the `.git` gitfile inside `worktree` and derive the originating
/// repo's working-tree path. Returns `None` if the file is missing,
/// malformed, or points to a path that no longer exists.
fn repo_path_for_worktree(worktree: &Path) -> Option<PathBuf> {
    let gitfile = worktree.join(".git");
    let contents = std::fs::read_to_string(&gitfile).ok()?;
    let line = contents.lines().find(|l| l.starts_with("gitdir:"))?;
    let raw = line.strip_prefix("gitdir:")?.trim();
    let gitdir = PathBuf::from(raw);
    // <repo>/.git/worktrees/<wt-name>  →  pop wt-name, pop worktrees, pop .git
    let repo_dot_git = gitdir.parent()?.parent()?;
    let repo = repo_dot_git.parent()?.to_path_buf();
    repo.exists().then_some(repo)
}

fn is_dir(ent: &std::fs::DirEntry) -> bool {
    ent.file_type().is_ok_and(|t| t.is_dir())
}

/// Recursive directory size + max-mtime walk for a single group. Best-
/// effort: I/O failures inside the walk silently skip the offending
/// entry. Returns `(None, None)` only if the root walk itself fails.
fn group_size_and_mtime(root: &Path) -> (Option<u64>, Option<i64>) {
    let mut total: u64 = 0;
    let mut latest: Option<i64> = None;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            let Ok(meta) = ent.metadata() else { continue };
            if let Ok(mtime) = meta.modified()
                && let Ok(elapsed) = mtime.duration_since(UNIX_EPOCH)
            {
                let unix = i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX);
                latest = Some(latest.map_or(unix, |cur| cur.max(unix)));
            } else if let Ok(elapsed) = SystemTime::now().duration_since(UNIX_EPOCH) {
                // Future modified time — clamp to "now" so the column
                // sorts sensibly rather than displaying nonsense.
                let unix = i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX);
                latest = Some(latest.map_or(unix, |cur| cur.max(unix)));
            }
            if meta.is_dir() {
                stack.push(ent.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    (Some(total), latest)
}

