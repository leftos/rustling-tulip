//! Branch-name suggestions for new sessions.
//!
//! A session branch is named `wt/<adjective>-<noun>` out of a 16 × 16 pool.
//! Picking from that pool blindly re-draws a name a discarded session left
//! behind after a few dozen sessions, and a spawn that lands on a leftover
//! either attaches months-old code or is refused outright. The daemon owns
//! the draw because only it can check the name against every member repo's
//! refs — local and remote — and against the worktree directories already on
//! disk, before handing it to a client.

use crate::git;
use crate::state::AppState;
use anyhow::anyhow;
use protocol::SuggestTarget;
use rand::seq::SliceRandom as _;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tracing::warn;

const ADJECTIVES: [&str; 16] = [
    "sleepy", "brave", "calm", "eager", "gentle", "happy", "jolly", "kind", "lively", "mighty",
    "nimble", "polite", "quick", "silly", "witty", "zen",
];

const NOUNS: [&str; 16] = [
    "otter", "panda", "lynx", "fox", "hawk", "raven", "tiger", "whale", "koala", "ferret",
    "badger", "marten", "lemur", "weasel", "gibbon", "gecko",
];

/// Random draws tried before falling back to an exhaustive scan. Cheap enough
/// to always attempt, and it keeps the common case (a mostly-empty pool) from
/// materializing all 256 combinations.
const RANDOM_DRAWS: u32 = 32;

fn compose(adjective: &str, noun: &str) -> String {
    format!("wt/{adjective}-{noun}")
}

/// A `wt/<adjective>-<noun>` name that is neither in `taken` nor rejected by
/// `dir_is_free`.
///
/// `taken` is the set of names git already knows; `dir_is_free` answers the
/// second question — whether a worktree directory for that name is sitting on
/// disk — and is only asked about names that cleared `taken`, so the common
/// case costs one filesystem check rather than 256. Keeping it a predicate
/// also keeps this function testable without git or a worktrees root.
///
/// Random draws first, then a shuffled scan of the whole pool so a nearly-full
/// pool still yields its last free name instead of degrading into retries.
/// When every combination is spoken for the name grows a `-<n>` suffix, the
/// smallest free one for a randomly chosen pair.
pub fn pick_free_name(
    taken: &HashSet<String>,
    rng: &mut impl rand::Rng,
    mut dir_is_free: impl FnMut(&str) -> bool,
) -> String {
    let mut is_free = |name: &str| !taken.contains(name) && dir_is_free(name);

    for _ in 0..RANDOM_DRAWS {
        let name = compose(
            ADJECTIVES[rng.gen_range(0..ADJECTIVES.len())],
            NOUNS[rng.gen_range(0..NOUNS.len())],
        );
        if is_free(&name) {
            return name;
        }
    }

    let mut combinations: Vec<(usize, usize)> = (0..ADJECTIVES.len())
        .flat_map(|a| (0..NOUNS.len()).map(move |n| (a, n)))
        .collect();
    combinations.shuffle(rng);
    for (a, n) in combinations {
        let name = compose(ADJECTIVES[a], NOUNS[n]);
        if is_free(&name) {
            return name;
        }
    }

    // Every combination is spoken for. Suffix a random pair with the smallest
    // free number; `taken` is finite, so one of the first `taken.len() + 1`
    // candidates clears it, and a directory blocking every one of those on top
    // of that is not a state a repo can reach.
    let stem = compose(
        ADJECTIVES[rng.gen_range(0..ADJECTIVES.len())],
        NOUNS[rng.gen_range(0..NOUNS.len())],
    );
    let limit = taken.len().saturating_add(2);
    (2..=limit)
        .map(|n| format!("{stem}-{n}"))
        .find(|name| is_free(name))
        .unwrap_or_else(|| format!("{stem}-{}", limit.saturating_add(1)))
}

