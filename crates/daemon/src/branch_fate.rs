//! What a discard would do to a session's branch, and why.
//!
//! Two questions live here: which refs a session branch counts as "landed"
//! into, and — given those refs — whether the daemon would delete the branch,
//! keep it, or refuse to touch it. The discard handler and the preview that
//! feeds the confirm dialog both resolve through this module, so the dialog
//! can never promise an outcome the discard won't honor.

use crate::git;
use crate::paths::normalize_path_key;
use crate::spawn_plan;
use crate::state::AppState;
use protocol::{BranchFate, RepoEntry, SpawnConfig, SpawnTarget, UntouchedReason};
use std::path::Path;
use tracing::warn;

/// What a session's spawn config says about the base its branch forked from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionBase {
    /// Repo-backed session. `Some` carries an explicitly requested base;
    /// `None` means the daemon picked the repo's default and will re-derive
    /// it the same way now.
    Repo(Option<String>),
    /// A standalone shell — no repo members, so no branch has a fate.
    Standalone,
}

/// Read the base branch out of a session's spawn config. A session with no
/// stored config (a sidecar written before spawn configs were persisted) is
/// treated as repo-backed with no explicit base, which re-derives the repo
/// default rather than guessing.
#[must_use]
pub fn session_base(config: Option<&SpawnConfig>) -> SessionBase {
    match config.map(|cfg| &cfg.target) {
        Some(
            SpawnTarget::Single { base_branch, .. } | SpawnTarget::Workspace { base_branch, .. },
        ) => SessionBase::Repo(base_branch.clone()),
        Some(SpawnTarget::Standalone { .. }) => SessionBase::Standalone,
        None => SessionBase::Repo(None),
    }
}

/// Refs a session branch counts as landed into: the resolved base and its
/// remote-tracking counterpart, remote first, deduped, existing refs only.
///
/// Remote leads because that is the ref the work actually lands on in a repo
/// driven through worktrees — the primary tree's `main` sits wherever it was
/// last checked out by hand and would report every merged branch as unmerged.
/// An empty result means nothing can be measured; callers keep the branch.
pub async fn merge_targets(
    state: &AppState,
    repo: &RepoEntry,
    explicit_base: Option<&str>,
) -> Vec<String> {
    let repo_path = Path::new(&repo.path);
    let local = spawn_plan::local_base_name(state, repo, explicit_base).await;
    let mut targets: Vec<String> = Vec::new();

    // `remote_tracking_for` returns the base unchanged when it already names
    // a remote ref, so `origin/main` never compounds into `origin/origin/main`.
    if let Some(remote) = git::remote_tracking_for(repo_path, &local).await {
        targets.push(remote);
    }
    if git::full_ref_exists(repo_path, &format!("refs/heads/{local}")).await
        && !targets.iter().any(|t| t == &local)
    {
        targets.push(local);
    }
    targets
}

/// Everything [`member_branch_fate`] needs about one member of a session.
pub struct FateInput<'a> {
    /// Registered repo the member belongs to.
    pub repo_path: &'a Path,
    /// Working tree the session ran in.
    pub member_worktree: &'a Path,
    /// Daemon-owned worktrees root; anything outside it is off-limits.
    pub worktrees_root: &'a Path,
    /// Branch the session checked out in `member_worktree`.
    pub branch: &'a str,
    /// Refs from [`merge_targets`], in priority order.
    pub targets: &'a [String],
}

