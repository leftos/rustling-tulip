//! Disk-scan + cross-reference + delete for the worktrees root, behind
//! `ClientMessage::InspectWorktreesRoot` and `ClientMessage::DeleteWorktreeAt`.
//!
//! Layout (see `crates/daemon/src/git.rs::workspace_worktree_paths` and
//! CLAUDE.md "Where things live on disk"): a single repo's worktree is the
//! group folder `<root>/wt.<branch-slug>/<repo-slug>` itself; a workspace's
//! members sit at `<root>/wt.<branch-slug>/<workspace-slug>/<offset>`. A
//! group folder is recognised by the marker file beside it,
//! `<root>/wt.<branch-slug>/<slug>.rt-group`, and each one is a *group* —
//! one row in the management modal.
//!
//! Folders from older layouts still list and delete. Unmarked content of a
//! `wt.<branch-slug>/` folder (the anchor layout,
//! `<root>/wt.<branch-slug>/<sanitized-anchor>/<rel-to-anchor>`) forms one
//! group for that `wt.` folder, and the earlier buried layout
//! `<root>/<sanitized-anchor>/wt.<branch-slug>/<member>` one group per buried
//! `wt.` folder. Existing worktrees keep their paths and are reused by
//! spawns, so these groups disappear only as they are deleted.

use anyhow::{Context as _, anyhow};
use protocol::{
    PinnedMemberWorktree, RootWorktreeEntry, RootWorktreeMember, RootWorktreeStatus,
    SessionSnapshot, SessionStatus, WorktreeLaunchTarget,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

use crate::paths::{normalize_path_key, simplify_path};
use crate::session::SessionRegistry;
use crate::state::AppState;

/// The repo and workspace registries, flattened into the lookups launch
/// resolution needs: originating repo path → repo id, and the workspaces each
/// repo belongs to.
struct LaunchIndex {
    repo_by_path: HashMap<String, String>,
    /// `(workspace_id, name, member_repo_ids)`, in registry order.
    workspaces: Vec<(String, String, Vec<String>)>,
}

impl LaunchIndex {
    fn build(state: &AppState) -> Self {
        state.with_persisted(|s| Self {
            repo_by_path: s
                .repos
                .iter()
                .map(|r| (normalize_path_key(&r.path), r.id.clone()))
                .collect(),
            workspaces: s
                .workspaces
                .iter()
                .map(|w| (w.id.clone(), w.name.clone(), w.member_repo_ids.clone()))
                .collect(),
        })
    }
}

/// Walk the worktrees root and return one [`RootWorktreeEntry`] per
/// group found (see [`walk_for_wt_dirs`]), cross-referenced against the live and
/// abandoned session registries and resolved to a launch target where the
/// originating repos are still registered. Best-effort: I/O failures inside
/// the walk are logged and skipped, never propagated.
pub fn scan_root(
    root: &Path,
    sessions: &SessionRegistry,
    state: &AppState,
) -> Vec<RootWorktreeEntry> {
    let snapshots = sessions.snapshots();
    let xref = build_session_xref(&snapshots);
    let index = LaunchIndex::build(state);

    let mut entries: Vec<RootWorktreeEntry> = Vec::new();
    if !root.exists() {
        return entries;
    }
    walk_for_wt_dirs(root, root, &xref, &index, &mut entries, 0);

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
/// Layouts read:
/// - **Group layout** (current): `wt.<branch-slug>/` sits at depth 0 of the
///   worktrees root and holds one folder per repo or workspace, each marked
///   by a `<name>.rt-group` file beside it. Each marked folder is its own
///   entry, labelled with the marker's name (see [`scan_wt_dir`]).
/// - **Anchor layout** (older): also at depth 0, with members nested under
///   the sanitized anchor. Unmarked children of the `wt.<slug>` folder are
///   searched for member dirs (any directory containing a `.git` file or
///   subdir) and form one entry for the `wt.` folder, its displayed anchor
///   the longest common path-prefix of those members' parents.
/// - **Buried layout** (oldest): `wt.<branch-slug>/` was buried
///   under the sanitized anchor (e.g., `<root>/X/dev/wt.foo/repo`). When
///   the walker matches `wt.<slug>` at depth ≥1 it treats the wt dir's
///   direct children as members and derives the anchor from the path
///   between the root and the wt dir's parent.
fn walk_for_wt_dirs(
    root: &Path,
    cur: &Path,
    xref: &HashMap<PathBuf, (String, bool)>,
    index: &LaunchIndex,
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
            if depth == 0 {
                scan_wt_dir(&path, branch_slug, xref, index, entries);
            } else {
                // Buried layout: anchor is path between root and wt's parent;
                // members are direct children of the wt dir.
                let anchor = path
                    .parent()
                    .and_then(|p| p.strip_prefix(root).ok())
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                let members = direct_child_dirs(&path);
                entries.push(build_entry(
                    &path,
                    &anchor,
                    branch_slug,
                    &members,
                    None,
                    xref,
                    index,
                ));
            }
            // Don't descend INTO a wt dir — its children are member
            // worktrees, not nested groups.
            continue;
        }
        walk_for_wt_dirs(root, &path, xref, index, entries, depth + 1);
    }
}

/// List one depth-0 `wt.<slug>/` folder: each child that is a marked group
/// folder is an entry of its own, labelled with the marker's name; any
/// other children (the anchor layout) form one entry for the `wt.<slug>/`
/// folder itself, as does a folder with no marked groups at all.
fn scan_wt_dir(
    wt_path: &Path,
    branch_slug: &str,
    xref: &HashMap<PathBuf, (String, bool)>,
    index: &LaunchIndex,
    entries: &mut Vec<RootWorktreeEntry>,
) {
    let (groups, rest): (Vec<PathBuf>, Vec<PathBuf>) = direct_child_dirs(wt_path)
        .into_iter()
        .partition(|child| is_group_dir(child));
    for group in &groups {
        let members = group_member_dirs(group);
        let marker = read_group_marker(group);
        entries.push(build_entry(
            group,
            &group_label(group, marker.as_ref()),
            branch_slug,
            &members,
            marker.as_ref(),
            xref,
            index,
        ));
    }
    if groups.is_empty() || !rest.is_empty() {
        let members = members_under(&rest);
        let anchor = anchor_from_member_parents(&members, wt_path);
        entries.push(build_entry(
            wt_path,
            &anchor,
            branch_slug,
            &members,
            None,
            xref,
            index,
        ));
    }
}

