//! Thin wrappers around `git` invocations the daemon needs.

use anyhow::{Context as _, anyhow};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;
use tracing::debug;

/// On Windows, suppresses the brief console window flash that would otherwise
/// appear for each git child. No-op on other platforms.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Environment forced onto network git invocations so a repo with expired or
/// missing credentials fails fast instead of blocking on a prompt. A blocked
/// fetch would hold the per-repo lock forever and wedge every other git call
/// for that repo — status refreshes, worktree adds, the lot.
const NON_INTERACTIVE_ENV: &[(&str, &str)] =
    &[("GIT_TERMINAL_PROMPT", "0"), ("GCM_INTERACTIVE", "never")];

/// Ceiling on a single fetch. Fetches run off the spawn path, but they still
/// hold the per-repo lock, so an unbounded one stalls that repo's other git
/// work. Twenty seconds is long enough for a slow-but-working remote and
/// short enough that a dead one doesn't linger.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

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
    run_git_inner(repo, args, false).await
}

/// Like [`run_git`] but for invocations that touch the network. Forces
/// [`NON_INTERACTIVE_ENV`] so a credential prompt can't hang the child, and
/// sets `kill_on_drop` so abandoning the future (via a timeout) actually
/// reaps the process instead of leaving it holding the per-repo lock.
async fn run_git_network(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    run_git_inner(repo, args, true).await
}

async fn run_git_inner(repo: &Path, args: &[&str], network: bool) -> anyhow::Result<String> {
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
    if network {
        for (key, value) in NON_INTERACTIVE_ENV {
            cmd.env(key, value);
        }
        cmd.kill_on_drop(true);
    }
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

/// Whether `repo` is a usable git working tree — i.e. it has both a `.git`
/// entry AND at least one commit. Folders that are not git repos, or were
/// just `git init`'d without an initial commit, return `false`. Callers use
/// this to decide whether to run branch/worktree operations or treat the
/// path as a plain directory.
pub async fn is_initialized(repo: &Path) -> bool {
    run_git(repo, &["rev-parse", "--verify", "HEAD"])
        .await
        .is_ok()
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

/// Remote-tracking branches as short names (`origin/main`). Kept separate
/// from [`list_branches`] so callers that ask "does this *local* branch
/// exist" keep their exact meaning. `<remote>/HEAD` is dropped — it's a
/// symbolic alias, not something a user would base work on.
pub async fn list_remote_branches(repo: &Path) -> anyhow::Result<Vec<String>> {
    let stdout = run_git(
        repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/remotes/"],
    )
    .await?;
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.ends_with("/HEAD"))
        .map(String::from)
        .collect())
}

/// Configured remote names, `origin` first when present. Ordering matters:
/// callers probing for a remote-tracking counterpart should land on `origin`
/// before some incidental second remote.
pub async fn remotes(repo: &Path) -> Vec<String> {
    let Ok(stdout) = run_git(repo, &["remote"]).await else {
        return Vec::new();
    };
    let mut names: Vec<String> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    names.sort_by_key(|r| r != "origin");
    names
}

/// Whether the fully-qualified `refname` (e.g. `refs/remotes/origin/main`)
/// exists. Uses `show-ref --verify` rather than `rev-parse`, which would
/// happily DWIM a bare name onto a tag or a remote-tracking ref and report
/// success for a local branch that doesn't exist.
pub async fn full_ref_exists(repo: &Path, refname: &str) -> bool {
    run_git(repo, &["show-ref", "--verify", "--quiet", refname])
        .await
        .is_ok()
}

/// The remote-tracking ref shadowing local branch `branch`, as a short name
/// (`origin/main`), or `None` when no remote carries that branch.
pub async fn remote_tracking_for(repo: &Path, branch: &str) -> Option<String> {
    let remotes = remotes(repo).await;
    // A base that already names a remote resolves to itself — re-prefixing
    // would ask for `origin/origin/main`.
    for remote in &remotes {
        if branch.starts_with(&format!("{remote}/")) {
            return full_ref_exists(repo, &format!("refs/remotes/{branch}"))
                .await
                .then(|| branch.to_string());
        }
    }
    for remote in &remotes {
        let short = format!("{remote}/{branch}");
        if full_ref_exists(repo, &format!("refs/remotes/{short}")).await {
            return Some(short);
        }
    }
    None
}

/// Resolve the ref a *new* branch should fork from, preferring the
/// remote-tracking counterpart of `base` when one exists.
///
/// In a repo driven entirely through worktrees, the local default branch
/// never advances: every session forks off it, works, and pushes to the
/// remote, leaving the primary working tree's `main` pinned wherever it was
/// last checked out by hand. Forking from `origin/main` makes "base off
/// main" mean what it looks like it means. Falls back to `base` verbatim
/// when there is no remote-tracking counterpart — no remote configured,
/// never fetched, or `base` is a raw SHA or tag.
pub async fn resolve_base_ref(repo: &Path, base: &str) -> String {
    remote_tracking_for(repo, base)
        .await
        .unwrap_or_else(|| base.to_string())
}

