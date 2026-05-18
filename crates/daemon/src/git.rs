//! Thin wrappers around `git` invocations the daemon needs.

use anyhow::{Context as _, anyhow};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;
use tracing::debug;

/// On Windows, suppresses the brief console window flash that would otherwise
/// appear for each git child. No-op on other platforms.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Process-wide registry of per-repo async mutexes. Two `git` invocations
/// against the same repo serialize through this; invocations against
/// different repos still run in parallel. Without this, concurrent
/// `git worktree add` / `worktree remove` calls (which the daemon issues
/// e.g. when a user spawns a workspace immediately after killing a session)
/// contend on `.git/index.lock` and can stall for tens of seconds on
/// Windows under antivirus load. Path keys are not canonicalized — callers
/// pass the path string from the registry which is already consistent.
fn repo_locks() -> &'static StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>> {
    static REGISTRY: OnceLock<StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn repo_lock(repo: &Path) -> Arc<AsyncMutex<()>> {
    let mut guard = repo_locks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(
        guard
            .entry(repo.to_path_buf())
            .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
    )
}

async fn run_git(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    // Per-invocation timing logged at debug; bumped to info when slower than
    // 500ms so the user sees outliers without flipping log filters. Worktree
    // operations on Windows commonly hit several seconds and we want that to
    // surface during slow-spawn investigations.
    //
    // Pre-spawn "begin" line is also info-level: if git itself hangs (lock
    // contention, network probe, antivirus), `output.await` never returns
    // and the post-completion timing log never fires — leaving the operator
    // staring at a daemon log with no trail. The begin/end pair makes the
    // hang state visible.
    let started = std::time::Instant::now();
    let lock = repo_lock(repo);
    let _guard = lock.lock().await;
    let acquired_after_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if acquired_after_ms >= 100 {
        tracing::info!(
            waited_ms = acquired_after_ms,
            ?args,
            repo = %repo.display(),
            "git invocation: waited on per-repo lock"
        );
    }
    tracing::info!(?args, repo = %repo.display(), "git invocation: begin");
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let output = cmd
        .output()
        .await
        .with_context(|| format!("spawning git {args:?}"))?;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if elapsed_ms >= 500 {
        tracing::info!(elapsed_ms, ?args, "slow git invocation");
    } else {
        debug!(elapsed_ms, ?args, "git invocation");
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "git {args:?} failed with status {}: {stderr}",
            output.status,
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub async fn current_branch(repo: &Path) -> anyhow::Result<Option<String>> {
    let stdout = run_git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    let trimmed = stdout.trim();
    Ok((!trimmed.is_empty() && trimmed != "HEAD").then(|| trimmed.to_string()))
}

pub async fn default_branch(repo: &Path) -> Option<String> {
    let candidates = ["main", "master", "trunk"];
    for c in candidates {
        if run_git(repo, &["rev-parse", "--verify", c]).await.is_ok() {
            return Some(c.to_string());
        }
    }
    current_branch(repo).await.ok().flatten()
}

pub async fn list_branches(repo: &Path) -> anyhow::Result<Vec<String>> {
    let stdout = run_git(
        repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )
    .await?;
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// Return all non-main worktrees for `repo` as `(branch, path)` pairs.
///
/// The main worktree (first entry from `git worktree list --porcelain`) is
/// always omitted — callers only want additional worktrees created via
/// `git worktree add`. Worktrees in a detached-HEAD state (no `branch` line
/// in the porcelain output) are included with an empty branch string.
pub async fn list_worktrees(repo: &Path) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let stdout = run_git(repo, &["worktree", "list", "--porcelain"]).await?;
    let mut all_blocks: Vec<(String, PathBuf)> = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch = String::new();

    for line in stdout.lines() {
        if line.starts_with("worktree ") {
            if let Some(path) = current_path.take() {
                all_blocks.push((std::mem::take(&mut current_branch), path));
            }
            current_path = Some(PathBuf::from(line.trim_start_matches("worktree ")));
            current_branch = String::new();
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            current_branch = branch_ref.trim_start_matches("refs/heads/").to_string();
        }
    }
    if let Some(path) = current_path.take() {
        all_blocks.push((std::mem::take(&mut current_branch), path));
    }

    // First entry is always the main worktree; skip it.
    Ok(all_blocks.into_iter().skip(1).collect())
}

/// Add a worktree at `target_path` checking out `branch`. If `create_from_base`
/// is `Some(base)`, the branch is created off `base` first.
pub async fn worktree_add(
    repo: &Path,
    target_path: &Path,
    branch: &str,
    create_from_base: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent).context("creating worktree parent dir")?;
    }
    let target_str = target_path.to_string_lossy();
    let mut args: Vec<&str> = vec!["worktree", "add"];
    if let Some(base) = create_from_base {
        args.extend_from_slice(&["-b", branch, &target_str, base]);
    } else {
        args.extend_from_slice(&[&target_str, branch]);
    }
    run_git(repo, &args).await.map(|_| ())
}

