//! The Dashboard's recent work: what a session's worktrees gained since it
//! started, read from git and cached per worktree for [`TTL`].
//!
//! One member is one worktree — a worktree session's created worktree, a
//! workspace member's, or the repository's own checkout for an in-place
//! session. A member whose path is not a git worktree contributes nothing (a
//! missing path is logged, an existing folder that is not a repository is
//! not), so no read discovers an enclosing repository. A member whose read
//! fails is logged, contributes nothing and withholds the session's landed
//! totals; the drawer still answers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use protocol::{GitFileChange, RecentWork, SessionMember, WorkCommit, WorkFile, WorkTotals};
use tokio::time::Instant;
use tracing::warn;

use crate::dashboard::pr;
use crate::git_inspect;
use crate::sync::lock;

/// How long one worktree's read is reused.
const TTL: Duration = Duration::from_secs(5);

/// How many commits one session's detail shows.
const COMMITS_SHOWN: usize = 20;

/// How many files one session's detail shows.
const FILES_SHOWN: usize = 50;

/// An untracked file larger than this has no line count: reading it costs
/// more than the drawer is worth.
const UNTRACKED_MAX_BYTES: u64 = 1024 * 1024;

/// How far into an untracked file the binary probe reads.
const BINARY_PROBE_BYTES: usize = 8192;

/// The Dashboard's cache of worktree reads.
#[derive(Default)]
pub(crate) struct WorkCache {
    entries: Mutex<HashMap<Key, Cached>>,
}

/// What one worktree's read is cached under: the worktree and the session's
/// start, so a session restarted at the same path reads afresh.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    worktree: PathBuf,
    started_at: DateTime<Utc>,
}

/// One cached read and when it was made.
struct Cached {
    read: Arc<WorktreeRead>,
    at: Instant,
}

impl WorkCache {
    /// The cached read of `key`, when it is younger than [`TTL`].
    fn get(&self, key: &Key) -> Option<Arc<WorktreeRead>> {
        let entries = lock(&self.entries);
        let cached = entries.get(key)?;
        (Instant::now().duration_since(cached.at) < TTL).then(|| Arc::clone(&cached.read))
    }

    /// Cache `read` for `key`, dropping the entries that have expired.
    fn insert(&self, key: Key, read: Arc<WorktreeRead>) {
        let now = Instant::now();
        let mut entries = lock(&self.entries);
        entries.retain(|_, cached| now.duration_since(cached.at) < TTL);
        entries.insert(key, Cached { read, at: now });
    }
}

/// One worktree's read: its commits since the start, its uncommitted files
/// and what it landed. `failed` marks a read that errored.
#[derive(Debug, Default)]
struct WorktreeRead {
    failed: bool,
    commits: Vec<WorkCommit>,
    files: Vec<MemberFile>,
    landed: Option<WorkTotals>,
}

/// One uncommitted file of one member, before the session's sort and cut.
#[derive(Debug)]
struct MemberFile {
    path: String,
    status: String,
    /// git does not track it, so its lines are read from disk after the cut.
    untracked: bool,
    added: u32,
    deleted: u32,
}

/// One file the session's sort and cut kept.
struct KeptFile {
    /// The member it came from, and its path inside that member's worktree.
    member: usize,
    path_in_member: String,
    /// Its lines are still to be read from disk.
    untracked: bool,
    /// The file as the drawer shows it, its path already prefixed.
    view: WorkFile,
}

/// `members`' recent work as the one flat [`RecentWork`] the drawer shows.
pub(crate) async fn recent_work(
    cache: &WorkCache,
    prs: &pr::PrCache,
    members: &[SessionMember],
    started_at: DateTime<Utc>,
) -> RecentWork {
    let mut reads = Vec::with_capacity(members.len());
    for member in members {
        reads.push(worktree_read(cache, &member.worktree_path, started_at).await);
    }
    let (kept, files_total) = merged(members, &reads);
    RecentWork {
        commits: merged_commits(&reads),
        files: kept_files(members, kept).await,
        files_total,
        landed: landed(&reads),
        pull_request: pr::pull_request(prs, members).await,
        ..RecentWork::default()
    }
}