/// `(ahead, behind)` of `left` relative to `right`: commits reachable from
/// `left` but not `right`, and vice versa. `None` when either ref is missing.
pub async fn ahead_behind(repo: &Path, left: &str, right: &str) -> Option<(u32, u32)> {
    let range = format!("{left}...{right}");
    let stdout = run_git(repo, &["rev-list", "--left-right", "--count", &range])
        .await
        .ok()?;
    parse_ahead_behind(&stdout)
}

fn parse_ahead_behind(stdout: &str) -> Option<(u32, u32)> {
    let mut parts = stdout.lines().next()?.split_whitespace();
    let ahead = parts.next()?.parse().ok()?;
    let behind = parts.next()?.parse().ok()?;
    Some((ahead, behind))
}

/// Abbreviated SHA of `HEAD` at `path`, which may be a worktree rather than
/// the primary repo directory.
pub async fn head_short_sha(path: &Path) -> Option<String> {
    let stdout = run_git(path, &["rev-parse", "--short", "HEAD"])
        .await
        .ok()?;
    let trimmed = stdout.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// `git fetch --prune` against the default remote, bounded by
/// [`FETCH_TIMEOUT`] and hardened against credential prompts.
///
/// Callers treat failure as advisory: a fetch is a freshness improvement,
/// never a precondition. A repo with no remote is a no-op success.
pub async fn fetch(repo: &Path) -> anyhow::Result<()> {
    if remotes(repo).await.is_empty() {
        return Ok(());
    }
    let fut = run_git_network(
        repo,
        &[
            "-c",
            "credential.interactive=false",
            "fetch",
            "--prune",
            "--quiet",
        ],
    );
    match tokio::time::timeout(FETCH_TIMEOUT, fut).await {
        Ok(result) => result.map(|_| ()),
        Err(_) => Err(anyhow!(
            "git fetch in {} timed out after {}s",
            repo.display(),
            FETCH_TIMEOUT.as_secs()
        )),
    }
}

/// Return all non-main worktrees for `repo` as `(branch, path)` pairs.
///
/// The main worktree (first entry from `git worktree list --porcelain`) is
/// always omitted — callers only want additional worktrees created via
/// `git worktree add`. Worktrees in a detached-HEAD state (no `branch` line
/// in the porcelain output) are included with an empty branch string.
pub async fn list_worktrees(repo: &Path) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let stdout = run_git(repo, &["worktree", "list", "--porcelain"]).await?;
    // First entry is always the main worktree; skip it.
    Ok(parse_worktree_list(&stdout).into_iter().skip(1).collect())
}

/// Every worktree in `git worktree list --porcelain` output as `(branch, path)`
/// pairs, main worktree first. Detached-HEAD worktrees carry an empty branch.
fn parse_worktree_list(stdout: &str) -> Vec<(String, PathBuf)> {
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
    all_blocks
}

/// Path of the worktree that currently has `branch` checked out, main
/// worktree included. `None` when no worktree holds it (or git failed).
///
/// A branch checked out anywhere is undeletable, so the discard path uses
/// this to tell the session own worktree apart from any other holder before
/// it considers reaping.
pub async fn worktree_holding_branch(repo: &Path, branch: &str) -> Option<PathBuf> {
    let stdout = run_git(repo, &["worktree", "list", "--porcelain"])
        .await
        .ok()?;
    parse_worktree_list(&stdout)
        .into_iter()
        .find(|(b, _)| b == branch)
        .map(|(_, path)| path)
}

/// Error when `branch` names an existing remote-tracking ref (`origin/main`).
///
/// Creating a *local* branch by that name materializes `refs/heads/origin/main`
/// alongside `refs/remotes/origin/main`, after which every bare `origin/main`
/// reference is ambiguous. Callers that are about to create or check out a
/// branch run this first so the request fails loudly instead.
async fn ensure_not_remote_tracking(repo: &Path, branch: &str) -> anyhow::Result<()> {
    if full_ref_exists(repo, &format!("refs/remotes/{branch}")).await {
        return Err(anyhow!(
            "'{branch}' names a remote-tracking ref — pick a local branch name, or use '{branch}' as the base branch instead"
        ));
    }
    Ok(())
}

/// Add a worktree at `target_path` checking out `branch`. If `create_from_base`
/// is `Some(base)`, the branch is created off `base` first.
pub async fn worktree_add(
    repo: &Path,
    target_path: &Path,
    branch: &str,
    create_from_base: Option<&str>,
) -> anyhow::Result<()> {
    if create_from_base.is_some() {
        ensure_not_remote_tracking(repo, branch).await?;
    }
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

/// `git branch -D <branch>`. Used when recreating a worktree from a fresh
/// base: `worktree remove` leaves the branch behind, so re-adding with `-b`
/// would fail on "branch already exists". Errors when the branch is absent,
/// which callers treat as success.
pub async fn delete_branch(repo: &Path, branch: &str) -> anyhow::Result<()> {
    run_git(repo, &["branch", "-D", branch]).await.map(|_| ())
}

/// Whether the work on a branch already exists somewhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchMergeStatus {
    /// Every commit on the branch is carried by `into`, established `via`.
    Merged {
        into: String,
        via: protocol::MergeEvidence,
    },
    /// The branch introduces `unique_commits` patches no target carries,
    /// counted against whichever target it is closest to.
    Unmerged { unique_commits: u32 },
}