pub async fn worktree_remove(repo: &Path, target_path: &Path) -> anyhow::Result<()> {
    let target_str = target_path.to_string_lossy();
    run_git(repo, &["worktree", "remove", "--force", &target_str])
        .await
        .map(|_| ())
}

/// `git -C <repo> worktree prune`. Drops orphaned
/// `.git/worktrees/<name>/` admin dirs whose target on disk is gone.
/// Used by [`crate::worktree_cleanup`] after a member removal so the
/// originating repo's worktree registry stays in sync with the disk.
pub async fn worktree_prune(repo: &Path) -> anyhow::Result<()> {
    run_git(repo, &["worktree", "prune"]).await.map(|_| ())
}

pub async fn changed_files(worktree: &Path) -> anyhow::Result<Vec<String>> {
    let stdout = run_git(worktree, &["status", "--porcelain=1"]).await?;
    Ok(stdout
        .lines()
        .filter_map(|l| l.get(3..).map(str::trim).map(String::from))
        .filter(|l| !l.is_empty())
        .collect())
}

pub async fn is_clean(repo: &Path) -> anyhow::Result<bool> {
    Ok(changed_files(repo).await?.is_empty())
}

/// Check out `branch` directly in `repo`'s working tree (no worktree). If
/// the branch doesn't exist, create it from `create_from_base`. The working
/// tree must be clean when an actual switch is required — callers should
/// surface the returned error to the user.
///
/// Same-branch fast path: when the repo's current branch already equals
/// `branch`, this is a no-op and the function returns Ok without running
/// any git command and without checking cleanliness. Spawning a session
/// against the branch you're already on must never touch the working
/// tree, even if it has uncommitted changes — that's the whole point of
/// "spawn in-place" on the current branch.
pub async fn checkout_in_place(
    repo: &Path,
    branch: &str,
    create_from_base: Option<&str>,
) -> anyhow::Result<()> {
    if current_branch(repo).await.ok().flatten().as_deref() == Some(branch) {
        return Ok(());
    }
    if !is_clean(repo).await? {
        return Err(anyhow!(
            "{} has uncommitted changes; switching to '{branch}' would touch them — commit or stash first, or use a worktree",
            repo.display()
        ));
    }
    let branch_exists = run_git(repo, &["rev-parse", "--verify", branch])
        .await
        .is_ok();
    if branch_exists {
        run_git(repo, &["checkout", branch]).await.map(|_| ())
    } else {
        let base = create_from_base.unwrap_or("HEAD");
        run_git(repo, &["checkout", "-b", branch, base])
            .await
            .map(|_| ())
    }
}

/// Convert a branch name into a filesystem-safe slug by replacing path
/// separators. `feature/foo` → `feature-foo`. Pure helper; no allocation
/// other than the resulting `String`.
fn branch_slug(branch: &str) -> String {
    branch
        .chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .collect()
}