/// Suffix of the marker file written beside a group folder:
/// `wt.<slug>/<name>.rt-group` marks `wt.<slug>/<name>/` as a group.
const GROUP_MARKER_SUFFIX: &str = ".rt-group";

/// What a group folder is named after, as recorded in its marker.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GroupMarker {
    pub kind: crate::git::GroupKind,
    pub name: String,
}

/// The marker path beside `group_dir`. It sits beside rather than inside
/// because a single repo's group folder is the git working tree itself.
fn marker_path(group_dir: &Path) -> Option<PathBuf> {
    let mut name = group_dir.file_name()?.to_os_string();
    name.push(GROUP_MARKER_SUFFIX);
    Some(group_dir.with_file_name(name))
}

/// Whether `dir` is a group folder: a child of a `wt.*` folder with a marker
/// beside it.
fn is_group_dir(dir: &Path) -> bool {
    let under_wt = dir
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("wt."));
    under_wt && marker_path(dir).is_some_and(|m| m.is_file())
}

/// The marker beside `group_dir`, when one is there and parses. An
/// unreadable marker is logged and reads as absent.
fn read_group_marker(group_dir: &Path) -> Option<GroupMarker> {
    let marker = marker_path(group_dir)?;
    let parsed = std::fs::read_to_string(&marker)
        .map_err(anyhow::Error::from)
        .and_then(|text| serde_json::from_str::<GroupMarker>(&text).map_err(anyhow::Error::from));
    match parsed {
        Ok(m) => Some(m),
        Err(err) => {
            warn!(?err, marker = %marker.display(), "unreadable group marker; labelling by folder name");
            None
        }
    }
}

/// The name a group folder's marker records, or the folder's own name when
/// the marker is missing or unreadable.
fn group_label(group_dir: &Path, marker: Option<&GroupMarker>) -> String {
    marker.map_or_else(
        || {
            group_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        },
        |m| m.name.clone(),
    )
}

/// Write the marker beside `group_dir`, unless one recording the same kind
/// and name is already there. A marker that differs (the group was renamed,
/// or the folder now belongs to the other kind) is rewritten.
pub fn write_group_marker(
    group_dir: &Path,
    kind: crate::git::GroupKind,
    name: &str,
) -> anyhow::Result<()> {
    let marker = marker_path(group_dir)
        .ok_or_else(|| anyhow!("group folder has no name: {}", group_dir.display()))?;
    let wanted = GroupMarker {
        kind,
        name: name.to_string(),
    };
    let current = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|text| serde_json::from_str::<GroupMarker>(&text).ok());
    if current.as_ref() == Some(&wanted) {
        return Ok(());
    }
    let body = serde_json::to_string(&wanted)?;
    std::fs::write(&marker, body)
        .with_context(|| format!("writing group marker {}", marker.display()))
}

/// Mark `group_dir` as a group when any of `worktrees` sits in it, so the
/// scanner lists it on its own. A failure is logged rather than raised:
/// unmarked, the group still lists under its `wt.<branch>` folder.
pub fn mark_group(group_dir: &Path, kind: crate::git::GroupKind, name: &str, worktrees: &[&Path]) {
    if !worktrees.iter().any(|w| w.starts_with(group_dir)) {
        return;
    }
    if let Err(err) = write_group_marker(group_dir, kind, name) {
        warn!(?err, group = %group_dir.display(), "could not write the worktree group marker");
    }
}

/// In a `wt.*` folder, remove each group marker whose group folder is gone.
/// Anything else, `dir` included, is left alone. Best-effort: a failure is
/// logged.
pub fn remove_orphan_group_markers(dir: &Path) {
    let is_wt = dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("wt."));
    if !is_wt {
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!(?err, dir = %dir.display(), "could not list the wt folder for stale markers");
            }
            return;
        }
    };
    for ent in rd.flatten() {
        if !ent.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let path = ent.path();
        let Some(group_name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(GROUP_MARKER_SUFFIX))
        else {
            continue;
        };
        if dir.join(group_name).exists() {
            continue;
        }
        if let Err(err) = std::fs::remove_file(&path) {
            warn!(?err, marker = %path.display(), "could not remove a stale group marker");
        }
    }
}

/// Member worktrees of a group folder: the folder itself when it holds a
/// `.git` of any kind (a single repo's group, or a folder a clone was made
/// in), plus every linked worktree found inside it — a member nested in
/// another member counts as one of its own.
fn group_member_dirs(group_dir: &Path) -> Vec<PathBuf> {
    let mut members: Vec<PathBuf> = Vec::new();
    if group_dir.join(".git").exists() {
        members.push(group_dir.to_path_buf());
    }
    members.extend(find_member_dirs(group_dir));
    members
}

/// Whether `dir` is a linked worktree: its `.git` is a *file* naming a
/// `<repo>/.git/worktrees/<name>` admin directory, the shape `git worktree
/// add` leaves. A `.git` directory (a plain clone) and a gitfile pointing
/// into `<repo>/.git/modules/…` (a submodule, a worktree's own
/// `…/worktrees/<wt>/modules/…` included) are not linked worktrees.
fn is_linked_worktree(dir: &Path) -> bool {
    if !dir.join(".git").is_file() {
        return false;
    }
    let Some(gitdir) = gitdir_for_worktree(dir) else {
        return false;
    };
    let mut components = gitdir.components().rev();
    components.next().is_some()
        && component_is(components.next(), "worktrees")
        && component_is(components.next(), ".git")
}

/// Whether a path component spells `want`, compared without case on Windows
/// (git may write `.GIT` or `Worktrees` into a gitfile there).
fn component_is(component: Option<std::path::Component<'_>>, want: &str) -> bool {
    component.is_some_and(|c| {
        let text = c.as_os_str().to_string_lossy();
        if cfg!(windows) {
            text.eq_ignore_ascii_case(want)
        } else {
            text.as_ref() == want
        }
    })
}