/// One member's read, from the cache when it is fresh.
async fn worktree_read(
    cache: &WorkCache,
    path: &str,
    started_at: DateTime<Utc>,
) -> Arc<WorktreeRead> {
    let key = Key {
        worktree: PathBuf::from(path),
        started_at,
    };
    if let Some(read) = cache.get(&key) {
        return read;
    }
    let read = Arc::new(read_worktree(&key.worktree, started_at).await);
    cache.insert(key, Arc::clone(&read));
    read
}

/// Read one worktree, after checking the path is one: a missing path is
/// logged, and an existing folder that is not a repository says nothing. A
/// read that fails is logged and marked failed.
async fn read_worktree(path: &Path, started_at: DateTime<Utc>) -> WorktreeRead {
    if !path.exists() {
        warn!(
            worktree = %path.display(),
            "dashboard: a session's worktree does not exist; it shows no work"
        );
        return WorktreeRead::default();
    }
    if !path.join(".git").exists() {
        return WorktreeRead::default();
    }
    match read_worktree_inner(path, started_at).await {
        Ok(read) => read,
        Err(err) => {
            warn!(
                worktree = %path.display(),
                %err,
                "dashboard: a session's worktree read failed; it shows no work"
            );
            WorktreeRead {
                failed: true,
                ..WorktreeRead::default()
            }
        }
    }
}

async fn read_worktree_inner(
    path: &Path,
    started_at: DateTime<Utc>,
) -> anyhow::Result<WorktreeRead> {
    let commits = git_inspect::commits_since(path, started_at).await?;
    let (index, worktree) = git_inspect::worktree_status(path).await?;
    let pending = pending_files(index, worktree);
    let numstat = git_inspect::numstat_against_head(path).await?;
    let files = member_files(&pending, &numstat);
    let landed = landed_totals(path, started_at, &commits, &files).await?;
    Ok(WorktreeRead {
        failed: false,
        commits,
        files,
        landed,
    })
}

/// What the worktree landed: only a clean tree with commits has any, and the
/// totals start at the oldest commit since the start.
async fn landed_totals(
    path: &Path,
    started_at: DateTime<Utc>,
    commits: &[WorkCommit],
    files: &[MemberFile],
) -> anyhow::Result<Option<WorkTotals>> {
    if !files.is_empty() || commits.is_empty() {
        return Ok(None);
    }
    let Some(oldest) = git_inspect::oldest_commit_since(path, started_at).await? else {
        return Ok(None);
    };
    Ok(Some(git_inspect::totals_since(path, &oldest).await?))
}

/// One uncommitted file before its line counts are known.
struct PendingFile {
    path: String,
    status: String,
    /// git does not track it, so its lines are read from disk.
    untracked: bool,
}

/// The worktree's uncommitted files from [`git_inspect::worktree_status`]'s
/// two buckets, merged by path: a path in both counts once, at its index
/// status, and an untracked file reads as added.
fn pending_files(index: Vec<GitFileChange>, worktree: Vec<GitFileChange>) -> Vec<PendingFile> {
    let mut files: HashMap<String, PendingFile> = HashMap::new();
    for change in index {
        files.insert(
            change.path.clone(),
            PendingFile {
                path: change.path,
                status: change.status,
                untracked: false,
            },
        );
    }
    for change in worktree {
        if files.contains_key(&change.path) {
            continue;
        }
        let untracked = change.status == "?";
        files.insert(
            change.path.clone(),
            PendingFile {
                path: change.path,
                status: if untracked {
                    "A".to_owned()
                } else {
                    change.status
                },
                untracked,
            },
        );
    }
    files.into_values().collect()
}

/// The files with the line counts git knows: a tracked file's from `numstat`,
/// an untracked file's starting at 0 until the cut says whether it is shown.
fn member_files(
    pending: &[PendingFile],
    numstat: &HashMap<String, git_inspect::LineCounts>,
) -> Vec<MemberFile> {
    pending
        .iter()
        .map(|file| {
            let counts = if file.untracked {
                git_inspect::LineCounts::default()
            } else {
                numstat.get(&file.path).copied().unwrap_or_default()
            };
            MemberFile {
                path: file.path.clone(),
                status: file.status.clone(),
                untracked: file.untracked,
                added: counts.added,
                deleted: counts.deleted,
            }
        })
        .collect()
}