/// Longest common path-component prefix of `paths`. Empty if there's no
/// shared prefix (e.g. cross-drive on Windows: `X:\…` vs `Y:\…`).
fn common_path_prefix(paths: &[PathBuf]) -> PathBuf {
    let Some((first, rest)) = paths.split_first() else {
        return PathBuf::new();
    };
    let mut prefix: Vec<Component> = first.components().collect();
    for path in rest {
        let other: Vec<Component> = path.components().collect();
        let common_len = prefix
            .iter()
            .zip(other.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(common_len);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.iter().collect()
}

/// Sanitize an anchor path into a relative `PathBuf` suitable for joining
/// under a worktrees root. Splits on both forward and back slashes, strips
/// `:` from each component (so drive letters `X:` become `X`), and drops
/// empty components. UNC prefixes (`\\server\share`) collapse to
/// `server/share` — acceptable lossy form for a rare case.
fn sanitize_anchor(anchor: &Path) -> PathBuf {
    let s = anchor.to_string_lossy();
    let mut out = PathBuf::new();
    for segment in s.split(['/', '\\']) {
        let cleaned: String = segment.chars().filter(|c| *c != ':').collect();
        if !cleaned.is_empty() {
            out.push(&cleaned);
        }
    }
    out
}

/// Build worktree paths under `worktrees_root` aligned with `member_repos`.
///
/// Anchor = common path-component prefix of each member's *parent* directory
/// (using parents — not the repos themselves — so a member that's an
/// ancestor of another member doesn't collide with the shared `wt.<slug>/`
/// folder). Each member's worktree is at
/// `<worktrees_root>/<sanitized-anchor>/wt.<slug>/<rel-to-anchor>`, where
/// `rel-to-anchor` mirrors the member's offset from the anchor in source
/// space — preserving inter-member relative paths (`../repo2` etc.).
///
/// Cross-drive members (no common ancestor with the first member, e.g. one
/// on `X:\` and another on `Y:\` on Windows) cannot preserve relativity.
/// They fall back to leaf-name placement under the anchor's `wt.<slug>/`,
/// with `-2`, `-3`, … suffixes appended to avoid leaf collisions.
///
/// Single-member callers (e.g. non-workspace sessions) pass a one-element
/// slice and get a single-element `Vec` back.
pub fn workspace_worktree_paths(
    worktrees_root: &Path,
    member_repos: &[&Path],
    branch: &str,
) -> Vec<PathBuf> {
    if member_repos.is_empty() {
        return Vec::new();
    }
    let slug = branch_slug(branch);

    let parents: Vec<PathBuf> = member_repos
        .iter()
        .map(|p| {
            p.parent()
                .map_or_else(|| (*p).to_path_buf(), Path::to_path_buf)
        })
        .collect();

    let mut anchor = common_path_prefix(&parents);
    if anchor.as_os_str().is_empty() {
        // Cross-drive fallback: anchor under the first member's parent.
        anchor.clone_from(&parents[0]);
    }

    let sanitized = sanitize_anchor(&anchor);
    let wt_root = worktrees_root.join(sanitized).join(format!("wt.{slug}"));

    // Names taken at the first level under `wt_root` so cross-drive members
    // can avoid collisions with anchor-matching member subtrees.
    let mut used_first_level: HashSet<String> = HashSet::new();
    let mut out: Vec<PathBuf> = Vec::with_capacity(member_repos.len());

    for member in member_repos {
        let path = if let Ok(rel) = member.strip_prefix(&anchor) {
            let rel_buf = if rel.as_os_str().is_empty() {
                // Anchor equals the member itself (rare edge case at fs roots);
                // fall back to the member's file name.
                PathBuf::from(
                    member
                        .file_name()
                        .map_or_else(|| OsString::from("repo"), std::ffi::OsStr::to_os_string),
                )
            } else {
                rel.to_path_buf()
            };
            if let Some(first) = rel_buf
                .components()
                .next()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
            {
                used_first_level.insert(first);
            }
            wt_root.join(rel_buf)
        } else {
            // Cross-drive member: leaf-name placement with collision suffix.
            let leaf = member
                .file_name()
                .map_or_else(|| "repo".to_string(), |n| n.to_string_lossy().into_owned());
            let mut candidate = leaf.clone();
            let mut suffix = 1u32;
            while used_first_level.contains(&candidate) {
                suffix += 1;
                candidate = format!("{leaf}-{suffix}");
            }
            used_first_level.insert(candidate.clone());
            wt_root.join(candidate)
        };
        out.push(path);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(strs: &[&str]) -> Vec<PathBuf> {
        strs.iter().map(PathBuf::from).collect()
    }

    fn refs(paths: &[PathBuf]) -> Vec<&Path> {
        paths.iter().map(PathBuf::as_path).collect()
    }

    #[test]
    fn branch_slug_replaces_separators() {
        assert_eq!(branch_slug("feature/foo"), "feature-foo");
        assert_eq!(branch_slug(r"feature\bar"), "feature-bar");
        assert_eq!(branch_slug("plain"), "plain");
    }

    #[test]
    fn sanitize_anchor_strips_drive_colons_and_empty_components() {
        // Drive letter form.
        assert_eq!(
            sanitize_anchor(Path::new("X:/dev")),
            PathBuf::from("X").join("dev")
        );
        // Unix absolute.
        assert_eq!(
            sanitize_anchor(Path::new("/home/u")),
            PathBuf::from("home").join("u")
        );
        // Mixed separators with empty leading components.
        assert_eq!(
            sanitize_anchor(Path::new(r"\\server\share\dir")),
            PathBuf::from("server").join("share").join("dir")
        );
    }

    #[cfg(windows)]
    #[test]
    fn workspace_worktree_paths_single_member_windows() {
        let root = PathBuf::from(r"C:\wt");
        let members = paths(&[r"X:\dev\foo"]);
        let got = workspace_worktree_paths(&root, &refs(&members), "main");
        assert_eq!(got, vec![PathBuf::from(r"C:\wt\X\dev\wt.main\foo")]);
    }

    #[cfg(windows)]
    #[test]
    fn workspace_worktree_paths_sibling_members_windows() {
        let root = PathBuf::from(r"C:\wt");
        let members = paths(&[r"X:\dev\repo1", r"X:\dev\repo2"]);
        let got = workspace_worktree_paths(&root, &refs(&members), "feature/x");
        assert_eq!(
            got,
            vec![
                PathBuf::from(r"C:\wt\X\dev\wt.feature-x\repo1"),
                PathBuf::from(r"C:\wt\X\dev\wt.feature-x\repo2"),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn workspace_worktree_paths_nested_parents_windows() {
        let root = PathBuf::from(r"C:\wt");
        let members = paths(&[r"X:\dev\a\repo1", r"X:\dev\b\repo2"]);
        let got = workspace_worktree_paths(&root, &refs(&members), "main");
        assert_eq!(
            got,
            vec![
                PathBuf::from(r"C:\wt\X\dev\wt.main\a\repo1"),
                PathBuf::from(r"C:\wt\X\dev\wt.main\b\repo2"),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn workspace_worktree_paths_ancestor_of_other_windows() {
        // Member-1 is an ancestor of member-2's parent. Anchor is the common
        // prefix of the *parents* (`X:\dev`), which keeps `wt.main\` from
        // overlapping with `repo1` itself.
        let root = PathBuf::from(r"C:\wt");
        let members = paths(&[r"X:\dev\repo1", r"X:\dev\repo1\sub"]);
        let got = workspace_worktree_paths(&root, &refs(&members), "main");
        assert_eq!(
            got,
            vec![
                PathBuf::from(r"C:\wt\X\dev\wt.main\repo1"),
                PathBuf::from(r"C:\wt\X\dev\wt.main\repo1\sub"),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn workspace_worktree_paths_cross_drive_fallback_windows() {
        let root = PathBuf::from(r"C:\wt");
        let members = paths(&[r"X:\foo", r"Y:\bar"]);
        let got = workspace_worktree_paths(&root, &refs(&members), "main");
        assert_eq!(
            got,
            vec![
                PathBuf::from(r"C:\wt\X\wt.main\foo"),
                PathBuf::from(r"C:\wt\X\wt.main\bar"),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn workspace_worktree_paths_cross_drive_collision_windows() {
        // Both members named `api`; the cross-drive one gets `-2`.
        let root = PathBuf::from(r"C:\wt");
        let members = paths(&[r"X:\team-a\api", r"Y:\team-b\api"]);
        let got = workspace_worktree_paths(&root, &refs(&members), "main");
        assert_eq!(
            got,
            vec![
                PathBuf::from(r"C:\wt\X\team-a\wt.main\api"),
                PathBuf::from(r"C:\wt\X\team-a\wt.main\api-2"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_worktree_paths_unix_sibling_members() {
        let root = PathBuf::from("/wt");
        let members = paths(&["/home/u/r1", "/home/u/r2"]);
        let got = workspace_worktree_paths(&root, &refs(&members), "main");
        assert_eq!(
            got,
            vec![
                PathBuf::from("/wt/home/u/wt.main/r1"),
                PathBuf::from("/wt/home/u/wt.main/r2"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_worktree_paths_unix_single_member() {
        let root = PathBuf::from("/wt");
        let members = paths(&["/home/u/foo"]);
        let got = workspace_worktree_paths(&root, &refs(&members), "feature/x");
        assert_eq!(got, vec![PathBuf::from("/wt/home/u/wt.feature-x/foo")]);
    }
}