/// Member worktrees at or below each of `dirs`.
fn members_under(dirs: &[PathBuf]) -> Vec<PathBuf> {
    dirs.iter().flat_map(|d| group_member_dirs(d)).collect()
}

/// Walk inside a group or anchor-layout directory and return every linked
/// worktree found, a member nested inside another included. Only a linked
/// worktree counts (see [`is_linked_worktree`]), so a submodule or a vendored
/// clone inside a member is not listed — descent still continues through it,
/// so a real member deeper down is found either way. Descent never enters a
/// `.git` entry. Best-effort — I/O failures inside the walk are silently
/// skipped.
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
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if is_linked_worktree(&path) {
                out.push(path.clone());
            }
            stack.push((path, depth + 1));
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

/// Delete a group from disk: a marked group folder (with its marker), or a
/// `wt.<branch>/` folder's unmarked content. Refuses if any member is
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
    if is_group_dir(&target) {
        assert_no_live_session(&target, &[], sessions)?;
        delete_members(&group_member_dirs(&target)).await?;
        finalize_group_dir(&target)?;
        remove_group_marker_and_empty_parent(&target);
        return Ok(());
    }
    // A `wt.<slug>/` folder: its marked groups are entries of their own and
    // stay; only the rest of its content is this entry's to delete.
    let (groups, rest): (Vec<PathBuf>, Vec<PathBuf>) = direct_child_dirs(&target)
        .into_iter()
        .partition(|child| is_group_dir(child));
    assert_no_live_session(&target, &groups, sessions)?;
    delete_members(&members_under(&rest)).await?;
    if groups.is_empty() {
        return finalize_group_dir(&target);
    }
    for leftover in &rest {
        finalize_group_dir(leftover)?;
    }
    Ok(())
}

