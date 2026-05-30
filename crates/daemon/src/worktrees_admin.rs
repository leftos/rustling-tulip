//! Disk-scan + cross-reference + delete for the worktrees root, behind
//! `ClientMessage::InspectWorktreesRoot` and `ClientMessage::DeleteWorktreeAt`.
//!
//! Layout assumption (see `crates/daemon/src/git.rs::workspace_worktree_paths`
//! and CLAUDE.md "Where things live on disk"): every per-session worktree
//! lives at `<root>/wt.<branch-slug>/<sanitized-anchor>/<rel-to-anchor>`.
//! `wt.<branch-slug>/` is a direct child of the worktrees root, and members
//! sit nested under the sanitized anchor inside it. Each `wt.<branch-slug>/`
//! directory is one *group* — one row in the management modal — and may
//! contain one member (single-repo session) or several (workspace session).
//!
//! For backward compatibility, the scanner also detects pre-rename leftovers
//! laid out as `<root>/<sanitized-anchor>/wt.<branch-slug>/<member>` (wt dir
//! buried under an anchor prefix) so the modal can still manage them after
//! the layout change. Old-layout groups disappear naturally as their
//! originating sessions stop and clean up via their persisted paths.

use anyhow::{Context as _, anyhow};
use protocol::{
    RootWorktreeEntry, RootWorktreeMember, RootWorktreeStatus, SessionSnapshot, SessionStatus,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

use crate::paths::simplify_path;
use crate::session::SessionRegistry;

/// Walk the worktrees root and return one [`RootWorktreeEntry`] per
/// `wt.<branch>/` group found, cross-referenced against the live and
/// abandoned session registries. Best-effort: I/O failures inside the
/// walk are logged and skipped, never propagated.
pub fn scan_root(root: &Path, sessions: &SessionRegistry) -> Vec<RootWorktreeEntry> {
    let snapshots = sessions.snapshots();
    let xref = build_session_xref(&snapshots);

    let mut entries: Vec<RootWorktreeEntry> = Vec::new();
    if !root.exists() {
        return entries;
    }
    walk_for_wt_dirs(root, root, &xref, &mut entries, 0);

    // Stable ordering: anchor asc, then branch asc — so the modal renders
    // deterministically across consecutive scans.
    entries.sort_by(|a, b| {
        a.anchor
            .cmp(&b.anchor)
            .then_with(|| a.branch_slug.cmp(&b.branch_slug))
    });
    entries
}

/// Maximum nesting depth `scan_root` will descend from the worktrees
/// root before giving up. The anchor depth is the number of path
/// components in `sanitize_anchor`'s output; on Windows that's typically
/// 2 (drive letter + first dir) and on Unix 1–3. 8 is comfortably above
/// any realistic anchor without risking runaway walks if the user
/// points the worktrees root at something pathological.
const MAX_SCAN_DEPTH: usize = 8;

/// Recursive descent looking for `wt.<branch-slug>/` directories.
///
/// Two layouts are supported:
/// - **New layout** (current): `wt.<branch-slug>/` sits at depth 0 of the
///   worktrees root, with members nested under the sanitized anchor inside
///   it. When the walker matches `wt.<slug>` at depth 0 it descends INSIDE
///   to discover the member dirs (any directory containing a `.git` file
///   or subdir) and derives the displayed anchor from the longest common
///   path-prefix of those members' parents relative to the wt dir.
/// - **Old layout** (pre-rename leftovers): `wt.<branch-slug>/` was buried
///   under the sanitized anchor (e.g., `<root>/X/dev/wt.foo/repo`). When
///   the walker matches `wt.<slug>` at depth ≥1 it treats the wt dir's
///   direct children as members and derives the anchor from the path
///   between the root and the wt dir's parent.
fn walk_for_wt_dirs(
    root: &Path,
    cur: &Path,
    xref: &HashMap<PathBuf, (String, bool)>,
    entries: &mut Vec<RootWorktreeEntry>,
    depth: usize,
) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let rd = match std::fs::read_dir(cur) {
        Ok(rd) => rd,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!(?err, dir = %cur.display(), "scan_root: read failed");
            }
            return;
        }
    };
    for ent in rd.flatten() {
        if !is_dir(&ent) {
            continue;
        }
        let path = ent.path();
        let name = ent.file_name().to_string_lossy().into_owned();
        if let Some(branch_slug) = name.strip_prefix("wt.") {
            let (anchor_rel, member_paths) = if depth == 0 {
                // New layout: walk inside to discover members; anchor is
                // the longest common parent prefix of those members
                // relative to the wt dir.
                let members = find_member_dirs(&path);
                let anchor = anchor_from_member_parents(&members, &path);
                (anchor, members)
            } else {
                // Old layout: anchor is path between root and wt's parent;
                // members are direct children of the wt dir.
                let anchor = path
                    .parent()
                    .and_then(|p| p.strip_prefix(root).ok())
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                let members = direct_child_dirs(&path);
                (anchor, members)
            };
            entries.push(build_entry(
                &path,
                &anchor_rel,
                branch_slug,
                &member_paths,
                xref,
            ));
            // Don't descend INTO a wt dir — its children are member
            // worktrees, not nested groups.
            continue;
        }
        walk_for_wt_dirs(root, &path, xref, entries, depth + 1);
    }
}

