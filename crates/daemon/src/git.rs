//! Thin wrappers around `git` invocations the daemon needs.

use anyhow::{Context as _, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

/// Build a default worktree path: `<repo_parent>/<repo_name>.wt/<branch_slug>`.
pub fn default_worktree_path(repo: &Path, branch: &str) -> PathBuf {
    let repo_name = repo
        .file_name()
        .map_or_else(|| "repo".to_string(), |n| n.to_string_lossy().into_owned());
    let parent = repo.parent().unwrap_or(repo);
    let slug: String = branch
        .chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .collect();
    parent.join(format!("{repo_name}.wt")).join(slug)
}