/// The session's commits: every member's, newest first, cut at
/// [`COMMITS_SHOWN`].
fn merged_commits(reads: &[Arc<WorktreeRead>]) -> Vec<WorkCommit> {
    let mut commits: Vec<WorkCommit> = reads
        .iter()
        .flat_map(|read| read.commits.iter().cloned())
        .collect();
    commits.sort_by_key(|commit| std::cmp::Reverse(commit.at));
    commits.truncate(COMMITS_SHOWN);
    commits
}

/// The session's files in the drawer's order — by path, a workspace's paths
/// prefixed with their repo's name — cut at [`FILES_SHOWN`], with the count
/// taken before the cut.
fn merged(members: &[SessionMember], reads: &[Arc<WorktreeRead>]) -> (Vec<KeptFile>, u32) {
    let prefix = members.len() > 1;
    let mut kept: Vec<KeptFile> = Vec::new();
    for (index, (member, read)) in members.iter().zip(reads).enumerate() {
        kept.extend(read.files.iter().map(|file| KeptFile {
            member: index,
            path_in_member: file.path.clone(),
            untracked: file.untracked,
            view: WorkFile {
                path: shown_path(member, prefix, &file.path),
                status: file.status.clone(),
                added: file.added,
                deleted: file.deleted,
            },
        }));
    }
    kept.sort_by(|a, b| a.view.path.cmp(&b.view.path));
    let files_total = u32::try_from(kept.len()).unwrap_or(u32::MAX);
    kept.truncate(FILES_SHOWN);
    (kept, files_total)
}

/// `path` as the session's list shows it: a workspace prefixes it with the
/// member's repo name.
fn shown_path(member: &SessionMember, prefix: bool, path: &str) -> String {
    if prefix {
        format!("{}/{path}", member.repo_name)
    } else {
        path.to_owned()
    }
}

/// The drawer's files: only the untracked ones the cut kept are read for
/// their lines, so a folder of untracked files costs no more than is shown.
async fn kept_files(members: &[SessionMember], kept: Vec<KeptFile>) -> Vec<WorkFile> {
    let mut files = Vec::with_capacity(kept.len());
    for mut file in kept {
        if file.untracked {
            let path = Path::new(&members[file.member].worktree_path).join(&file.path_in_member);
            file.view.added = untracked_lines(&path).await;
        }
        files.push(file.view);
    }
    files
}

/// How many lines an untracked file adds: its line count, or 0 when it is not
/// a regular file, is too large, or looks binary.
async fn untracked_lines(path: &Path) -> u32 {
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return 0;
    };
    if !meta.is_file() || meta.len() > UNTRACKED_MAX_BYTES {
        return 0;
    }
    let Ok(bytes) = tokio::fs::read(path).await else {
        return 0;
    };
    if bytes.iter().take(BINARY_PROBE_BYTES).any(|&byte| byte == 0) {
        return 0;
    }
    // One chunk per line, so the chunks past the first count the newlines.
    let newlines = bytes.split(|&byte| byte == b'\n').count().saturating_sub(1);
    let lines = if bytes.last().is_some_and(|&last| last != b'\n') {
        newlines + 1
    } else {
        newlines
    };
    u32::try_from(lines).unwrap_or(u32::MAX)
}