/// Decide one member's branch fate under [`protocol::BranchCleanup::Auto`].
///
/// The three `Untouched` reasons are hard refusals: they hold under every
/// cleanup mode, because deleting there would either destroy a branch the
/// daemon does not own or fail outright. Only when none of them applies does
/// the merge measurement decide between delete and keep.
pub async fn member_branch_fate(input: FateInput<'_>) -> BranchFate {
    if !input.member_worktree.starts_with(input.worktrees_root) {
        return BranchFate::Untouched {
            reason: UntouchedReason::ExternalWorktree,
        };
    }
    if let Some(holder) = git::worktree_holding_branch(input.repo_path, input.branch).await
        && normalize_path_key(&holder.to_string_lossy())
            != normalize_path_key(&input.member_worktree.to_string_lossy())
    {
        return BranchFate::Untouched {
            reason: UntouchedReason::CheckedOutElsewhere,
        };
    }
    if !git::full_ref_exists(input.repo_path, &format!("refs/heads/{}", input.branch)).await {
        return BranchFate::Untouched {
            reason: UntouchedReason::BranchMissing,
        };
    }

    match git::branch_merge_status(input.repo_path, input.branch, input.targets).await {
        Ok(git::BranchMergeStatus::Merged { into, via }) => BranchFate::WillDelete { into, via },
        Ok(git::BranchMergeStatus::Unmerged { unique_commits }) => BranchFate::KeptByDefault {
            unique_commits: Some(unique_commits),
            checked_against: input.targets.to_vec(),
        },
        Err(err) => {
            warn!(
                ?err,
                branch = input.branch,
                repo = %input.repo_path.display(),
                "branch fate: merge check failed; keeping the branch"
            );
            BranchFate::KeptByDefault {
                unique_commits: None,
                checked_against: input.targets.to_vec(),
            }
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
    use crate::paths::Dirs;
    use std::path::PathBuf;
    use std::process::Stdio;

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

    fn empty_state(root: &Path) -> AppState {
        AppState::load_or_default(&test_dirs(root)).expect("load state")
    }

    async fn git(repo: &Path, args: &[&str]) {
        let status = tokio::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .expect("spawn git");
        assert!(
            status.success(),
            "git {args:?} failed in {}",
            repo.display()
        );
    }

    /// Throwaway repo on `main` with one commit. `with_remote` adds a bare
    /// origin and pushes, so `origin/main` exists.
    async fn scratch_repo(tag: &str, with_remote: bool) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rt-fate-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create repo dir");
        git(&root, &["init"]).await;
        git(&root, &["config", "user.email", "t@example.com"]).await;
        git(&root, &["config", "user.name", "Test"]).await;
        git(&root, &["config", "commit.gpgsign", "false"]).await;
        std::fs::write(root.join("README.md"), "init\n").expect("seed file");
        git(&root, &["add", "."]).await;
        git(&root, &["commit", "-m", "init"]).await;
        git(&root, &["branch", "-M", "main"]).await;

        if with_remote {
            let origin = root.with_file_name(format!(
                "{}-origin",
                root.file_name()
                    .expect("repo leaf")
                    .to_string_lossy()
                    .into_owned()
            ));
            let _ = std::fs::remove_dir_all(&origin);
            std::fs::create_dir_all(&origin).expect("create origin dir");
            git(&origin, &["init", "--bare"]).await;
            let origin_str = origin.to_string_lossy().into_owned();
            git(&root, &["remote", "add", "origin", &origin_str]).await;
            git(&root, &["push", "-u", "origin", "main"]).await;
        }
        root
    }

    fn cleanup(repo: &Path) {
        let _ = std::fs::remove_dir_all(repo);
        let leaf = repo
            .file_name()
            .expect("repo leaf")
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_dir_all(repo.with_file_name(format!("{leaf}-origin")));
    }

    fn repo_entry(path: &Path, default_branch: Option<&str>) -> RepoEntry {
        RepoEntry {
            id: "r1".to_string(),
            name: "repo".to_string(),
            path: path.to_string_lossy().into_owned(),
            default_branch: default_branch.map(str::to_string),
            default_use_worktree: true,
            appearance: protocol::AppearanceOverrides::default(),
            last_agent: None,
            last_spawn_config: None,
        }
    }

    #[tokio::test]
    async fn merge_targets_leads_with_the_remote_counterpart() {
        let repo = scratch_repo("targets-remote", true).await;
        let state = empty_state(&repo.join("cfg"));
        let entry = repo_entry(&repo, Some("main"));
        assert_eq!(
            merge_targets(&state, &entry, None).await,
            vec!["origin/main".to_string(), "main".to_string()]
        );
        cleanup(&repo);
    }

    #[tokio::test]
    async fn merge_targets_falls_back_to_the_local_base_alone() {
        let repo = scratch_repo("targets-local", false).await;
        let state = empty_state(&repo.join("cfg"));
        // No persisted default either: the detection chain has to find `main`.
        let entry = repo_entry(&repo, None);
        assert_eq!(
            merge_targets(&state, &entry, None).await,
            vec!["main".to_string()]
        );
        cleanup(&repo);
    }

    #[tokio::test]
    async fn merge_targets_does_not_duplicate_an_explicit_remote_base() {
        let repo = scratch_repo("targets-explicit", true).await;
        let state = empty_state(&repo.join("cfg"));
        let entry = repo_entry(&repo, Some("main"));
        assert_eq!(
            merge_targets(&state, &entry, Some("origin/main")).await,
            vec!["origin/main".to_string()],
            "an explicit remote base must not compound or repeat"
        );
        cleanup(&repo);
    }

    #[tokio::test]
    async fn member_branch_fate_refuses_a_worktree_outside_the_root() {
        let repo = scratch_repo("fate-external", false).await;
        let fate = member_branch_fate(FateInput {
            repo_path: &repo,
            member_worktree: &repo,
            worktrees_root: &repo.join("worktrees"),
            branch: "main",
            targets: &["main".to_string()],
        })
        .await;
        assert_eq!(
            fate,
            BranchFate::Untouched {
                reason: UntouchedReason::ExternalWorktree
            }
        );
        cleanup(&repo);
    }

    #[tokio::test]
    async fn member_branch_fate_refuses_a_branch_another_worktree_holds() {
        let repo = scratch_repo("fate-elsewhere", false).await;
        let root = repo.join("wt-root");
        let held = root.join("held");
        let member = root.join("member");
        std::fs::create_dir_all(&member).expect("member dir");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "wt/held",
                &held.to_string_lossy(),
                "main",
            ],
        )
        .await;

        let fate = member_branch_fate(FateInput {
            repo_path: &repo,
            member_worktree: &member,
            worktrees_root: &root,
            branch: "wt/held",
            targets: &["main".to_string()],
        })
        .await;
        assert_eq!(
            fate,
            BranchFate::Untouched {
                reason: UntouchedReason::CheckedOutElsewhere
            }
        );
        cleanup(&repo);
    }

    #[tokio::test]
    async fn member_branch_fate_reports_a_missing_branch() {
        let repo = scratch_repo("fate-missing", false).await;
        let root = repo.join("wt-root");
        let member = root.join("member");
        std::fs::create_dir_all(&member).expect("member dir");
        let fate = member_branch_fate(FateInput {
            repo_path: &repo,
            member_worktree: &member,
            worktrees_root: &root,
            branch: "wt/never-existed",
            targets: &["main".to_string()],
        })
        .await;
        assert_eq!(
            fate,
            BranchFate::Untouched {
                reason: UntouchedReason::BranchMissing
            }
        );
        cleanup(&repo);
    }

    #[tokio::test]
    async fn member_branch_fate_keeps_a_branch_with_unique_work() {
        let repo = scratch_repo("fate-unique", false).await;
        let root = repo.join("wt-root");
        let member = root.join("member");
        std::fs::create_dir_all(&member).expect("member dir");
        git(&repo, &["checkout", "-b", "wt/unique"]).await;
        std::fs::write(repo.join("only.txt"), "here\n").expect("write");
        git(&repo, &["add", "."]).await;
        git(&repo, &["commit", "-m", "unique"]).await;
        git(&repo, &["checkout", "main"]).await;

        let targets = vec!["main".to_string()];
        let fate = member_branch_fate(FateInput {
            repo_path: &repo,
            member_worktree: &member,
            worktrees_root: &root,
            branch: "wt/unique",
            targets: &targets,
        })
        .await;
        assert_eq!(
            fate,
            BranchFate::KeptByDefault {
                unique_commits: Some(1),
                checked_against: targets,
            }
        );
        cleanup(&repo);
    }

    #[test]
    fn session_base_reads_the_spawn_target() {
        let single = SpawnConfig {
            target: SpawnTarget::Single {
                repo_id: "r1".to_string(),
                branch_name: "wt/x".to_string(),
                base_branch: Some("develop".to_string()),
                use_worktree: true,
                checkout_strategy: None,
                worktree_reuse: protocol::WorktreeReusePolicy::default(),
                existing_worktree: None,
            },
            mode: protocol::SessionMode::Interactive,
            dangerously_skip_permissions: false,
            agent_options: protocol::AgentOptions::default(),
            model: None,
            extra_env: Vec::new(),
        };
        assert_eq!(
            session_base(Some(&single)),
            SessionBase::Repo(Some("develop".to_string()))
        );

        let standalone = SpawnConfig {
            target: SpawnTarget::Standalone { cwd: None },
            ..single
        };
        assert_eq!(session_base(Some(&standalone)), SessionBase::Standalone);
        assert_eq!(session_base(None), SessionBase::Repo(None));
    }
}