/// Every branch name already spoken for across `repo_paths`: local branches
/// plus remote-tracking ones with their `<remote>/` prefix stripped, because
/// an `origin/wt/x` that no local branch mirrors still becomes `wt/x` the
/// moment someone checks it out.
///
/// A repo whose git calls fail contributes nothing rather than aborting the
/// suggestion — a suggestion is worth giving from partial knowledge, and the
/// spawn itself still refuses a leftover it finds.
pub async fn taken_names(repo_paths: &[PathBuf]) -> HashSet<String> {
    let mut taken = HashSet::new();
    for repo in repo_paths {
        match git::list_branches(repo).await {
            Ok(branches) => taken.extend(branches),
            Err(err) => {
                warn!(?err, repo = %repo.display(), "listing local branches failed; treating as none");
            }
        }
        let remotes = git::remotes(repo).await;
        match git::list_remote_branches(repo).await {
            Ok(branches) => {
                for branch in branches {
                    taken.insert(strip_remote_prefix(&branch, &remotes));
                }
            }
            Err(err) => {
                warn!(?err, repo = %repo.display(), "listing remote branches failed; treating as none");
            }
        }
    }
    taken
}

/// `origin/wt/x` → `wt/x`. Only configured remote names are stripped, so a
/// local branch that genuinely starts with a slash-separated word is left
/// alone.
fn strip_remote_prefix(branch: &str, remotes: &[String]) -> String {
    remotes
        .iter()
        .find_map(|remote| branch.strip_prefix(&format!("{remote}/")))
        .map_or_else(|| branch.to_string(), str::to_string)
}

/// Suggest a branch name free in every repo behind `target`.
///
/// "Free" covers both halves of what a spawn refuses: no member repo has the
/// branch, and no member's derived worktree directory exists under
/// `worktrees_root`. A directory left behind without its branch would
/// otherwise get the name refused at launch.
pub async fn suggest(
    state: &AppState,
    worktrees_root: &Path,
    target: &SuggestTarget,
) -> anyhow::Result<String> {
    let repo_paths = target_repo_paths(state, target)?;
    let taken = taken_names(&repo_paths).await;
    let repo_refs: Vec<&Path> = repo_paths.iter().map(PathBuf::as_path).collect();
    Ok(pick_free_name(
        &taken,
        &mut rand::thread_rng(),
        |candidate| !worktree_dir_exists(worktrees_root, &repo_refs, candidate),
    ))
}

/// Whether any member's worktree directory for `branch` already exists.
///
/// Derived through [`git::workspace_worktree_paths`] — the same function the
/// spawn uses — so the check can't disagree with the path the spawn would
/// pick.
fn worktree_dir_exists(worktrees_root: &Path, repo_paths: &[&Path], branch: &str) -> bool {
    git::workspace_worktree_paths(worktrees_root, repo_paths, branch)
        .iter()
        .any(|path| path.exists())
}