/// The session's landed totals: no member failed, every member clean, and one
/// of them with totals.
fn landed(reads: &[Arc<WorktreeRead>]) -> Option<WorkTotals> {
    if reads
        .iter()
        .any(|read| read.failed || !read.files.is_empty())
    {
        return None;
    }
    let mut sum: Option<WorkTotals> = None;
    for total in reads.iter().filter_map(|read| read.landed.as_ref()) {
        let sum = sum.get_or_insert(WorkTotals {
            files: 0,
            added: 0,
            deleted: 0,
        });
        sum.files += total.files;
        sum.added += total.added;
        sum.deleted += total.deleted;
    }
    sum
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use super::*;
    use crate::file_fetch::test_support::TestDir;
    use crate::git_inspect::test_support::{commit_file, commit_file_at, git, init_repo};

    /// A member reading `path`, named `name`.
    fn member(path: &Path, name: &str) -> SessionMember {
        SessionMember {
            repo_id: "r1".to_owned(),
            repo_name: name.to_owned(),
            branch: "main".to_owned(),
            worktree_path: path.to_string_lossy().into_owned(),
        }
    }

    /// A pull-request cache that runs no `gh`: no fixture repository is a
    /// GitHub one, and no test spawns the real CLI.
    fn no_pull_requests() -> pr::PrCache {
        pr::PrCache::with_program(pr::NO_PROGRAM)
    }

    /// A start far enough back that every fixture commit is since it.
    fn long_ago() -> DateTime<Utc> {
        "2020-01-01T00:00:00Z".parse().expect("a valid time")
    }

    /// `path`'s work for a single-member session started at `started_at`.
    async fn work_of(path: &Path, started_at: DateTime<Utc>) -> RecentWork {
        recent_work(
            &WorkCache::default(),
            &no_pull_requests(),
            &[member(path, "repo")],
            started_at,
        )
        .await
    }

    fn paths(files: &[WorkFile]) -> Vec<&str> {
        files.iter().map(|file| file.path.as_str()).collect()
    }

    fn subjects(commits: &[WorkCommit]) -> Vec<&str> {
        commits
            .iter()
            .map(|commit| commit.subject.as_str())
            .collect()
    }

    /// The file at `path` in `work`, failing the test when it is missing.
    fn file<'a>(work: &'a RecentWork, path: &str) -> &'a WorkFile {
        assert!(
            work.files.iter().any(|file| file.path == path),
            "{path} is listed, in {:?}",
            paths(&work.files)
        );
        work.files
            .iter()
            .find(|file| file.path == path)
            .expect("checked just above")
    }

    #[tokio::test]
    async fn uncommitted_files_carry_status_and_line_counts() {
        let dir = init_repo("work-files").await;
        let repo = dir.path();
        commit_file(repo, "keep.txt", b"one\ntwo\nthree\n").await;
        commit_file(repo, "gone.txt", b"bye\n").await;
        std::fs::write(repo.join("keep.txt"), "one\nTWO\nthree\nfour\n").expect("edit file");
        std::fs::remove_file(repo.join("gone.txt")).expect("remove file");
        std::fs::write(repo.join("new.txt"), "alpha\nbeta\n").expect("write staged file");
        git(repo, &["add", "--", "new.txt"]).await;
        std::fs::write(repo.join("notes.txt"), "a\nb\nc\n").expect("write untracked file");

        let work = work_of(repo, long_ago()).await;
        assert_eq!(work.files_total, 4);
        assert_eq!(
            paths(&work.files),
            vec!["gone.txt", "keep.txt", "new.txt", "notes.txt"]
        );
        let counts = |path: &str| {
            let entry = file(&work, path);
            (entry.status.as_str(), entry.added, entry.deleted)
        };
        assert_eq!(counts("gone.txt"), ("D", 0, 1));
        assert_eq!(counts("keep.txt"), ("M", 2, 1));
        assert_eq!(counts("new.txt"), ("A", 2, 0));
        assert_eq!(counts("notes.txt"), ("A", 3, 0));
        assert_eq!(work.landed, None, "a dirty tree landed nothing");
    }

    #[tokio::test]
    async fn untracked_files_in_a_new_folder_are_listed_one_by_one_with_their_lines() {
        let dir = init_repo("work-folder").await;
        let repo = dir.path();
        commit_file(repo, "seed.txt", b"seed\n").await;
        let folder = repo.join("new");
        std::fs::create_dir_all(&folder).expect("create folder");
        std::fs::write(folder.join("a.txt"), "1\n2\n3\n").expect("write first file");
        std::fs::write(folder.join("b.txt"), "only\n").expect("write second file");

        let work = work_of(repo, long_ago()).await;
        assert_eq!(paths(&work.files), vec!["new/a.txt", "new/b.txt"]);
        assert_eq!(file(&work, "new/a.txt").status, "A");
        assert_eq!(file(&work, "new/a.txt").added, 3);
        assert_eq!(file(&work, "new/b.txt").status, "A");
        assert_eq!(file(&work, "new/b.txt").added, 1);
        assert_eq!(
            work.files_total, 2,
            "each file of the folder is its own entry"
        );
        assert_eq!(work.landed, None);
    }

    #[tokio::test]
    async fn a_clean_tree_with_commits_reports_landed_totals() {
        let dir = init_repo("work-landed").await;
        let repo = dir.path();
        commit_file_at(repo, "a.txt", b"one\ntwo\n", "2026-10-01T10:00:00+00:00").await;
        commit_file_at(repo, "b.txt", b"three\n", "2026-10-02T10:00:00+00:00").await;

        let work = work_of(repo, long_ago()).await;
        assert_eq!(subjects(&work.commits), vec!["add b.txt", "add a.txt"]);
        assert_eq!(work.files_total, 0);
        assert_eq!(
            work.landed,
            Some(WorkTotals {
                files: 2,
                added: 3,
                deleted: 0
            })
        );
    }

    #[tokio::test]
    async fn a_dirty_tree_reports_no_landed_totals() {
        let dir = init_repo("work-dirty").await;
        let repo = dir.path();
        commit_file_at(repo, "a.txt", b"one\n", "2026-10-01T10:00:00+00:00").await;
        std::fs::write(repo.join("draft.txt"), "wip\n").expect("write untracked file");

        let work = work_of(repo, long_ago()).await;
        assert_eq!(work.commits.len(), 1);
        assert_eq!(paths(&work.files), vec!["draft.txt"]);
        assert_eq!(work.landed, None);
    }

    #[tokio::test]
    async fn a_clean_tree_without_commits_is_nothing_yet() {
        let dir = init_repo("work-empty").await;
        let repo = dir.path();

        let work = work_of(repo, Utc::now()).await;
        assert_eq!(work, RecentWork::default());
    }

    #[tokio::test]
    async fn files_are_cut_at_fifty_with_the_full_count() {
        let dir = init_repo("work-cut").await;
        let repo = dir.path();
        for n in 0..51 {
            std::fs::write(repo.join(format!("f{n:02}.txt")), "x\ny\n").expect("write file");
        }

        let work = work_of(repo, long_ago()).await;
        assert_eq!(work.files_total, 51, "the count is taken before the cut");
        assert_eq!(work.files.len(), 50);
        assert_eq!(work.files[0].path, "f00.txt");
        assert_eq!(work.files[49].path, "f49.txt");
        assert!(
            work.files
                .iter()
                .all(|file| { file.status == "A" && file.added == 2 && file.deleted == 0 }),
            "every kept file carries the count of its own content, read after the cut"
        );
    }

    #[tokio::test]
    async fn a_workspace_session_prefixes_paths_with_the_repo_name() {
        let alpha_dir = init_repo("work-alpha").await;
        let beta_dir = init_repo("work-beta").await;
        let alpha = alpha_dir.path();
        let beta = beta_dir.path();
        commit_file_at(alpha, "a.txt", b"a\n", "2026-10-01T10:00:00+00:00").await;
        commit_file_at(beta, "b.txt", b"b\n", "2026-10-02T10:00:00+00:00").await;
        std::fs::write(alpha.join("draft.txt"), "wa\n").expect("write untracked file");
        std::fs::write(beta.join("draft.txt"), "wb\n").expect("write untracked file");

        let work = recent_work(
            &WorkCache::default(),
            &no_pull_requests(),
            &[member(alpha, "alpha"), member(beta, "beta")],
            long_ago(),
        )
        .await;
        assert_eq!(
            paths(&work.files),
            vec!["alpha/draft.txt", "beta/draft.txt"],
            "each member's paths carry its repo name"
        );
        assert_eq!(work.files_total, 2);
        assert_eq!(subjects(&work.commits), vec!["add b.txt", "add a.txt"]);
        assert_eq!(work.landed, None, "one dirty member lands nothing");
    }

    #[tokio::test]
    async fn a_failed_member_withholds_landed_totals() {
        let dir = init_repo("work-failed").await;
        let repo = dir.path();
        commit_file_at(repo, "a.txt", b"one\n", "2026-10-01T10:00:00+00:00").await;
        let broken_dir = TestDir::new("work-failed-member");
        let broken = broken_dir.path();
        // A `.git` link pointing at nothing: the path looks like a worktree
        // and every git read on it fails.
        std::fs::write(broken.join(".git"), "gitdir: nowhere\n").expect("write a git link");

        let work = recent_work(
            &WorkCache::default(),
            &no_pull_requests(),
            &[member(repo, "repo"), member(broken, "broken")],
            long_ago(),
        )
        .await;
        assert_eq!(
            subjects(&work.commits),
            vec!["add a.txt"],
            "the readable member still reads"
        );
        assert_eq!(
            work.landed, None,
            "a failed member withholds the landed totals"
        );
    }

    #[tokio::test]
    async fn an_in_place_session_reads_the_repos_own_checkout() {
        let dir = init_repo("work-in-place").await;
        let repo = dir.path();
        commit_file_at(repo, "a.txt", b"one\n", "2026-10-01T10:00:00+00:00").await;
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").expect("edit file");
        let started = "2026-09-01T00:00:00Z".parse().expect("a valid time");

        let work = work_of(repo, started).await;
        assert_eq!(subjects(&work.commits), vec!["add a.txt"]);
        assert_eq!(paths(&work.files), vec!["a.txt"]);
        assert_eq!(file(&work, "a.txt").added, 1);
        assert_eq!(file(&work, "a.txt").deleted, 0);
        assert_eq!(work.landed, None);
    }

    #[tokio::test]
    async fn a_path_that_is_not_a_repo_gives_empty_work() {
        let plain_dir = TestDir::new("work-plain");
        std::fs::write(plain_dir.path().join("notes.txt"), "a\n").expect("write a file");

        let work = work_of(plain_dir.path(), long_ago()).await;
        assert_eq!(work, RecentWork::default());
    }

    #[tokio::test]
    async fn a_plain_folder_inside_a_repo_reports_no_work() {
        let dir = init_repo("work-plain-in-repo").await;
        let repo = dir.path();
        commit_file(repo, "seed.txt", b"seed\n").await;
        let plain = repo.join("plain");
        std::fs::create_dir_all(&plain).expect("create plain folder");
        std::fs::write(plain.join("notes.txt"), "a\n").expect("write a file");

        let work = work_of(&plain, long_ago()).await;
        assert_eq!(
            work,
            RecentWork::default(),
            "no read reaches the enclosing repository"
        );
    }

    #[tokio::test]
    async fn the_read_is_cached_for_five_seconds() {
        let dir = init_repo("work-cache").await;
        let repo = dir.path();
        commit_file_at(repo, "a.txt", b"one\n", "2026-10-01T10:00:00+00:00").await;
        let cache = WorkCache::default();
        let prs = no_pull_requests();
        let members = [member(repo, "repo")];
        let first = recent_work(&cache, &prs, &members, long_ago()).await;
        assert_eq!(subjects(&first.commits), vec!["add a.txt"]);

        commit_file_at(repo, "b.txt", b"two\n", "2026-10-02T10:00:00+00:00").await;
        let cached = recent_work(&cache, &prs, &members, long_ago()).await;
        assert_eq!(cached, first, "a read inside five seconds is reused");

        expire(&cache);
        let fresh = recent_work(&cache, &prs, &members, long_ago()).await;
        assert_eq!(
            subjects(&fresh.commits),
            vec!["add b.txt", "add a.txt"],
            "a read past five seconds sees the new commit"
        );
    }

    /// Make every cached entry [`TTL`] old, as five seconds of real time
    /// would. The cache test runs on the real clock: a paused one would make
    /// the read's own 10 s timeout fire before git finished.
    fn expire(cache: &WorkCache) {
        let now = Instant::now();
        let mut entries = lock(&cache.entries);
        for cached in entries.values_mut() {
            cached.at = now - TTL;
        }
    }
}