/// Walk inside a new-layout `wt.<slug>/` directory and return every
/// `.git`-bearing subdirectory found. Stops descending once a dir is
/// identified as a member (its inner structure is the member's own
/// content, not nested groups). Best-effort — I/O failures inside the
/// walk are silently skipped.
fn find_member_dirs(wt_path: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(wt_path.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_SCAN_DEPTH {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            if !is_dir(&ent) {
                continue;
            }
            let path = ent.path();
            if path.join(".git").exists() {
                out.push(path);
            } else {
                stack.push((path, depth + 1));
            }
        }
    }
    out
}

/// Direct child directories of `wt_path`. Used to enumerate old-layout
/// members where each member is a direct child of the wt dir.
fn direct_child_dirs(wt_path: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(wt_path) else {
        return Vec::new();
    };
    rd.flatten().filter(is_dir).map(|ent| ent.path()).collect()
}

/// Anchor display for a new-layout group: longest common path-prefix of
/// each member's parent directory, relative to `wt_path`. For a workspace
/// laid out at `<root>/wt.foo/X/dev/{a,b}`, the anchor is `X/dev`. For a
/// single-member layout `<root>/wt.foo/X/dev/repo`, also `X/dev`. Empty
/// when there are no members or the members share no common parent.
fn anchor_from_member_parents(members: &[PathBuf], wt_path: &Path) -> String {
    let rel_parents: Vec<Vec<String>> = members
        .iter()
        .filter_map(|m| m.parent())
        .filter_map(|p| p.strip_prefix(wt_path).ok())
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect()
        })
        .collect();
    let Some((first, rest)) = rel_parents.split_first() else {
        return String::new();
    };
    let mut prefix = first.clone();
    for other in rest {
        let n = prefix
            .iter()
            .zip(other.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(n);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.join("/")
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
    let target = simplify_path(
        &target
            .canonicalize()
            .with_context(|| format!("canonicalizing {}", target.display()))?,
    );
    let root_canon = simplify_path(
        &root
            .canonicalize()
            .with_context(|| format!("canonicalizing root {}", root.display()))?,
    );
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
/// a member path that lives anywhere under the canonical target wt dir.
/// Uses `starts_with` rather than `parent ==` so it catches both new-layout
/// members (nested under an anchor inside the wt dir) and old-layout
/// members (direct children of the wt dir).
fn assert_no_live_session(target: &Path, sessions: &SessionRegistry) -> anyhow::Result<()> {
    let snapshots = sessions.snapshots();
    for snap in &snapshots {
        if !is_session_live(snap) {
            continue;
        }
        for member_path in &snap.worktree_paths {
            let mp = Path::new(member_path);
            let mp_canon = mp
                .canonicalize()
                .map_or_else(|_| mp.to_path_buf(), |p| simplify_path(&p));
            if mp_canon.starts_with(target) {
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
/// `git worktree prune` after deletion. Uses `find_member_dirs` so it
/// works for both new-layout (members nested under anchor inside the wt
/// dir) and old-layout (members as direct children) groups.
async fn delete_members(target: &Path) -> anyhow::Result<()> {
    let member_paths = find_member_dirs(target);
    for member_path in member_paths {
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

/// Build a per-wt-group cross-reference from the canonical wt-dir path
/// to `(session_id, is_live)`. The wt dir is found by walking up from
/// each member path to the nearest ancestor whose file name starts with
/// `wt.` — which handles both the new layout (wt dir at depth 0 of the
/// worktrees root) and old-layout leftovers (wt dir buried under an
/// anchor prefix).
fn build_session_xref(snapshots: &[SessionSnapshot]) -> HashMap<PathBuf, (String, bool)> {
    let mut map: HashMap<PathBuf, (String, bool)> = HashMap::new();
    for snap in snapshots {
        let live = is_session_live(snap);
        for member_path in &snap.worktree_paths {
            if let Some(wt_dir) = wt_dir_for_member(Path::new(member_path)) {
                let key = std::fs::canonicalize(&wt_dir)
                    .map_or_else(|_| wt_dir.clone(), |p| simplify_path(&p));
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

/// Walk up from `member_path` to the nearest ancestor directory whose
/// file name starts with `wt.`. Returns `None` if no such ancestor
/// exists. Works for both layouts because the wt dir is always somewhere
/// above the member in the on-disk tree.
fn wt_dir_for_member(member_path: &Path) -> Option<PathBuf> {
    let mut cur = member_path.parent()?;
    loop {
        if cur
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.starts_with("wt."))
        {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}

fn build_entry(
    wt_path: &Path,
    anchor_name: &str,
    branch_slug: &str,
    member_paths: &[PathBuf],
    xref: &HashMap<PathBuf, (String, bool)>,
) -> RootWorktreeEntry {
    let key = std::fs::canonicalize(wt_path)
        .map_or_else(|_| wt_path.to_path_buf(), |p| simplify_path(&p));
    let (session_id, status) = match xref.get(&key) {
        Some((sid, true)) => (Some(sid.clone()), RootWorktreeStatus::Active),
        Some((sid, false)) => (Some(sid.clone()), RootWorktreeStatus::Detached),
        None => (None, RootWorktreeStatus::Stale),
    };

    let mut members: Vec<RootWorktreeMember> = Vec::with_capacity(member_paths.len());
    for member_path in member_paths {
        let repo_path = repo_path_for_worktree(member_path);
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

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::print_stderr,
    clippy::cast_precision_loss,
    clippy::map_unwrap_or,
    reason = "tests assert preconditions with unwrap; the live-disk probe \
              prints scan results to stderr so the operator can read them"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use uuid::Uuid;

    /// Lightweight `tempdir` stand-in — same pattern as `binary_cache::tests`
    /// to avoid adding a dep just for tests.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("rt-worktrees-admin-{}", Uuid::new_v4().simple()));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Build an empty `SessionRegistry` pointed at a temp config dir.
    /// The xref it produces is empty, so every wt group `scan_root`
    /// returns shows `RootWorktreeStatus::Stale` — useful for tests
    /// that just want to verify the walker reaches the right dirs.
    fn empty_registry(tmp: &std::path::Path) -> Arc<SessionRegistry> {
        let dirs = crate::paths::Dirs {
            config: tmp.to_path_buf(),
            state_file: tmp.join("state.json"),
            handshake_file: tmp.join("daemon.json"),
            lan_config_file: tmp.join("lan.json"),
            lan_cert_file: tmp.join("lan-cert.pem"),
            lan_key_file: tmp.join("lan-key.pem"),
            sessions_dir: tmp.join("sessions"),
            worktrees_dir: tmp.join("worktrees"),
            binaries_dir: tmp.join("binaries"),
        };
        SessionRegistry::new(dirs)
    }

    fn touch(p: &std::path::Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"").unwrap();
    }

    #[test]
    fn scan_root_walks_through_multi_segment_anchor() {
        // Mirrors the on-disk layout `sanitize_anchor("X:/dev")`
        // produces: <root>/X/dev/wt.<slug>/<member>/.git. The
        // pre-walker code only descended one level, so it missed
        // every Windows-style two-segment anchor.
        let tmp = Scratch::new();
        let root = tmp.path().join("worktrees");
        let wt_dir = root.join("X").join("dev").join("wt.feature-foo");
        let member = wt_dir.join("repo1");
        touch(&member.join(".git"));

        let cfg_tmp = Scratch::new();
        let sessions = empty_registry(cfg_tmp.path());
        let entries = scan_root(&root, &sessions);

        assert_eq!(entries.len(), 1, "expected one wt group, got {entries:?}");
        let entry = &entries[0];
        assert_eq!(entry.branch_slug, "feature-foo");
        assert_eq!(entry.anchor, "X/dev");
        assert_eq!(entry.path, wt_dir.to_string_lossy());
        // No live session, so it's stale.
        assert!(matches!(entry.status, RootWorktreeStatus::Stale));
    }

    #[test]
    fn scan_root_finds_groups_at_multiple_depths() {
        // Mix of depths: one wt at depth 1 (unix-style anchor) and
        // another at depth 2 (windows-style). The walker should find
        // both without confusion.
        let tmp = Scratch::new();
        let root = tmp.path().join("worktrees");
        touch(
            &root
                .join("flat-anchor")
                .join("wt.foo")
                .join("repo")
                .join(".git"),
        );
        touch(
            &root
                .join("X")
                .join("dev")
                .join("wt.bar")
                .join("repo")
                .join(".git"),
        );

        let cfg_tmp = Scratch::new();
        let sessions = empty_registry(cfg_tmp.path());
        let entries = scan_root(&root, &sessions);

        let slugs: Vec<&str> = entries.iter().map(|e| e.branch_slug.as_str()).collect();
        assert_eq!(slugs, vec!["bar", "foo"]); // sorted by anchor asc
        let anchors: Vec<&str> = entries.iter().map(|e| e.anchor.as_str()).collect();
        assert_eq!(anchors, vec!["X/dev", "flat-anchor"]);
    }

    #[test]
    fn scan_root_does_not_descend_into_wt_dirs() {
        // A wt dir whose member happens to start with `wt.` must not
        // be mistaken for a nested group. (Hypothetical: branch slug
        // could in principle look like one but the walker stops at
        // the first wt.<slug> match anyway.)
        let tmp = Scratch::new();
        let root = tmp.path().join("worktrees");
        let wt_dir = root.join("X").join("wt.outer");
        touch(&wt_dir.join("wt.inner-looking-member").join(".git"));

        let cfg_tmp = Scratch::new();
        let sessions = empty_registry(cfg_tmp.path());
        let entries = scan_root(&root, &sessions);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].branch_slug, "outer");
    }

    #[test]
    fn scan_root_new_layout_single_member() {
        // New layout: `<root>/wt.<slug>/<sanitized-anchor>/<member>/.git`.
        // Anchor is computed from the path between the wt dir and the
        // member's parent, not from anything above the wt dir.
        let tmp = Scratch::new();
        let root = tmp.path().join("worktrees");
        let wt_dir = root.join("wt.feature-foo");
        let member = wt_dir.join("X").join("dev").join("repo1");
        touch(&member.join(".git"));

        let cfg_tmp = Scratch::new();
        let sessions = empty_registry(cfg_tmp.path());
        let entries = scan_root(&root, &sessions);

        assert_eq!(entries.len(), 1, "expected one wt group, got {entries:?}");
        let entry = &entries[0];
        assert_eq!(entry.branch_slug, "feature-foo");
        assert_eq!(entry.anchor, "X/dev");
        assert_eq!(entry.path, wt_dir.to_string_lossy());
        assert_eq!(entry.members.len(), 1);
        assert_eq!(entry.members[0].worktree_path, member.to_string_lossy());
        assert!(matches!(entry.status, RootWorktreeStatus::Stale));
    }

    #[test]
    fn scan_root_new_layout_multi_member_workspace() {
        // New layout workspace: two members sharing an anchor inside the
        // wt dir. Anchor display is the common path prefix of member
        // parents relative to the wt dir.
        let tmp = Scratch::new();
        let root = tmp.path().join("worktrees");
        let wt_dir = root.join("wt.main");
        touch(&wt_dir.join("X").join("dev").join("yaat").join(".git"));
        touch(
            &wt_dir
                .join("X")
                .join("dev")
                .join("yaat-server")
                .join(".git"),
        );

        let cfg_tmp = Scratch::new();
        let sessions = empty_registry(cfg_tmp.path());
        let entries = scan_root(&root, &sessions);

        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.branch_slug, "main");
        assert_eq!(entry.anchor, "X/dev");
        assert_eq!(entry.members.len(), 2);
        let hints: Vec<&str> = entry
            .members
            .iter()
            .map(|m| m.repo_name_hint.as_str())
            .collect();
        assert_eq!(hints, vec!["yaat", "yaat-server"]);
    }

    #[test]
    fn scan_root_mixed_new_and_old_layouts() {
        // One new-layout group at depth 0 and one old-layout group at
        // depth ≥1 should both surface. Verifies the walker handles
        // transitional disk state (some leftovers from before the
        // layout rename, some new spawns after).
        let tmp = Scratch::new();
        let root = tmp.path().join("worktrees");
        // New layout: wt at depth 0
        touch(
            &root
                .join("wt.alpha")
                .join("X")
                .join("dev")
                .join("repo")
                .join(".git"),
        );
        // Old layout: wt buried under an anchor
        touch(
            &root
                .join("Y")
                .join("dev")
                .join("wt.beta")
                .join("repo")
                .join(".git"),
        );

        let cfg_tmp = Scratch::new();
        let sessions = empty_registry(cfg_tmp.path());
        let entries = scan_root(&root, &sessions);

        assert_eq!(entries.len(), 2);
        let pairs: Vec<(&str, &str)> = entries
            .iter()
            .map(|e| (e.anchor.as_str(), e.branch_slug.as_str()))
            .collect();
        // Sorted by anchor asc.
        assert_eq!(pairs, vec![("X/dev", "alpha"), ("Y/dev", "beta")]);
    }

    /// Probe-style test that scans whatever directory `RT_SCAN_PATH`
    /// points at and prints every entry's anchor + slug + status + size.
    /// Skipped by default (no env var set); run with
    /// `cargo test -p daemon scan_user_dir -- --include-ignored --nocapture
    /// RT_SCAN_PATH=...` to point at a real worktrees root.
    #[test]
    #[ignore = "live-disk probe — needs RT_SCAN_PATH to point at a worktrees root"]
    fn scan_user_dir_probe() {
        let Ok(raw) = std::env::var("RT_SCAN_PATH") else {
            eprintln!("RT_SCAN_PATH not set; nothing to probe");
            return;
        };
        let path = PathBuf::from(&raw);
        let cfg_tmp = Scratch::new();
        let sessions = empty_registry(cfg_tmp.path());
        let entries = scan_root(&path, &sessions);
        eprintln!("=== scan_root({}) ===", path.display());
        eprintln!("found {} group(s):", entries.len());
        for e in &entries {
            let size_mb = e
                .size_bytes
                .map(|b| (b as f64) / (1024.0 * 1024.0))
                .unwrap_or(0.0);
            eprintln!(
                "  [{:?}] anchor={} slug={} size={:.1}MB members={} path={}",
                e.status,
                e.anchor,
                e.branch_slug,
                size_mb,
                e.members.len(),
                e.path,
            );
        }
    }
}