/// Remove the marker beside a deleted group folder, then its `wt.<slug>/`
/// parent if nothing else is left in it. Best-effort: a failure is logged.
fn remove_group_marker_and_empty_parent(group_dir: &Path) {
    if let Some(marker) = marker_path(group_dir)
        && let Err(err) = std::fs::remove_file(&marker)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        warn!(?err, marker = %marker.display(), "could not remove the group marker");
    }
    if let Some(parent) = group_dir.parent()
        && std::fs::read_dir(parent).is_ok_and(|mut rd| rd.next().is_none())
        && let Err(err) = std::fs::remove_dir(parent)
    {
        warn!(?err, dir = %parent.display(), "could not remove the empty wt folder");
    }
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
    if !wt_name.starts_with("wt.") && !is_group_dir(&target) {
        return Err(anyhow!(
            "refusing to delete {}: not a wt.<branch> directory or a worktree group folder",
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
/// Paths under any of `kept` (group folders the delete leaves alone) don't
/// count.
fn assert_no_live_session(
    target: &Path,
    kept: &[PathBuf],
    sessions: &SessionRegistry,
) -> anyhow::Result<()> {
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
            if mp_canon.starts_with(target) && !kept.iter().any(|k| mp_canon.starts_with(k)) {
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
/// `git worktree prune` after deletion. The caller lists the members, so a
/// group folder that is itself a worktree and members nested under an
/// anchor are handled alike.
async fn delete_members(member_paths: &[PathBuf]) -> anyhow::Result<()> {
    for member_path in &members_deepest_first(member_paths) {
        let repo_path = repo_path_for_worktree(member_path);
        if let Some(repo) = repo_path.as_deref() {
            match crate::worktree_cleanup::remove_member(repo, member_path).await {
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
            std::fs::remove_dir_all(member_path)
                .or_else(|err| match err.kind() {
                    std::io::ErrorKind::NotFound => Ok(()),
                    _ => Err(err),
                })
                .with_context(|| format!("removing member dir {}", member_path.display()))?;
        }
    }
    Ok(())
}

/// The members in removal order: deepest first, so a member nested inside
/// another gets its own `git worktree remove`/prune before the folder
/// holding it is removed wholesale.
fn members_deepest_first(member_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut ordered: Vec<PathBuf> = member_paths.to_vec();
    ordered.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    ordered
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

/// The group entry `member_path` belongs to: the nearest marked group
/// folder at or above it (a single repo's group folder is the member
/// itself), else the nearest ancestor whose name starts with `wt.` (the
/// anchor layouts). `None` when neither exists.
fn wt_dir_for_member(member_path: &Path) -> Option<PathBuf> {
    for dir in member_path.ancestors() {
        if is_group_dir(dir) {
            return Some(dir.to_path_buf());
        }
        let is_wt = dir
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.starts_with("wt."));
        if is_wt && dir != member_path {
            return Some(dir.to_path_buf());
        }
    }
    None
}

/// Branch checked out in a member worktree, read straight from the worktree's
/// admin `HEAD` file rather than by shelling out to git — `scan_root` is a
/// synchronous disk walk and a group can have many members.
///
/// Returns `None` for a detached HEAD, an unreadable gitfile, or an
/// unreachable admin directory.
fn head_branch_for_worktree(worktree: &Path) -> Option<String> {
    let gitdir = gitdir_for_worktree(worktree)?;
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    (!branch.is_empty()).then(|| branch.to_string())
}

/// Read the `gitdir:` line out of a worktree's `.git` file — i.e.
/// `<repo>/.git/worktrees/<name>`.
fn gitdir_for_worktree(worktree: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(worktree.join(".git")).ok()?;
    let line = contents.lines().find(|l| l.starts_with("gitdir:"))?;
    Some(PathBuf::from(line.strip_prefix("gitdir:")?.trim()))
}

/// Map a group's members onto something spawnable: a registered repo for a
/// single member, or the workspace containing them all. Returns the target,
/// or the reason a "launch here" button should stay disabled.
///
/// Member directories whose originating repo isn't registered are dropped
/// rather than blocking the whole group — a workspace whose other members
/// still resolve stays launchable, and the dropped member gets a fresh
/// worktree in the same group at spawn time.
///
/// A group whose marker says it is a workspace launches as that workspace
/// even with one pinned member, so the workspace's other members get fresh
/// worktrees at spawn time instead of quietly spawning a lone repo.
fn resolve_launch(
    members: &[RootWorktreeMember],
    index: &LaunchIndex,
    marker: Option<&GroupMarker>,
) -> (Option<WorktreeLaunchTarget>, Option<String>) {
    if members.is_empty() {
        return (
            None,
            Some("this group has no member worktrees on disk".to_string()),
        );
    }
    let pins: Vec<PinnedMemberWorktree> = members
        .iter()
        .filter_map(|m| {
            let repo_path = m.repo_path.as_deref()?;
            let repo_id = index.repo_by_path.get(&normalize_path_key(repo_path))?;
            Some(PinnedMemberWorktree {
                repo_id: repo_id.clone(),
                path: m.worktree_path.clone(),
            })
        })
        .collect();

    let Some(first) = pins.first() else {
        let unreachable = members.iter().all(|m| m.repo_path.is_none());
        let reason = if unreachable {
            "the originating repo is no longer on disk".to_string()
        } else {
            let names: Vec<&str> = members
                .iter()
                .filter_map(|m| m.repo_path.as_deref())
                .collect();
            format!("not registered in rustling-tulip: {}", names.join(", "))
        };
        return (None, Some(reason));
    };

    let branch = head_branch_for_worktree(Path::new(&first.path));
    let workspace_marker = marker.filter(|m| m.kind == crate::git::GroupKind::Workspace);
    if workspace_marker.is_none() && pins.len() == 1 {
        return (
            Some(WorktreeLaunchTarget::Single {
                repo_id: first.repo_id.clone(),
                branch,
                worktree_path: first.path.clone(),
            }),
            None,
        );
    }

    // The workspace to launch: the one the group folder is named after, else
    // the tightest fit that holds every pin, so a broad "everything"
    // workspace doesn't shadow the specific one. Either way the workspace
    // must have every pin.
    let covers = |member_ids: &[String]| pins.iter().all(|p| member_ids.contains(&p.repo_id));
    let named = workspace_marker.and_then(|m| {
        let wanted = crate::git::name_slug(&m.name);
        index.workspaces.iter().find(|(_, name, member_ids)| {
            crate::git::name_slug(name) == wanted && covers(member_ids)
        })
    });
    let workspace = named.or_else(|| {
        index
            .workspaces
            .iter()
            .filter(|(_, _, member_ids)| covers(member_ids))
            .min_by_key(|(_, _, member_ids)| member_ids.len())
    });
    match workspace {
        Some((workspace_id, _, _)) => (
            Some(WorktreeLaunchTarget::Workspace {
                workspace_id: workspace_id.clone(),
                branch,
                members: pins,
            }),
            None,
        ),
        // The workspace this group was made for is gone from the registry:
        // with a single pin the group still spawns the repo it holds.
        None if workspace_marker.is_some() && pins.len() == 1 => (
            Some(WorktreeLaunchTarget::Single {
                repo_id: first.repo_id.clone(),
                branch,
                worktree_path: first.path.clone(),
            }),
            None,
        ),
        None => (
            None,
            Some("no workspace contains all of this group's repos".to_string()),
        ),
    }
}

fn build_entry(
    wt_path: &Path,
    anchor_name: &str,
    branch_slug: &str,
    member_paths: &[PathBuf],
    marker: Option<&GroupMarker>,
    xref: &HashMap<PathBuf, (String, bool)>,
    index: &LaunchIndex,
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
    let (launch, launch_blocked_reason) = resolve_launch(&members, index, marker);

    RootWorktreeEntry {
        path: wt_path.to_string_lossy().into_owned(),
        anchor: anchor_name.to_owned(),
        branch_slug: branch_slug.to_owned(),
        members,
        status,
        session_id,
        size_bytes,
        last_modified_unix,
        launch,
        launch_blocked_reason,
    }
}

/// True iff this session is currently running a process — i.e., deleting
/// its worktree would yank the rug out from under live work. Orphan
/// (detached but process still alive) counts as live for safety.
///
/// Shared with the spawn dialog's worktree picker so "Active" means the same
/// thing in both places.
pub fn is_session_live(snap: &SessionSnapshot) -> bool {
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
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    clippy::cast_precision_loss,
    clippy::map_unwrap_or,
    reason = "tests assert preconditions with unwrap/expect and panic on an \
              unexpected enum variant; the live-disk probe prints scan results \
              to stderr so the operator can read them"
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

    fn test_dirs(tmp: &std::path::Path) -> crate::paths::Dirs {
        crate::paths::Dirs {
            config: tmp.to_path_buf(),
            state_file: tmp.join("state.json"),
            handshake_file: tmp.join("daemon.json"),
            lan_config_file: tmp.join("lan.json"),
            lan_cert_file: tmp.join("lan-cert.pem"),
            lan_key_file: tmp.join("lan-key.pem"),
            sessions_dir: tmp.join("sessions"),
            worktrees_dir: tmp.join("worktrees"),
            binaries_dir: tmp.join("binaries"),
        }
    }

    /// Build an empty `SessionRegistry` pointed at a temp config dir.
    /// The xref it produces is empty, so every wt group `scan_root`
    /// returns shows `RootWorktreeStatus::Stale` — useful for tests
    /// that just want to verify the walker reaches the right dirs.
    fn empty_registry(tmp: &std::path::Path) -> Arc<SessionRegistry> {
        SessionRegistry::new(test_dirs(tmp))
    }

    /// Empty repo/workspace registry — every group resolves to no launch
    /// target, which is what the walker-shape tests want.
    fn empty_state(tmp: &std::path::Path) -> AppState {
        AppState::load_or_default(&test_dirs(tmp)).unwrap()
    }

    fn touch(p: &std::path::Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"").unwrap();
    }

    fn member(worktree_path: &str, repo_path: Option<&str>) -> RootWorktreeMember {
        RootWorktreeMember {
            worktree_path: worktree_path.to_string(),
            repo_path: repo_path.map(str::to_string),
            repo_name_hint: "hint".to_string(),
        }
    }

    fn index(repos: &[(&str, &str)], workspaces: &[(&str, &str, &[&str])]) -> LaunchIndex {
        LaunchIndex {
            repo_by_path: repos
                .iter()
                .map(|(path, id)| (normalize_path_key(path), (*id).to_string()))
                .collect(),
            workspaces: workspaces
                .iter()
                .map(|(id, name, members)| {
                    (
                        (*id).to_string(),
                        (*name).to_string(),
                        members.iter().map(|m| (*m).to_string()).collect(),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn resolve_launch_maps_a_lone_member_to_its_repo() {
        let members = vec![member("X:/wt/wt.foo/X/dev/repo1", Some("X:/dev/repo1"))];
        let (target, blocked) =
            resolve_launch(&members, &index(&[("X:/dev/repo1", "r1")], &[]), None);
        assert!(blocked.is_none(), "unexpected block: {blocked:?}");
        match target.expect("a registered single member must resolve") {
            WorktreeLaunchTarget::Single {
                repo_id,
                worktree_path,
                ..
            } => {
                assert_eq!(repo_id, "r1");
                assert_eq!(worktree_path, "X:/wt/wt.foo/X/dev/repo1");
            }
            other => panic!("expected a single target, got {other:?}"),
        }
    }

    #[test]
    fn resolve_launch_blocks_an_unregistered_repo() {
        // The repo is on disk but was removed from the registry: there's no
        // repo_id to build a spawn target from, so the button stays disabled
        // with a reason rather than silently registering anything.
        let members = vec![member("X:/wt/wt.foo/X/dev/repo1", Some("X:/dev/repo1"))];
        let (target, blocked) = resolve_launch(&members, &index(&[], &[]), None);
        assert!(target.is_none());
        let reason = blocked.expect("an unregistered repo must explain itself");
        assert!(reason.contains("not registered"), "unexpected: {reason}");
    }

    #[test]
    fn resolve_launch_blocks_when_the_repo_is_gone_from_disk() {
        let members = vec![member("X:/wt/wt.foo/X/dev/repo1", None)];
        let (target, blocked) = resolve_launch(&members, &index(&[], &[]), None);
        assert!(target.is_none());
        let reason = blocked.expect("an unreachable repo must explain itself");
        assert!(reason.contains("no longer on disk"), "unexpected: {reason}");
    }

    #[test]
    fn resolve_launch_maps_several_members_to_their_workspace() {
        let members = vec![
            member("X:/wt/wt.foo/X/dev/api", Some("X:/dev/api")),
            member("X:/wt/wt.foo/X/dev/web", Some("X:/dev/web")),
        ];
        let idx = index(
            &[("X:/dev/api", "r-api"), ("X:/dev/web", "r-web")],
            &[("ws1", "Shop", &["r-api", "r-web"])],
        );
        let (target, blocked) = resolve_launch(&members, &idx, None);
        assert!(blocked.is_none(), "unexpected block: {blocked:?}");
        match target.expect("a multi-member group must resolve to a workspace") {
            WorktreeLaunchTarget::Workspace {
                workspace_id,
                members,
                ..
            } => {
                assert_eq!(workspace_id, "ws1");
                assert_eq!(members.len(), 2);
            }
            other => panic!("expected a workspace target, got {other:?}"),
        }
    }

    #[test]
    fn resolve_launch_prefers_the_tightest_workspace() {
        // Two workspaces contain both repos; the specific one should win over
        // an "everything" workspace that merely happens to include them.
        let members = vec![
            member("X:/wt/wt.foo/X/dev/api", Some("X:/dev/api")),
            member("X:/wt/wt.foo/X/dev/web", Some("X:/dev/web")),
        ];
        let idx = index(
            &[("X:/dev/api", "r-api"), ("X:/dev/web", "r-web")],
            &[
                ("everything", "Everything", &["r-api", "r-web", "r-docs"]),
                ("ws-pair", "Pair", &["r-api", "r-web"]),
            ],
        );
        let (target, _) = resolve_launch(&members, &idx, None);
        match target.expect("must resolve") {
            WorktreeLaunchTarget::Workspace { workspace_id, .. } => {
                assert_eq!(workspace_id, "ws-pair");
            }
            other => panic!("expected a workspace target, got {other:?}"),
        }
    }

    #[test]
    fn resolve_launch_blocks_members_no_workspace_covers() {
        let members = vec![
            member("X:/wt/wt.foo/X/dev/api", Some("X:/dev/api")),
            member("X:/wt/wt.foo/X/dev/web", Some("X:/dev/web")),
        ];
        let idx = index(
            &[("X:/dev/api", "r-api"), ("X:/dev/web", "r-web")],
            &[("ws1", "Shop", &["r-api"])],
        );
        let (target, blocked) = resolve_launch(&members, &idx, None);
        assert!(target.is_none());
        assert!(
            blocked
                .expect("must explain itself")
                .contains("no workspace")
        );
    }

    #[test]
    fn resolve_launch_drops_an_unresolvable_member_when_others_resolve() {
        // A workspace member whose repo was unregistered shouldn't sink the
        // whole group — the remaining members pin, and the dropped one gets a
        // fresh worktree in the same group at spawn time.
        let members = vec![
            member("X:/wt/wt.foo/X/dev/api", Some("X:/dev/api")),
            member("X:/wt/wt.foo/X/dev/web", Some("X:/dev/web")),
            member("X:/wt/wt.foo/X/dev/gone", Some("X:/dev/gone")),
        ];
        let idx = index(
            &[("X:/dev/api", "r-api"), ("X:/dev/web", "r-web")],
            &[("ws1", "Shop", &["r-api", "r-web", "r-docs"])],
        );
        let (target, blocked) = resolve_launch(&members, &idx, None);
        assert!(blocked.is_none(), "unexpected block: {blocked:?}");
        match target.expect("must resolve from the surviving members") {
            WorktreeLaunchTarget::Workspace { members, .. } => assert_eq!(members.len(), 2),
            other => panic!("expected a workspace target, got {other:?}"),
        }
    }

    #[test]
    fn resolve_launch_blocks_an_empty_group() {
        let (target, blocked) = resolve_launch(&[], &index(&[], &[]), None);
        assert!(target.is_none());
        assert!(blocked.expect("must explain itself").contains("no member"));
    }

    #[test]
    fn resolve_launch_keeps_a_repo_marker_single_with_one_pin() {
        // A repo-kind marker (like no marker at all) keeps today's behaviour:
        // one pinned member spawns its repo, never a workspace.
        let members = vec![member("X:/wt/wt.foo/X/dev/repo1", Some("X:/dev/repo1"))];
        let idx = index(&[("X:/dev/repo1", "r1")], &[("ws1", "Shop", &["r1"])]);
        let marker = GroupMarker {
            kind: crate::git::GroupKind::Repo,
            name: "Shop".to_string(),
        };

        let (target, blocked) = resolve_launch(&members, &idx, Some(&marker));

        assert!(blocked.is_none(), "unexpected block: {blocked:?}");
        assert!(
            matches!(target, Some(WorktreeLaunchTarget::Single { .. })),
            "expected a single target, got {target:?}"
        );
    }

    #[test]
    fn one_member_workspace_group_launches_as_workspace() {
        // A workspace group holding one member's worktree still launches as
        // the workspace, so its other members get fresh worktrees.
        let cfg_tmp = Scratch::new();
        let web = cfg_tmp.path().join("web");
        let admin = web.join(".git").join("worktrees").join("w1");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let state = serde_json::json!({
            "repos": [{
                "id": "r-web",
                "name": "web",
                "path": web.to_string_lossy(),
                "default_branch": null,
            }],
            "workspaces": [{
                "id": "ws",
                "name": "Shop",
                "member_repo_ids": ["r-web", "r-api"],
            }],
        });
        std::fs::write(
            cfg_tmp.path().join("state.json"),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();

        let tmp = Scratch::new();
        let root = tmp.path().join("worktrees");
        let wt = root.join("wt.feat");
        let shop = seed_group(
            &wt,
            "shop",
            crate::git::GroupKind::Workspace,
            "Shop",
            &["web"],
        );
        std::fs::write(
            shop.join("web").join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();

        let entries = scan_root(
            &root,
            &empty_registry(cfg_tmp.path()),
            &empty_state(cfg_tmp.path()),
        );

        let launch = entry_for(&entries, &shop)
            .launch
            .clone()
            .expect("a registered member must resolve");
        match launch {
            WorktreeLaunchTarget::Workspace {
                workspace_id,
                members,
                ..
            } => {
                assert_eq!(workspace_id, "ws");
                assert_eq!(members.len(), 1);
            }
            other => panic!("expected a workspace target, got {other:?}"),
        }
    }

    #[test]
    fn head_branch_reads_the_worktree_admin_head() {
        // The picker labels a pinned worktree with the branch it's really on,
        // which lives in <repo>/.git/worktrees/<name>/HEAD — not in the
        // group's directory name, which only ever records the branch the
        // worktree was created for.
        let tmp = Scratch::new();
        let worktree = tmp.path().join("wt.old-name").join("repo");
        let admin = tmp
            .path()
            .join("repo")
            .join(".git")
            .join("worktrees")
            .join("w1");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/feature/renamed\n").unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();

        assert_eq!(
            head_branch_for_worktree(&worktree).as_deref(),
            Some("feature/renamed")
        );
    }

    #[test]
    fn head_branch_is_none_for_a_detached_worktree() {
        let tmp = Scratch::new();
        let worktree = tmp.path().join("wt.x").join("repo");
        let admin = tmp
            .path()
            .join("repo")
            .join(".git")
            .join("worktrees")
            .join("w1");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            admin.join("HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();

        assert!(head_branch_for_worktree(&worktree).is_none());
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
        let state = empty_state(cfg_tmp.path());
        let entries = scan_root(&root, &sessions, &state);

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
        let state = empty_state(cfg_tmp.path());
        let entries = scan_root(&root, &sessions, &state);

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
        let state = empty_state(cfg_tmp.path());
        let entries = scan_root(&root, &sessions, &state);

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
        linked_worktree(&member);

        let cfg_tmp = Scratch::new();
        let sessions = empty_registry(cfg_tmp.path());
        let state = empty_state(cfg_tmp.path());
        let entries = scan_root(&root, &sessions, &state);

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
        linked_worktree(&wt_dir.join("X").join("dev").join("yaat"));
        linked_worktree(&wt_dir.join("X").join("dev").join("yaat-server"));

        let cfg_tmp = Scratch::new();
        let sessions = empty_registry(cfg_tmp.path());
        let state = empty_state(cfg_tmp.path());
        let entries = scan_root(&root, &sessions, &state);

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
        linked_worktree(&root.join("wt.alpha").join("X").join("dev").join("repo"));
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
        let state = empty_state(cfg_tmp.path());
        let entries = scan_root(&root, &sessions, &state);

        assert_eq!(entries.len(), 2);
        let pairs: Vec<(&str, &str)> = entries
            .iter()
            .map(|e| (e.anchor.as_str(), e.branch_slug.as_str()))
            .collect();
        // Sorted by anchor asc.
        assert_eq!(pairs, vec![("X/dev", "alpha"), ("Y/dev", "beta")]);
    }

    /// Write `dir/.git` as a linked worktree's gitfile — the shape a real
    /// spawn leaves behind — naming a repo root that does not exist, so no
    /// fixture shells out to git.
    fn linked_worktree(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        let gitdir = dir
            .join("..")
            .join("missing-repo")
            .join(".git")
            .join("worktrees")
            .join("w1");
        std::fs::write(dir.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
    }

    /// Seed `<wt>/<folder>` as a marked group whose members are the given
    /// relative paths as linked worktrees (an empty relative path makes the
    /// folder itself the worktree, as for a single repo).
    fn seed_group(
        wt: &std::path::Path,
        folder: &str,
        kind: crate::git::GroupKind,
        name: &str,
        members: &[&str],
    ) -> PathBuf {
        let group = wt.join(folder);
        for member in members {
            linked_worktree(&group.join(member));
        }
        write_group_marker(&group, kind, name).unwrap();
        group
    }

    fn entry_for<'a>(
        entries: &'a [RootWorktreeEntry],
        path: &std::path::Path,
    ) -> &'a RootWorktreeEntry {
        let key = normalize_path_key(&path.to_string_lossy());
        entries
            .iter()
            .find(|e| normalize_path_key(&e.path) == key)
            .unwrap_or_else(|| panic!("no entry for {}: {entries:?}", path.display()))
    }

    #[test]
    fn stale_marker_is_rewritten() {
        let tmp = Scratch::new();
        let group = tmp.path().join("wt.feat").join("shop");
        std::fs::create_dir_all(&group).unwrap();
        write_group_marker(&group, crate::git::GroupKind::Repo, "Shop").unwrap();

        write_group_marker(&group, crate::git::GroupKind::Workspace, "SHOP").unwrap();

        let marker = tmp.path().join("wt.feat").join("shop.rt-group");
        let parsed: GroupMarker =
            serde_json::from_str(&std::fs::read_to_string(&marker).unwrap()).unwrap();
        assert_eq!(
            parsed,
            GroupMarker {
                kind: crate::git::GroupKind::Workspace,
                name: "SHOP".to_string(),
            }
        );
    }

    #[test]
    fn orphan_marker_sweep_skips_directories() {
        let tmp = Scratch::new();
        let wt = tmp.path().join("wt.feat");
        // A folder whose name ends like a marker, with no group beside it.
        let dir = wt.join("odd.rt-group");
        touch(&dir.join("keep.txt"));
        let stale = wt.join("gone.rt-group");
        std::fs::write(&stale, "{}").unwrap();

        remove_orphan_group_markers(&wt);

        assert!(dir.join("keep.txt").is_file(), "a directory is left alone");
        assert!(!stale.exists(), "a stale marker file is removed");
    }

    #[test]
    fn marker_is_written_beside_the_group_folder() {
        let tmp = Scratch::new();
        let wt = tmp.path().join("wt.feat");
        let group = wt.join("app");
        std::fs::create_dir_all(&group).unwrap();

        mark_group(
            &group,
            crate::git::GroupKind::Repo,
            "App",
            &[group.as_path()],
        );

        let marker = wt.join("app.rt-group");
        let parsed: GroupMarker =
            serde_json::from_str(&std::fs::read_to_string(&marker).unwrap()).unwrap();
        assert_eq!(
            parsed,
            GroupMarker {
                kind: crate::git::GroupKind::Repo,
                name: "App".to_string(),
            }
        );
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&marker).unwrap()).unwrap();
        assert_eq!(raw, serde_json::json!({"kind": "repo", "name": "App"}));
        assert_eq!(
            std::fs::read_dir(&group).unwrap().count(),
            0,
            "nothing is written inside the group folder"
        );

        // A worktree reused from elsewhere doesn't mark a group it isn't in.
        let other = wt.join("shop");
        let elsewhere = tmp.path().join("elsewhere");
        mark_group(
            &other,
            crate::git::GroupKind::Workspace,
            "Shop",
            &[elsewhere.as_path()],
        );
        assert!(!wt.join("shop.rt-group").exists());
    }

    #[test]
    fn scanner_lists_each_new_layout_group_separately() {
        let tmp = Scratch::new();
        let root = tmp.path().join("worktrees");
        let wt = root.join("wt.feat");
        let app = seed_group(&wt, "app", crate::git::GroupKind::Repo, "App", &[""]);
        let shop = seed_group(
            &wt,
            "shop",
            crate::git::GroupKind::Workspace,
            "Shop",
            &["apps/web", "libs/core"],
        );

        let cfg_tmp = Scratch::new();
        let entries = scan_root(
            &root,
            &empty_registry(cfg_tmp.path()),
            &empty_state(cfg_tmp.path()),
        );

        assert_eq!(entries.len(), 2, "{entries:?}");
        let app_entry = entry_for(&entries, &app);
        assert_eq!(app_entry.anchor, "App");
        assert_eq!(app_entry.branch_slug, "feat");
        assert_eq!(app_entry.members.len(), 1);
        assert_eq!(
            normalize_path_key(&app_entry.members[0].worktree_path),
            normalize_path_key(&app.to_string_lossy())
        );
        let shop_entry = entry_for(&entries, &shop);
        assert_eq!(shop_entry.anchor, "Shop");
        assert_eq!(shop_entry.members.len(), 2);
    }

    #[test]
    fn scanner_keeps_old_layout_group_under_wt_folder() {
        let tmp = Scratch::new();
        let root = tmp.path().join("worktrees");
        let wt = root.join("wt.feat");
        linked_worktree(&wt.join("X").join("dev").join("repo"));
        let app = seed_group(&wt, "app", crate::git::GroupKind::Repo, "App", &[""]);

        let cfg_tmp = Scratch::new();
        let entries = scan_root(
            &root,
            &empty_registry(cfg_tmp.path()),
            &empty_state(cfg_tmp.path()),
        );

        assert_eq!(entries.len(), 2, "{entries:?}");
        let old = entry_for(&entries, &wt);
        assert_eq!(old.anchor, "X/dev");
        assert_eq!(
            old.members.len(),
            1,
            "the marked group is not an old-layout member"
        );
        assert_eq!(entry_for(&entries, &app).anchor, "App");
    }

    #[test]
    fn scanner_lists_a_member_nested_inside_another() {
        // A workspace member can hold another member's worktree: `repo1` and
        // `repo1/sub` are two members of the group, not one.
        let tmp = Scratch::new();
        let root = tmp.path().join("worktrees");
        let wt = root.join("wt.feat");
        let shop = seed_group(
            &wt,
            "shop",
            crate::git::GroupKind::Workspace,
            "Shop",
            &["repo1", "repo1/sub"],
        );

        let cfg_tmp = Scratch::new();
        let entries = scan_root(
            &root,
            &empty_registry(cfg_tmp.path()),
            &empty_state(cfg_tmp.path()),
        );

        let entry = entry_for(&entries, &shop);
        assert_eq!(entry.members.len(), 2, "{:?}", entry.members);
        let mut got: Vec<String> = entry
            .members
            .iter()
            .map(|m| normalize_path_key(&m.worktree_path))
            .collect();
        let mut want = vec![
            normalize_path_key(&shop.join("repo1").to_string_lossy()),
            normalize_path_key(&shop.join("repo1").join("sub").to_string_lossy()),
        ];
        got.sort();
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn scanner_ignores_a_submodule_inside_a_member() {
        // A submodule's `.git` file points into `…/modules/…`, not at a
        // worktree admin dir, so it is not a member of the group.
        let tmp = Scratch::new();
        let root = tmp.path().join("worktrees");
        let wt = root.join("wt.feat");
        let group = seed_group(
            &wt,
            "shop",
            crate::git::GroupKind::Workspace,
            "Shop",
            &["repo1"],
        );
        let module = group
            .join("repo1")
            .join("fake-repo")
            .join(".git")
            .join("worktrees")
            .join("w1")
            .join("modules")
            .join("lib");
        let sub = group.join("repo1").join("lib");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".git"), format!("gitdir: {}\n", module.display())).unwrap();

        let cfg_tmp = Scratch::new();
        let entries = scan_root(
            &root,
            &empty_registry(cfg_tmp.path()),
            &empty_state(cfg_tmp.path()),
        );

        let entry = entry_for(&entries, &group);
        assert_eq!(entry.members.len(), 1, "{:?}", entry.members);
        assert_eq!(
            normalize_path_key(&entry.members[0].worktree_path),
            normalize_path_key(&group.join("repo1").to_string_lossy())
        );
    }

    #[test]
    fn scanner_ignores_a_vendored_repo_inside_a_member() {
        // A plain clone inside a member worktree has a real `.git`
        // directory, not a worktree gitfile, so it is not a member either.
        let tmp = Scratch::new();
        let root = tmp.path().join("worktrees");
        let wt = root.join("wt.feat");
        let group = seed_group(
            &wt,
            "shop",
            crate::git::GroupKind::Workspace,
            "Shop",
            &["repo1"],
        );
        touch(&group.join("repo1").join("vendor").join(".git").join("HEAD"));

        let cfg_tmp = Scratch::new();
        let entries = scan_root(
            &root,
            &empty_registry(cfg_tmp.path()),
            &empty_state(cfg_tmp.path()),
        );

        let entry = entry_for(&entries, &group);
        assert_eq!(entry.members.len(), 1, "{:?}", entry.members);
        assert_eq!(
            normalize_path_key(&entry.members[0].worktree_path),
            normalize_path_key(&group.join("repo1").to_string_lossy())
        );
    }

    #[tokio::test]
    async fn delete_removes_nested_members_deepest_first() {
        // The nested member is removed before the folder holding it, so it is
        // removed as a member of its own rather than vanishing with its parent.
        let tmp = Scratch::new();
        let root = tmp.path().join("worktrees");
        let wt = root.join("wt.feat");
        let shop = seed_group(
            &wt,
            "shop",
            crate::git::GroupKind::Workspace,
            "Shop",
            &["repo1", "repo1/sub"],
        );
        let members = group_member_dirs(&shop);
        assert_eq!(members.len(), 2, "{members:?}");
        assert_eq!(
            members_deepest_first(&members),
            vec![shop.join("repo1").join("sub"), shop.join("repo1")],
            "the nested member is removed first"
        );

        let cfg_tmp = Scratch::new();
        delete_group(&root, &shop, &empty_registry(cfg_tmp.path()))
            .await
            .unwrap();

        assert!(!shop.join("repo1").join("sub").exists());
        assert!(!shop.join("repo1").exists());
        assert!(!shop.exists());
        assert!(!wt.join("shop.rt-group").exists());
    }

    #[tokio::test]
    async fn delete_accepts_a_new_layout_group_and_removes_its_marker() {
        let tmp = Scratch::new();
        let root = tmp.path().join("worktrees");
        let wt = root.join("wt.feat");
        let app = seed_group(&wt, "app", crate::git::GroupKind::Repo, "App", &[""]);
        let shop = seed_group(
            &wt,
            "shop",
            crate::git::GroupKind::Workspace,
            "Shop",
            &["apps/web"],
        );
        std::fs::create_dir_all(wt.join("unmarked")).unwrap();
        let cfg_tmp = Scratch::new();
        let sessions = empty_registry(cfg_tmp.path());

        assert!(
            validate_target(&root, &wt.join("unmarked")).is_err(),
            "an unmarked child of a wt folder is not a group"
        );

        delete_group(&root, &app, &sessions).await.unwrap();
        assert!(!app.exists());
        assert!(!wt.join("app.rt-group").exists());
        assert!(shop.exists() && wt.join("shop.rt-group").exists());

        std::fs::remove_dir(wt.join("unmarked")).unwrap();
        delete_group(&root, &shop, &sessions).await.unwrap();
        assert!(!shop.exists());
        assert!(!wt.join("shop.rt-group").exists());
        assert!(
            !wt.exists(),
            "the emptied wt folder goes with its last group"
        );
    }

    #[test]
    fn status_is_per_new_layout_group() {
        let tmp = Scratch::new();
        let root = tmp.path().join("worktrees");
        let wt = root.join("wt.feat");
        let app = seed_group(&wt, "app", crate::git::GroupKind::Repo, "App", &[""]);
        let shop = seed_group(
            &wt,
            "shop",
            crate::git::GroupKind::Workspace,
            "Shop",
            &["apps/web"],
        );
        // A session's worktree maps to its own group folder: the single
        // repo's folder is the member itself; a workspace member's is the
        // marked folder above it, not the shared `wt.feat`.
        assert_eq!(wt_dir_for_member(&app).as_deref(), Some(app.as_path()));
        assert_eq!(
            wt_dir_for_member(&shop.join("apps").join("web")).as_deref(),
            Some(shop.as_path())
        );
        let live_key = simplify_path(&std::fs::canonicalize(&app).unwrap());
        let xref = HashMap::from([(live_key, ("s1".to_string(), true))]);

        let mut entries = Vec::new();
        walk_for_wt_dirs(&root, &root, &xref, &index(&[], &[]), &mut entries, 0);

        assert_eq!(entries.len(), 2, "{entries:?}");
        let app_entry = entry_for(&entries, &app);
        assert_eq!(app_entry.status, RootWorktreeStatus::Active);
        assert_eq!(app_entry.session_id.as_deref(), Some("s1"));
        assert_eq!(entry_for(&entries, &shop).status, RootWorktreeStatus::Stale);
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
        let state = empty_state(cfg_tmp.path());
        let entries = scan_root(&path, &sessions, &state);
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