/// Repo directories a suggestion has to clear: the one repo, or every
/// registered member of the workspace.
fn target_repo_paths(state: &AppState, target: &SuggestTarget) -> anyhow::Result<Vec<PathBuf>> {
    match target {
        SuggestTarget::Repo { repo_id } => state
            .with_persisted(|s| {
                s.repos
                    .iter()
                    .find(|r| &r.id == repo_id)
                    .map(|r| vec![PathBuf::from(&r.path)])
            })
            .ok_or_else(|| anyhow!("unknown repo: {repo_id}")),
        SuggestTarget::Workspace { workspace_id } => {
            let paths = state
                .with_persisted(|s| {
                    s.workspaces
                        .iter()
                        .find(|w| &w.id == workspace_id)
                        .map(|ws| {
                            ws.member_repo_ids
                                .iter()
                                .filter_map(|id| s.repos.iter().find(|r| &r.id == id))
                                .map(|r| PathBuf::from(&r.path))
                                .collect::<Vec<_>>()
                        })
                })
                .ok_or_else(|| anyhow!("unknown workspace: {workspace_id}"))?;
            if paths.is_empty() {
                return Err(anyhow!(
                    "workspace {workspace_id} has no registered member repos"
                ));
            }
            Ok(paths)
        }
        SuggestTarget::Unknown => Err(anyhow!("unsupported branch-name suggestion target")),
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::paths::Dirs;
    use protocol::{AppearanceOverrides, RepoEntry, WorkspaceEntry};
    use rand::SeedableRng as _;
    use rand::rngs::StdRng;
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    fn all_names() -> Vec<String> {
        ADJECTIVES
            .iter()
            .flat_map(|a| NOUNS.iter().map(move |n| compose(a, n)))
            .collect()
    }

    #[test]
    fn picks_the_one_free_name_out_of_a_full_pool() {
        let free = compose("witty", "gecko");
        let taken: HashSet<String> = all_names().into_iter().filter(|n| n != &free).collect();
        assert_eq!(taken.len(), 255);

        let mut rng = StdRng::seed_from_u64(7);
        assert_eq!(pick_free_name(&taken, &mut rng, |_| true), free);
    }

    #[test]
    fn suffixes_when_every_combination_is_taken() {
        let taken: HashSet<String> = all_names().into_iter().collect();
        let mut rng = StdRng::seed_from_u64(11);

        let name = pick_free_name(&taken, &mut rng, |_| true);
        assert!(name.ends_with("-2"), "expected a -2 suffix, got {name}");
        assert!(
            all_names().contains(&name.trim_end_matches("-2").to_string()),
            "suffixed name must be built from the pool: {name}"
        );
        assert!(!taken.contains(&name));
    }

    #[test]
    fn skips_a_taken_suffix() {
        let mut taken: HashSet<String> = all_names().into_iter().collect();
        for name in all_names() {
            taken.insert(format!("{name}-2"));
        }
        let mut rng = StdRng::seed_from_u64(13);

        let name = pick_free_name(&taken, &mut rng, |_| true);
        assert!(name.ends_with("-3"), "expected a -3 suffix, got {name}");
        assert!(!taken.contains(&name));
    }

    // --- git-backed fixtures -------------------------------------------

    fn git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo on `main` with one commit, git identity configured, and signing
    /// off so a global `commit.gpgsign` can't block the commit.
    fn init_repo(root: &Path, name: &str) -> PathBuf {
        let repo = root.join(name);
        std::fs::create_dir_all(&repo).expect("create repo dir");
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("seed.txt"), "x\n").expect("write file");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "seed"]);
        repo
    }

    /// Give `repo` a bare `origin` alongside it and push `branch` there, so
    /// the repo carries `refs/remotes/origin/<branch>` with no local branch of
    /// that name.
    fn push_to_new_origin(root: &Path, repo: &Path, branch: &str) {
        let origin = root.join("origin.git");
        std::fs::create_dir_all(&origin).expect("create origin dir");
        git(&origin, &["init", "--bare"]);
        git(
            repo,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        );
        git(repo, &["branch", branch]);
        git(repo, &["push", "origin", branch]);
        git(repo, &["branch", "-D", branch]);
        git(repo, &["fetch", "origin"]);
    }

    /// Create many branches in one git process — 250-odd `git branch` spawns
    /// would dominate the test's runtime on Windows.
    fn create_branches(repo: &Path, names: &[String]) {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["update-ref", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn git update-ref");
        {
            let mut stdin = child.stdin.take().expect("update-ref stdin");
            for name in names {
                writeln!(stdin, "create refs/heads/{name} HEAD").expect("write ref update");
            }
        }
        let out = child.wait_with_output().expect("wait for git update-ref");
        assert!(
            out.status.success(),
            "git update-ref failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn scratch_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rt-names-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch root");
        root
    }

    #[tokio::test]
    async fn taken_names_unions_repos_and_strips_the_remote_prefix() {
        let root = scratch_root("taken");
        let first = init_repo(&root, "first");
        let second = init_repo(&root, "second");
        create_branches(&first, &[compose("brave", "otter")]);
        create_branches(&second, &[compose("calm", "lynx")]);
        push_to_new_origin(&root, &first, &compose("silly", "hawk"));

        let taken = taken_names(&[first.clone(), second.clone()]).await;

        assert!(taken.contains(&compose("brave", "otter")), "{taken:?}");
        assert!(taken.contains(&compose("calm", "lynx")), "{taken:?}");
        assert!(
            taken.contains(&compose("silly", "hawk")),
            "a remote-only branch must block its bare name: {taken:?}"
        );
        assert!(
            !taken.iter().any(|n| n.starts_with("origin/")),
            "the remote prefix must be stripped: {taken:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    fn test_dirs(root: &Path) -> Dirs {
        Dirs {
            config: root.to_path_buf(),
            state_file: root.join("state.json"),
            handshake_file: root.join("daemon.json"),
            lan_config_file: root.join("lan.json"),
            lan_cert_file: root.join("lan-cert.pem"),
            lan_key_file: root.join("lan-key.pem"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
        }
    }

    fn repo_entry(id: &str, path: &Path) -> RepoEntry {
        RepoEntry {
            id: id.to_string(),
            name: id.to_string(),
            path: path.to_string_lossy().into_owned(),
            default_branch: Some("main".to_string()),
            default_use_worktree: true,
            appearance: AppearanceOverrides::default(),
            last_agent: None,
            last_spawn_config: None,
        }
    }

    /// The point of asking the daemon: a name free in the first member but
    /// taken in the second must not be suggested.
    #[tokio::test]
    async fn suggest_excludes_a_branch_only_the_second_member_has() {
        let root = scratch_root("suggest-ws");
        let first = init_repo(&root, "first");
        let second = init_repo(&root, "second");

        // Leave exactly two names free after the first member, then let the
        // second member claim one of them.
        let free_in_first = compose("witty", "gecko");
        let only_in_second = compose("zen", "gibbon");
        let pool = all_names();
        let seeded: Vec<String> = pool
            .iter()
            .filter(|n| *n != &free_in_first && *n != &only_in_second)
            .cloned()
            .collect();
        create_branches(&first, &seeded);
        create_branches(&second, std::slice::from_ref(&only_in_second));

        let state = AppState::load_or_default(&test_dirs(&root)).expect("load state");
        state
            .mutate(|s| {
                s.repos.push(repo_entry("r1", &first));
                s.repos.push(repo_entry("r2", &second));
                s.workspaces.push(WorkspaceEntry {
                    id: "ws1".to_string(),
                    name: "ws".to_string(),
                    member_repo_ids: vec!["r1".to_string(), "r2".to_string()],
                    linked_vscode_workspace: None,
                    default_use_worktree: true,
                    appearance: AppearanceOverrides::default(),
                    last_spawn_config: None,
                });
            })
            .expect("seed state");

        let target = SuggestTarget::Workspace {
            workspace_id: "ws1".to_string(),
        };
        let name = suggest(&state, &state.worktrees_dir(), &target)
            .await
            .expect("suggestion");
        assert_eq!(
            name, free_in_first,
            "the only name free in both members must be the one suggested"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A worktree directory whose branch is gone is still a leftover: the
    /// spawn refuses it, so the suggestion must not hand out that name.
    #[tokio::test]
    async fn suggest_skips_a_name_whose_worktree_directory_exists() {
        let root = scratch_root("suggest-dir");
        let repo = init_repo(&root, "only");

        // Every pool name but one is a branch; the last one is free in git and
        // blocked only by the directory seeded below.
        let blocked = compose("witty", "gecko");
        let seeded: Vec<String> = all_names().into_iter().filter(|n| n != &blocked).collect();
        create_branches(&repo, &seeded);

        let state = AppState::load_or_default(&test_dirs(&root)).expect("load state");
        state
            .mutate(|s| s.repos.push(repo_entry("r1", &repo)))
            .expect("seed state");
        let worktrees_root = state.worktrees_dir();
        for path in git::workspace_worktree_paths(&worktrees_root, &[repo.as_path()], &blocked) {
            std::fs::create_dir_all(&path).expect("seed leftover worktree dir");
        }

        let target = SuggestTarget::Repo {
            repo_id: "r1".to_string(),
        };
        let name = suggest(&state, &worktrees_root, &target)
            .await
            .expect("suggestion");

        assert_ne!(
            name, blocked,
            "a name whose worktree directory is on disk must not be suggested"
        );
        assert!(
            !worktree_dir_exists(&worktrees_root, &[repo.as_path()], &name),
            "the suggested name must have no worktree directory: {name}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