/// Measure `branch` against `targets` (highest-priority target first).
///
/// A target counts as carrying the branch when the branch tip is reachable
/// from it (`merge-base --is-ancestor`) or when `git cherry` finds no patch
/// the target is missing. The second test is what makes a cherry-picked,
/// rebased, or otherwise-rewritten land readable; a squash merge or a
/// conflict-edited pick still reads as unique, which errs on the safe side.
///
/// Targets whose git calls fail are skipped. Errors when `branch` does not
/// exist, when `targets` is empty, or when every target was skipped — none
/// of those support a keep-or-delete conclusion.
pub async fn branch_merge_status(
    repo: &Path,
    branch: &str,
    targets: &[String],
) -> anyhow::Result<BranchMergeStatus> {
    // `merge-base --is-ancestor` exits 1 for "not an ancestor" and 128 for a
    // bad ref, and `run_git` collapses both into `Err`. Verifying the refs up
    // front is what lets a later failure be read as the honest answer.
    if !full_ref_exists(repo, &format!("refs/heads/{branch}")).await {
        return Err(anyhow!(
            "branch {branch} does not exist in {}",
            repo.display()
        ));
    }

    let mut closest: Option<u32> = None;
    for target in targets {
        if run_git(repo, &["rev-parse", "--verify", "--quiet", target])
            .await
            .is_err()
        {
            debug!(
                branch,
                target, "branch_merge_status: skipping missing target"
            );
            continue;
        }
        if run_git(repo, &["merge-base", "--is-ancestor", branch, target])
            .await
            .is_ok()
        {
            return Ok(BranchMergeStatus::Merged {
                into: target.clone(),
                via: protocol::MergeEvidence::Ancestry,
            });
        }
        let stdout = match run_git(repo, &["cherry", target, branch]).await {
            Ok(stdout) => stdout,
            Err(err) => {
                debug!(?err, branch, target, "branch_merge_status: cherry failed");
                continue;
            }
        };
        // `git cherry` prints `+ <sha>` for a patch the target lacks and
        // `- <sha>` for one it already carries.
        let unique = u32::try_from(stdout.lines().filter(|l| l.starts_with('+')).count())
            .unwrap_or(u32::MAX);
        if unique == 0 {
            return Ok(BranchMergeStatus::Merged {
                into: target.clone(),
                via: protocol::MergeEvidence::PatchEquivalent,
            });
        }
        closest = Some(closest.map_or(unique, |best: u32| best.min(unique)));
    }

    closest.map_or_else(
        || {
            Err(anyhow!(
                "no usable merge target for branch {branch} among {targets:?}"
            ))
        },
        |unique_commits| Ok(BranchMergeStatus::Unmerged { unique_commits }),
    )
}

/// Abbreviated SHA of an arbitrary ref in `repo` — the branch-name
/// counterpart of [`head_short_sha`], for refs that aren't checked out
/// anywhere.
pub async fn ref_short_sha(repo: &Path, refname: &str) -> Option<String> {
    let stdout = run_git(repo, &["rev-parse", "--short", refname])
        .await
        .ok()?;
    let trimmed = stdout.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
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

/// What an in-place checkout of `branch` would do right now, computed without
/// mutating the working tree. Drives the daemon's "confirm before touching a
/// dirty tree" prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InPlaceCheckout {
    /// Already on `branch`; switching is a no-op.
    SameBranch,
    /// On a different branch with a clean tree; safe to switch.
    Clean,
    /// On a different branch with `count` uncommitted changes a switch would
    /// touch.
    Dirty { count: usize },
}

/// Classify an in-place checkout of `branch` without changing anything.
pub async fn in_place_checkout_preflight(
    repo: &Path,
    branch: &str,
) -> anyhow::Result<InPlaceCheckout> {
    if current_branch(repo).await.ok().flatten().as_deref() == Some(branch) {
        return Ok(InPlaceCheckout::SameBranch);
    }
    let changed = changed_files(repo).await?;
    if changed.is_empty() {
        Ok(InPlaceCheckout::Clean)
    } else {
        Ok(InPlaceCheckout::Dirty {
            count: changed.len(),
        })
    }
}

/// Check out `branch` directly in `repo`'s working tree (no worktree),
/// creating it from `create_from_base` if it doesn't exist.
///
/// Same-branch fast path: when the repo's current branch already equals
/// `branch`, this is a no-op and returns Ok without running any git command
/// or checking cleanliness — spawning against the branch you're already on
/// must never touch the working tree, even when it has uncommitted changes.
///
/// Otherwise `strategy` governs a dirty tree:
/// - `None`: refuse if the tree is dirty. The daemon pre-checks via
///   [`in_place_checkout_preflight`] and prompts, so this is the safety
///   backstop (and the behavior for non-interactive callers like presets).
/// - `Some(Carry)`: `git checkout`, carrying uncommitted changes across (git
///   refuses if they would conflict with the target).
/// - `Some(Stash)`: stash (including untracked), switch, and leave the stash.
/// - `Some(Unknown)`: treated as `None`.
pub async fn checkout_in_place(
    repo: &Path,
    branch: &str,
    create_from_base: Option<&str>,
    strategy: Option<protocol::CheckoutStrategy>,
) -> anyhow::Result<()> {
    if current_branch(repo).await.ok().flatten().as_deref() == Some(branch) {
        return Ok(());
    }
    ensure_not_remote_tracking(repo, branch).await?;
    let carry = matches!(strategy, Some(protocol::CheckoutStrategy::Carry));
    let stash = matches!(strategy, Some(protocol::CheckoutStrategy::Stash));
    if !carry && !stash && !is_clean(repo).await? {
        return Err(anyhow!(
            "{} has uncommitted changes; switching to '{branch}' would touch them — commit or stash first, or use a worktree",
            repo.display()
        ));
    }
    if stash {
        let message = format!("rustling-tulip: switching to {branch}");
        run_git(
            repo,
            &["stash", "push", "--include-untracked", "-m", &message],
        )
        .await
        .context("stashing changes before in-place switch")?;
    }
    // "Exists" must mean the *local* branch exists. `rev-parse --verify`
    // would DWIM the name onto a tag or remote-tracking ref and report
    // success, sending the checkout below to a detached HEAD.
    let branch_exists = full_ref_exists(repo, &format!("refs/heads/{branch}")).await;
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
/// `<worktrees_root>/wt.<slug>/<sanitized-anchor>/<rel-to-anchor>`, where
/// `rel-to-anchor` mirrors the member's offset from the anchor in source
/// space — preserving inter-member relative paths (`../repo2` etc.).
///
/// Cross-drive members (no common ancestor with the first member, e.g. one
/// on `X:\` and another on `Y:\` on Windows) cannot preserve relativity.
/// They fall back to leaf-name placement under `wt.<slug>/<sanitized-anchor>/`,
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
    let wt_root = worktrees_root.join(format!("wt.{slug}")).join(sanitized);

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
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use super::*;

    fn paths(strs: &[&str]) -> Vec<PathBuf> {
        strs.iter().map(PathBuf::from).collect()
    }

    fn refs(paths: &[PathBuf]) -> Vec<&Path> {
        paths.iter().map(PathBuf::as_path).collect()
    }

    #[test]
    fn parse_ahead_behind_reads_left_then_right() {
        // `rev-list --left-right --count A...B` emits "<left-only>\t<right-only>".
        assert_eq!(parse_ahead_behind("0\t273\n"), Some((0, 273)));
        assert_eq!(parse_ahead_behind("4\t0\n"), Some((4, 0)));
        assert_eq!(parse_ahead_behind("12 7"), Some((12, 7)));
    }

    #[test]
    fn parse_ahead_behind_rejects_malformed_output() {
        assert_eq!(parse_ahead_behind(""), None);
        assert_eq!(parse_ahead_behind("5"), None);
        assert_eq!(parse_ahead_behind("fatal: bad revision\n"), None);
        assert_eq!(parse_ahead_behind("-1\t2"), None);
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
        assert_eq!(got, vec![PathBuf::from(r"C:\wt\wt.main\X\dev\foo")]);
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
                PathBuf::from(r"C:\wt\wt.feature-x\X\dev\repo1"),
                PathBuf::from(r"C:\wt\wt.feature-x\X\dev\repo2"),
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
                PathBuf::from(r"C:\wt\wt.main\X\dev\a\repo1"),
                PathBuf::from(r"C:\wt\wt.main\X\dev\b\repo2"),
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
                PathBuf::from(r"C:\wt\wt.main\X\dev\repo1"),
                PathBuf::from(r"C:\wt\wt.main\X\dev\repo1\sub"),
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
                PathBuf::from(r"C:\wt\wt.main\X\foo"),
                PathBuf::from(r"C:\wt\wt.main\X\bar"),
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
                PathBuf::from(r"C:\wt\wt.main\X\team-a\api"),
                PathBuf::from(r"C:\wt\wt.main\X\team-a\api-2"),
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
                PathBuf::from("/wt/wt.main/home/u/r1"),
                PathBuf::from("/wt/wt.main/home/u/r2"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_worktree_paths_unix_single_member() {
        let root = PathBuf::from("/wt");
        let members = paths(&["/home/u/foo"]);
        let got = workspace_worktree_paths(&root, &refs(&members), "feature/x");
        assert_eq!(got, vec![PathBuf::from("/wt/wt.feature-x/home/u/foo")]);
    }

    // --- In-place checkout integration tests (shell real git) ---------------
    // The daemon shells `git` for everything, so these spin up a throwaway repo
    // and exercise the real preflight/checkout behavior rather than mocking.

    /// Create a throwaway git repo on `main` with a single committed file.
    async fn init_repo(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rt-git-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create repo dir");
        run_git(&root, &["init"]).await.expect("git init");
        run_git(&root, &["config", "user.email", "t@example.com"])
            .await
            .expect("config email");
        run_git(&root, &["config", "user.name", "Test"])
            .await
            .expect("config name");
        // A global commit.gpgsign=true without a usable key would block commits.
        run_git(&root, &["config", "commit.gpgsign", "false"])
            .await
            .expect("config gpgsign");
        std::fs::write(root.join("README.md"), "init\n").expect("seed file");
        run_git(&root, &["add", "."]).await.expect("git add");
        run_git(&root, &["commit", "-m", "init"])
            .await
            .expect("git commit");
        // Normalize the initial branch name regardless of git's default.
        run_git(&root, &["branch", "-M", "main"])
            .await
            .expect("rename to main");
        root
    }

    fn write_file(repo: &Path, name: &str, body: &str) {
        std::fs::write(repo.join(name), body).expect("write file");
    }

    /// A repo whose local `main` trails `origin/main` by `behind` commits —
    /// the exact shape of a repo driven entirely through worktrees, where
    /// every session forks off, works, and pushes, and nobody ever pulls the
    /// primary working tree. Returns the working repo; the bare origin lives
    /// alongside it and is removed with the same prefix.
    async fn init_stale_repo(tag: &str, behind: u32) -> PathBuf {
        let repo = init_repo(tag).await;
        let origin = repo.with_file_name(format!(
            "{}-origin",
            repo.file_name()
                .expect("repo leaf")
                .to_string_lossy()
                .into_owned()
        ));
        let _ = std::fs::remove_dir_all(&origin);
        std::fs::create_dir_all(&origin).expect("create origin dir");
        run_git(&origin, &["init", "--bare"])
            .await
            .expect("init bare origin");

        let origin_str = origin.to_string_lossy().into_owned();
        run_git(&repo, &["remote", "add", "origin", &origin_str])
            .await
            .expect("remote add");
        run_git(&repo, &["push", "-u", "origin", "main"])
            .await
            .expect("initial push");

        // Advance origin/main without moving local main: commit on a scratch
        // branch, push it onto main, then throw the scratch branch away.
        run_git(&repo, &["checkout", "-b", "scratch"])
            .await
            .expect("scratch branch");
        for i in 0..behind {
            write_file(&repo, &format!("upstream-{i}.txt"), "landed\n");
            run_git(&repo, &["add", "."]).await.expect("add");
            run_git(&repo, &["commit", "-m", &format!("upstream {i}")])
                .await
                .expect("commit");
        }
        run_git(&repo, &["push", "origin", "scratch:main"])
            .await
            .expect("push to main");
        run_git(&repo, &["checkout", "main"])
            .await
            .expect("back to main");
        run_git(&repo, &["branch", "-D", "scratch"])
            .await
            .expect("drop scratch");
        fetch(&repo).await.expect("fetch");
        repo
    }

    fn cleanup_stale_repo(repo: &Path) {
        let _ = std::fs::remove_dir_all(repo);
        let leaf = repo
            .file_name()
            .expect("repo leaf")
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_dir_all(repo.with_file_name(format!("{leaf}-origin")));
    }

    #[tokio::test]
    async fn resolve_base_ref_prefers_remote_tracking() {
        let repo = init_stale_repo("base-remote", 2).await;
        assert_eq!(
            remote_tracking_for(&repo, "main").await.as_deref(),
            Some("origin/main")
        );
        assert_eq!(resolve_base_ref(&repo, "main").await, "origin/main");
        cleanup_stale_repo(&repo);
    }

    #[tokio::test]
    async fn resolve_base_ref_leaves_a_remote_ref_alone() {
        let repo = init_stale_repo("base-already-remote", 1).await;
        // Must not compound into `origin/origin/main`.
        assert_eq!(resolve_base_ref(&repo, "origin/main").await, "origin/main");
        cleanup_stale_repo(&repo);
    }

    #[tokio::test]
    async fn resolve_base_ref_falls_back_without_a_remote() {
        let repo = init_repo("base-no-remote").await;
        assert_eq!(remote_tracking_for(&repo, "main").await, None);
        assert_eq!(resolve_base_ref(&repo, "main").await, "main");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn fetch_without_a_remote_is_a_noop_success() {
        let repo = init_repo("fetch-no-remote").await;
        fetch(&repo).await.expect("fetch on remote-less repo is Ok");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn ahead_behind_measures_local_main_against_origin() {
        let repo = init_stale_repo("stale-count", 3).await;
        assert_eq!(
            ahead_behind(&repo, "main", "origin/main").await,
            Some((0, 3)),
            "local main trails origin/main by the pushed commits"
        );
        cleanup_stale_repo(&repo);
    }

    /// The core of the fix: basing a worktree on the remote-tracking ref puts
    /// it on current work, while basing it on the stale local branch forks
    /// from wherever that branch was abandoned.
    #[tokio::test]
    async fn worktree_from_remote_ref_skips_the_stale_local_branch() {
        let repo = init_stale_repo("wt-remote-base", 4).await;
        let wt = repo.with_file_name("rt-wt-remote-base-tree");
        let _ = std::fs::remove_dir_all(&wt);

        worktree_add(&repo, &wt, "wt/fresh", Some("origin/main"))
            .await
            .expect("worktree from origin/main");
        assert_eq!(
            ahead_behind(&wt, "HEAD", "origin/main").await,
            Some((0, 0)),
            "worktree sits exactly on origin/main"
        );
        assert_eq!(
            ahead_behind(&wt, "HEAD", "main").await,
            Some((4, 0)),
            "and therefore ahead of the stale local main"
        );

        let _ = std::fs::remove_dir_all(&wt);
        cleanup_stale_repo(&repo);
    }

    /// Recreating an existing worktree must actually move its fork point.
    /// Mirrors the remove → delete-branch → re-add sequence the daemon runs
    /// for `WorktreeReusePolicy::RecreateFromBase`; without the branch delete
    /// the re-add fails on "branch already exists" and the stale worktree
    /// silently survives.
    #[tokio::test]
    async fn recreating_a_worktree_moves_it_off_the_stale_base() {
        let repo = init_stale_repo("wt-recreate", 5).await;
        let wt = repo.with_file_name("rt-wt-recreate-tree");
        let _ = std::fs::remove_dir_all(&wt);

        worktree_add(&repo, &wt, "wt/stale", Some("main"))
            .await
            .expect("worktree from stale local main");
        assert_eq!(
            ahead_behind(&wt, "HEAD", "origin/main").await,
            Some((0, 5)),
            "forked from the stale local main"
        );

        worktree_remove(&repo, &wt).await.expect("remove worktree");
        delete_branch(&repo, "wt/stale")
            .await
            .expect("branch survives worktree removal and must be deleted");
        worktree_add(&repo, &wt, "wt/stale", Some("origin/main"))
            .await
            .expect("re-add from origin/main");
        assert_eq!(
            ahead_behind(&wt, "HEAD", "origin/main").await,
            Some((0, 0)),
            "recreated worktree is level with the remote"
        );

        let _ = std::fs::remove_dir_all(&wt);
        cleanup_stale_repo(&repo);
    }

    /// Commit `body` to `name` on the current branch and return its sha.
    async fn commit_file(repo: &Path, name: &str, body: &str) -> String {
        write_file(repo, name, body);
        run_git(repo, &["add", "."]).await.expect("add");
        run_git(repo, &["commit", "-m", name])
            .await
            .expect("commit");
        run_git(repo, &["rev-parse", "HEAD"])
            .await
            .expect("rev-parse")
            .trim()
            .to_string()
    }

    /// A branch that never moved off its base carries nothing unique, and
    /// ancestry is the cheapest way to say so.
    #[tokio::test]
    async fn branch_merge_status_reports_ancestry_for_an_untouched_branch() {
        let repo = init_repo("merge-ancestry").await;
        run_git(&repo, &["branch", "wt/fresh"])
            .await
            .expect("create branch");
        let status = branch_merge_status(&repo, "wt/fresh", &["main".to_string()])
            .await
            .expect("status");
        assert_eq!(
            status,
            BranchMergeStatus::Merged {
                into: "main".to_string(),
                via: protocol::MergeEvidence::Ancestry,
            }
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// The case the old `git branch -d` rule got wrong: the work landed on
    /// main under a different sha, so ancestry says no but the patch is
    /// already there.
    #[tokio::test]
    async fn branch_merge_status_sees_a_cherry_picked_land() {
        let repo = init_repo("merge-cherry").await;
        run_git(&repo, &["checkout", "-b", "wt/picked"])
            .await
            .expect("checkout branch");
        let sha = commit_file(&repo, "picked.txt", "landed elsewhere\n").await;
        run_git(&repo, &["checkout", "main"])
            .await
            .expect("back to main");
        // Move main off the fork point first: cherry-picking onto the exact
        // parent reproduces the original commit byte for byte, which would
        // fast-forward main and make this an ancestry case instead.
        commit_file(&repo, "main-only.txt", "diverged\n").await;
        run_git(&repo, &["cherry-pick", &sha])
            .await
            .expect("cherry-pick onto main");

        let status = branch_merge_status(&repo, "wt/picked", &["main".to_string()])
            .await
            .expect("status");
        assert_eq!(
            status,
            BranchMergeStatus::Merged {
                into: "main".to_string(),
                via: protocol::MergeEvidence::PatchEquivalent,
            }
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Work pushed straight to the remote leaves the stale local `main`
    /// unaware of it; the remote-tracking target is the one that answers.
    #[tokio::test]
    async fn branch_merge_status_finds_a_remote_only_land() {
        let repo = init_stale_repo("merge-remote", 0).await;
        run_git(&repo, &["checkout", "-b", "wt/pushed"])
            .await
            .expect("checkout branch");
        commit_file(&repo, "pushed.txt", "on the remote\n").await;
        run_git(&repo, &["push", "origin", "wt/pushed:main"])
            .await
            .expect("push onto origin main");
        run_git(&repo, &["checkout", "main"])
            .await
            .expect("back to main");
        fetch(&repo).await.expect("fetch");

        let targets = vec!["main".to_string(), "origin/main".to_string()];
        let status = branch_merge_status(&repo, "wt/pushed", &targets)
            .await
            .expect("status");
        assert_eq!(
            status,
            BranchMergeStatus::Merged {
                into: "origin/main".to_string(),
                via: protocol::MergeEvidence::Ancestry,
            },
            "local main is stale; origin/main is the target that carries the work"
        );
        cleanup_stale_repo(&repo);
    }

    /// Nothing landed anywhere: the count is what the confirm dialog shows.
    #[tokio::test]
    async fn branch_merge_status_counts_unique_commits() {
        let repo = init_repo("merge-unique").await;
        run_git(&repo, &["checkout", "-b", "wt/unique"])
            .await
            .expect("checkout branch");
        commit_file(&repo, "one.txt", "first\n").await;
        commit_file(&repo, "two.txt", "second\n").await;
        run_git(&repo, &["checkout", "main"])
            .await
            .expect("back to main");

        let status = branch_merge_status(&repo, "wt/unique", &["main".to_string()])
            .await
            .expect("status");
        assert_eq!(status, BranchMergeStatus::Unmerged { unique_commits: 2 });
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A partially-landed branch still counts as unmerged, and the count
    /// excludes the commit that was picked across.
    #[tokio::test]
    async fn branch_merge_status_excludes_the_landed_half_from_the_count() {
        let repo = init_repo("merge-mixed").await;
        run_git(&repo, &["checkout", "-b", "wt/mixed"])
            .await
            .expect("checkout branch");
        let landed = commit_file(&repo, "landed.txt", "goes to main\n").await;
        commit_file(&repo, "stays.txt", "only here\n").await;
        run_git(&repo, &["checkout", "main"])
            .await
            .expect("back to main");
        run_git(&repo, &["cherry-pick", &landed])
            .await
            .expect("cherry-pick the first commit");

        let status = branch_merge_status(&repo, "wt/mixed", &["main".to_string()])
            .await
            .expect("status");
        assert_eq!(status, BranchMergeStatus::Unmerged { unique_commits: 1 });
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Both no-answer shapes are errors, never a silent "merged" that would
    /// license a delete.
    #[tokio::test]
    async fn branch_merge_status_errs_without_a_branch_or_a_target() {
        let repo = init_repo("merge-errors").await;
        assert!(
            branch_merge_status(&repo, "wt/ghost", &["main".to_string()])
                .await
                .is_err(),
            "a missing branch has no merge status"
        );
        run_git(&repo, &["branch", "wt/real"])
            .await
            .expect("create branch");
        assert!(
            branch_merge_status(&repo, "wt/real", &[]).await.is_err(),
            "nothing to measure against is an error, not a merge"
        );
        assert!(
            branch_merge_status(&repo, "wt/real", &["origin/nope".to_string()])
                .await
                .is_err(),
            "every target skipped is an error, not a merge"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn worktree_holding_branch_finds_the_checkout() {
        let repo = init_repo("holder").await;
        let wt = repo.with_file_name(format!(
            "{}-wt",
            repo.file_name()
                .expect("repo leaf")
                .to_string_lossy()
                .into_owned()
        ));
        let _ = std::fs::remove_dir_all(&wt);
        worktree_add(&repo, &wt, "wt/held", Some("main"))
            .await
            .expect("worktree add");
        run_git(&repo, &["branch", "wt/loose"])
            .await
            .expect("create loose branch");

        let holder = worktree_holding_branch(&repo, "wt/held")
            .await
            .expect("held branch has a worktree");
        assert_eq!(
            crate::paths::normalize_path_key(&holder.to_string_lossy()),
            crate::paths::normalize_path_key(&wt.to_string_lossy())
        );
        assert!(
            worktree_holding_branch(&repo, "wt/loose").await.is_none(),
            "a branch with no worktree is held by nobody"
        );
        assert_eq!(
            worktree_holding_branch(&repo, "main")
                .await
                .map(|p| crate::paths::normalize_path_key(&p.to_string_lossy())),
            Some(crate::paths::normalize_path_key(&repo.to_string_lossy())),
            "the main worktree counts as a holder"
        );

        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn list_remote_branches_reports_tracking_refs_without_head() {
        let repo = init_stale_repo("remote-list", 1).await;
        let remote = list_remote_branches(&repo).await.expect("list remotes");
        assert!(
            remote.contains(&"origin/main".to_string()),
            "expected origin/main in {remote:?}"
        );
        assert!(
            !remote.iter().any(|r| r.ends_with("/HEAD")),
            "symbolic HEAD alias must be filtered out of {remote:?}"
        );
        // The local list must stay local-only so `branch_exists` checks keep
        // their exact meaning.
        let local = list_branches(&repo).await.expect("list branches");
        assert_eq!(local, vec!["main".to_string()]);
        cleanup_stale_repo(&repo);
    }

    #[tokio::test]
    async fn preflight_same_branch_is_noop() {
        let repo = init_repo("preflight-same").await;
        let got = in_place_checkout_preflight(&repo, "main")
            .await
            .expect("preflight");
        assert_eq!(got, InPlaceCheckout::SameBranch);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn preflight_clean_other_branch_is_clean() {
        let repo = init_repo("preflight-clean").await;
        let got = in_place_checkout_preflight(&repo, "feature")
            .await
            .expect("preflight");
        assert_eq!(got, InPlaceCheckout::Clean);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn preflight_dirty_counts_changes() {
        let repo = init_repo("preflight-dirty").await;
        write_file(&repo, "scratch.txt", "uncommitted\n");
        let got = in_place_checkout_preflight(&repo, "feature")
            .await
            .expect("preflight");
        assert_eq!(got, InPlaceCheckout::Dirty { count: 1 });
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn checkout_same_branch_never_touches_dirty_tree() {
        let repo = init_repo("co-same").await;
        write_file(&repo, "scratch.txt", "uncommitted\n");
        // Spawning against the branch you're already on is a no-op even when
        // the tree is dirty, and needs no strategy.
        checkout_in_place(&repo, "main", Some("main"), None)
            .await
            .expect("same-branch checkout is a no-op");
        assert!(repo.join("scratch.txt").exists(), "dirty file untouched");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn checkout_clean_creates_and_switches_branch() {
        let repo = init_repo("co-clean").await;
        checkout_in_place(&repo, "feature", Some("main"), None)
            .await
            .expect("clean switch");
        assert_eq!(
            current_branch(&repo).await.expect("branch").as_deref(),
            Some("feature")
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn checkout_dirty_without_strategy_errors() {
        let repo = init_repo("co-dirty-none").await;
        write_file(&repo, "scratch.txt", "uncommitted\n");
        let result = checkout_in_place(&repo, "feature", Some("main"), None).await;
        assert!(result.is_err(), "dirty in-place switch must refuse");
        assert_eq!(
            current_branch(&repo).await.expect("branch").as_deref(),
            Some("main"),
            "stays on the original branch after refusing"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn checkout_carry_keeps_changes_on_new_branch() {
        let repo = init_repo("co-carry").await;
        // Modify a tracked file; carrying applies cleanly onto a branch forked
        // from the same commit.
        write_file(&repo, "README.md", "carried edit\n");
        checkout_in_place(
            &repo,
            "feature",
            Some("main"),
            Some(protocol::CheckoutStrategy::Carry),
        )
        .await
        .expect("carry switch");
        assert_eq!(
            current_branch(&repo).await.expect("branch").as_deref(),
            Some("feature")
        );
        let body = std::fs::read_to_string(repo.join("README.md")).expect("read README");
        assert_eq!(body, "carried edit\n", "edit carried across the switch");
        assert!(
            !is_clean(&repo).await.expect("clean check"),
            "carried change is still uncommitted on the new branch"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn checkout_stash_cleans_tree_and_leaves_stash() {
        let repo = init_repo("co-stash").await;
        write_file(&repo, "README.md", "stashed edit\n");
        write_file(&repo, "scratch.txt", "untracked too\n");
        checkout_in_place(
            &repo,
            "feature",
            Some("main"),
            Some(protocol::CheckoutStrategy::Stash),
        )
        .await
        .expect("stash switch");
        assert_eq!(
            current_branch(&repo).await.expect("branch").as_deref(),
            Some("feature")
        );
        assert!(
            is_clean(&repo).await.expect("clean check"),
            "tree is clean after stashing"
        );
        let stashes = run_git(&repo, &["stash", "list"])
            .await
            .expect("stash list");
        assert!(
            stashes.contains("switching to feature"),
            "stash left for the user to pop: {stashes:?}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Creating a worktree branch named after a remote-tracking ref would
    /// materialize `refs/heads/origin/main`, making every later `origin/main`
    /// reference ambiguous. The add must refuse instead.
    #[tokio::test]
    async fn worktree_add_rejects_remote_tracking_branch_name() {
        let repo = init_stale_repo("wt-remote-name", 1).await;
        let wt = repo.with_file_name("rt-wt-remote-name-tree");
        let _ = std::fs::remove_dir_all(&wt);

        let result = worktree_add(&repo, &wt, "origin/main", Some("origin/main")).await;
        assert!(
            result.is_err(),
            "branch name shadowing a remote-tracking ref must refuse"
        );
        let message = format!("{:#}", result.expect_err("checked above"));
        assert!(
            message.contains("remote-tracking"),
            "error names the cause: {message}"
        );
        assert!(
            !full_ref_exists(&repo, "refs/heads/origin/main").await,
            "no shadowing local branch was created"
        );

        let _ = std::fs::remove_dir_all(&wt);
        cleanup_stale_repo(&repo);
    }

    #[tokio::test]
    async fn checkout_in_place_rejects_remote_tracking_branch_name() {
        let repo = init_stale_repo("co-remote-name", 1).await;
        let result = checkout_in_place(&repo, "origin/main", Some("main"), None).await;
        assert!(
            result.is_err(),
            "in-place checkout of a remote-tracking name must refuse"
        );
        assert_eq!(
            current_branch(&repo).await.expect("branch").as_deref(),
            Some("main"),
            "stays attached to the original branch"
        );
        assert!(
            !full_ref_exists(&repo, "refs/heads/origin/main").await,
            "no shadowing local branch was created"
        );
        cleanup_stale_repo(&repo);
    }

    /// The existence probe must mean "local branch exists", not "the name
    /// resolves to something". A tag sharing the name used to win the probe,
    /// so the checkout landed on the tag as a detached HEAD instead of
    /// creating the requested branch.
    #[tokio::test]
    async fn checkout_creates_branch_even_when_a_tag_shadows_the_name() {
        let repo = init_repo("co-tag-shadow").await;
        run_git(&repo, &["tag", "v-shadow"]).await.expect("tag");
        checkout_in_place(&repo, "v-shadow", Some("main"), None)
            .await
            .expect("checkout creates the branch");
        // `--abbrev-ref` would print `heads/v-shadow` here (the tag makes the
        // bare name ambiguous), so assert on the full symbolic ref instead.
        let head = run_git(&repo, &["symbolic-ref", "HEAD"])
            .await
            .expect("HEAD is attached to a branch, not detached on the tag");
        assert_eq!(head.trim(), "refs/heads/v-shadow");
        let _ = std::fs::remove_dir_all(&repo);
    }
}
