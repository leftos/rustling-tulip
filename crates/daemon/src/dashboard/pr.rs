//! The Dashboard's pull-request line: the pull request a session's branch has
//! open or landed, read from `gh` in its worktree and cached per branch for
//! [`TTL`].
//!
//! `gh` is read, never required: a machine without it, a folder that is not a
//! GitHub repository, a run that fails and output that does not parse all give
//! no pull request, so the drawer still answers. A member whose branch is empty
//! is never read, and a workspace takes the first member that has one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use protocol::{PullRequestChecks, PullRequestState, SessionMember, WorkPullRequest};
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::Instant;
use tracing::debug;

use crate::sync::lock;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The program the daemon reads pull requests with.
const PROGRAM: &str = "gh";

/// A test's stand-in for [`PROGRAM`]: one that does not exist, so no test
/// spawns the real `gh`.
#[cfg(test)]
pub(crate) const NO_PROGRAM: &str = "rt-no-such-gh-program";

/// How long one branch's read is reused.
const TTL: Duration = Duration::from_secs(60);

/// How long `gh` may take before it is killed: a logged-out CLI or a slow
/// network must not hold the drawer.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The fields one read asks `gh` for.
const FIELDS: &str = "number,state,statusCheckRollup,isCrossRepository";

/// How many pull requests one read asks for: a branch name is shared with
/// every fork's, so the repository's own may not be the first.
const LIMIT: &str = "10";

/// The Dashboard's cache of pull-request reads.
pub(crate) struct PrCache {
    /// The program the reads run; a test passes its own stand-in.
    program: String,
    entries: Mutex<HashMap<Key, Cached>>,
}

impl Default for PrCache {
    fn default() -> Self {
        Self::with_program(PROGRAM)
    }
}

impl PrCache {
    /// A cache whose reads run `program`.
    pub(crate) fn with_program(program: &str) -> Self {
        Self {
            program: program.to_owned(),
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The cached read of `key`, when it is younger than [`TTL`]: the inner
    /// `None` is a cached "no pull request".
    #[expect(
        clippy::option_option,
        reason = "the outer option is a cache hit, the inner a cached miss"
    )]
    fn get(&self, key: &Key) -> Option<Option<WorkPullRequest>> {
        let entries = lock(&self.entries);
        let cached = entries.get(key)?;
        (Instant::now().duration_since(cached.at) < TTL).then(|| cached.pr.clone())
    }

    /// Cache `pr` for `key`, dropping the entries that have expired.
    fn insert(&self, key: Key, pr: Option<WorkPullRequest>) {
        let now = Instant::now();
        let mut entries = lock(&self.entries);
        entries.retain(|_, cached| now.duration_since(cached.at) < TTL);
        entries.insert(key, Cached { pr, at: now });
    }

    /// How many reads it holds: a test sees that a skipped member left none.
    #[cfg(test)]
    fn entries_len(&self) -> usize {
        lock(&self.entries).len()
    }
}

/// What one read is cached under: the worktree it ran in and the branch.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    worktree: PathBuf,
    branch: String,
}

/// One cached read and when it was made.
struct Cached {
    pr: Option<WorkPullRequest>,
    at: Instant,
}

/// `members`' pull request: the first member with a branch that has one.
pub(crate) async fn pull_request(
    cache: &PrCache,
    members: &[SessionMember],
) -> Option<WorkPullRequest> {
    for member in members {
        if member.branch.is_empty() {
            continue;
        }
        if let Some(pr) = member_pull_request(cache, member).await {
            return Some(pr);
        }
    }
    None
}

/// One member's pull request, from the cache when it is fresh.
async fn member_pull_request(cache: &PrCache, member: &SessionMember) -> Option<WorkPullRequest> {
    let key = Key {
        worktree: PathBuf::from(&member.worktree_path),
        branch: member.branch.clone(),
    };
    if let Some(cached) = cache.get(&key) {
        return cached;
    }
    let pr = read_pull_request(&cache.program, &key.worktree, &key.branch).await;
    cache.insert(key, pr.clone());
    pr
}

/// `gh`'s most recent pull request of this repository for `branch` in
/// `worktree`: `None` when the program cannot be started, the run fails, it
/// does not finish within [`READ_TIMEOUT`], or its output does not parse.
/// Only an uncached read runs it, and none of these is a warning: a drawer
/// with no pull request is a normal answer.
async fn read_pull_request(
    program: &str,
    worktree: &Path,
    branch: &str,
) -> Option<WorkPullRequest> {
    let mut cmd = Command::new(program);
    cmd.current_dir(worktree)
        .args(["pr", "list", "--head"])
        .arg(branch)
        .args(["--state", "all", "--limit", LIMIT, "--json", FIELDS])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let output = match tokio::time::timeout(READ_TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => {
            debug!(
                program,
                branch,
                %err,
                "dashboard: the pull-request read did not start; the drawer shows no pull request"
            );
            return None;
        }
        Err(_) => {
            debug!(
                program,
                branch,
                seconds = READ_TIMEOUT.as_secs(),
                "dashboard: the pull-request read did not finish; the drawer shows no pull request"
            );
            return None;
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        debug!(
            program,
            branch,
            first_line = stderr.lines().next().unwrap_or_default(),
            "dashboard: the pull-request read failed; the drawer shows no pull request"
        );
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    match parse(&stdout) {
        Ok(Some(pr)) => Some(pr),
        Ok(None) => {
            debug!(
                program,
                branch, "dashboard: the pull-request read listed no pull request"
            );
            None
        }
        Err(err) => {
            debug!(
                program,
                branch,
                %err,
                "dashboard: the pull-request read's output did not parse; the drawer shows no pull request"
            );
            None
        }
    }
}

/// The first pull request `gh`'s JSON lists that this repository owns:
/// `Ok(None)` when it lists none, `Err` when its output does not parse. A
/// cross-repository one was opened from a fork, another repository's branch of
/// the same name, and is never this worktree's.
fn parse(json: &str) -> Result<Option<WorkPullRequest>, serde_json::Error> {
    let listed: Vec<Listed> = serde_json::from_str(json)?;
    let Some(first) = listed.into_iter().find(|pr| !pr.is_cross_repository) else {
        return Ok(None);
    };
    let checks = first.status_check_rollup.as_deref().unwrap_or_default();
    Ok(Some(WorkPullRequest {
        number: first.number,
        state: state(&first.state),
        checks: checks_of(checks),
    }))
}

/// One pull request as `gh` prints it.
#[derive(Deserialize)]
struct Listed {
    number: u32,
    state: String,
    /// Absent on a pull request it listed without the field.
    #[serde(default, rename = "statusCheckRollup")]
    status_check_rollup: Option<Vec<RollupEntry>>,
    /// Opened from a fork: another repository's branch of the same name.
    #[serde(default, rename = "isCrossRepository")]
    is_cross_repository: bool,
}

/// One entry of a rollup: a check run or a status context.
#[derive(Deserialize)]
struct RollupEntry {
    #[serde(default, rename = "__typename")]
    typename: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

impl RollupEntry {
    /// Whether it is a check run, whose result is its conclusion.
    fn is_check_run(&self) -> bool {
        self.typename.as_deref() == Some("CheckRun")
    }

    /// Whether it is a status context, whose result is its state.
    fn is_status_context(&self) -> bool {
        self.typename.as_deref() == Some("StatusContext")
    }

    /// Whether it ended failed.
    fn failed(&self) -> bool {
        if self.is_check_run() {
            matches!(
                self.conclusion.as_deref(),
                Some(
                    "FAILURE"
                        | "ERROR"
                        | "CANCELLED"
                        | "TIMED_OUT"
                        | "ACTION_REQUIRED"
                        | "STARTUP_FAILURE"
                )
            )
        } else if self.is_status_context() {
            matches!(self.state.as_deref(), Some("FAILURE" | "ERROR"))
        } else {
            false
        }
    }

    /// Whether it has not finished yet.
    fn unfinished(&self) -> bool {
        if self.is_check_run() {
            self.status.as_deref() != Some("COMPLETED")
        } else if self.is_status_context() {
            matches!(self.state.as_deref(), Some("PENDING" | "EXPECTED"))
        } else {
            false
        }
    }
}

/// A pull request's state from `gh`'s word.
fn state(state: &str) -> PullRequestState {
    match state {
        "OPEN" => PullRequestState::Open,
        "MERGED" => PullRequestState::Merged,
        "CLOSED" => PullRequestState::Closed,
        _ => PullRequestState::Unknown,
    }
}

/// The rollup's combined result: the first failed check wins, then the first
/// unfinished one, else every check passed. No check at all is
/// [`PullRequestChecks::None`].
fn checks_of(rollup: &[RollupEntry]) -> PullRequestChecks {
    if rollup.is_empty() {
        return PullRequestChecks::None;
    }
    if rollup.iter().any(RollupEntry::failed) {
        return PullRequestChecks::Failed;
    }
    if rollup.iter().any(RollupEntry::unfinished) {
        return PullRequestChecks::Pending;
    }
    PullRequestChecks::Passed
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use super::*;
    use crate::file_fetch::test_support::TestDir;

    /// A member at `path` on `branch`.
    fn member_at(path: &Path, branch: &str) -> SessionMember {
        SessionMember {
            repo_id: "r1".to_owned(),
            repo_name: "repo".to_owned(),
            branch: branch.to_owned(),
            worktree_path: path.to_string_lossy().into_owned(),
        }
    }

    /// One listed pull request as `gh` prints it, its rollup `rollup`.
    fn listed(state: &str, rollup: &str) -> String {
        format!(r#"[{{"number":12,"state":"{state}","statusCheckRollup":{rollup}}}]"#)
    }

    /// One listed pull request with no rollup field at all.
    fn listed_without_a_rollup(state: &str) -> String {
        format!(r#"[{{"number":12,"state":"{state}"}}]"#)
    }

    /// One pull request as `gh` prints it: `cross` marks one opened from a
    /// fork.
    fn entry(number: u32, state: &str, rollup: &str, cross: bool) -> String {
        format!(
            r#"{{"number":{number},"state":"{state}","statusCheckRollup":{rollup},"isCrossRepository":{cross}}}"#
        )
    }

    /// A list of `entries` as `gh` prints it.
    fn list(entries: &[String]) -> String {
        format!("[{}]", entries.join(","))
    }

    /// A rollup of `entries`.
    fn rollup(entries: &[String]) -> String {
        format!("[{}]", entries.join(","))
    }

    /// One check run, its conclusion `None` while it runs.
    fn check_run(status: &str, conclusion: Option<&str>) -> String {
        let conclusion = conclusion.map_or_else(
            || "null".to_owned(),
            |conclusion| format!(r#""{conclusion}""#),
        );
        format!(r#"{{"__typename":"CheckRun","status":"{status}","conclusion":{conclusion}}}"#)
    }

    /// One status context.
    fn status_context(state: &str) -> String {
        format!(r#"{{"__typename":"StatusContext","state":"{state}"}}"#)
    }

    /// The pull request `json` lists, failing the test when it lists none.
    fn pr_of(json: &str) -> WorkPullRequest {
        parse(json).expect("parses").expect("a listed pull request")
    }

    #[test]
    fn an_open_pr_with_passing_checks_parses() {
        let json = listed("OPEN", &rollup(&[check_run("COMPLETED", Some("SUCCESS"))]));
        let pr = pr_of(&json);
        assert_eq!(pr.number, 12);
        assert_eq!(pr.state, PullRequestState::Open);
        assert_eq!(pr.checks, PullRequestChecks::Passed);
    }

    #[test]
    fn a_merged_pr_parses() {
        let pr = pr_of(&listed("MERGED", &rollup(&[])));
        assert_eq!(pr.state, PullRequestState::Merged);
        assert_eq!(pr.checks, PullRequestChecks::None);
    }

    #[test]
    fn a_closed_pr_parses() {
        assert_eq!(
            pr_of(&listed("CLOSED", &rollup(&[]))).state,
            PullRequestState::Closed
        );
    }

    #[test]
    fn any_failed_check_run_reads_failed() {
        for conclusion in [
            "FAILURE",
            "ERROR",
            "CANCELLED",
            "TIMED_OUT",
            "ACTION_REQUIRED",
            "STARTUP_FAILURE",
        ] {
            let json = listed(
                "OPEN",
                &rollup(&[
                    check_run("COMPLETED", Some("SUCCESS")),
                    check_run("COMPLETED", Some(conclusion)),
                ]),
            );
            assert_eq!(
                pr_of(&json).checks,
                PullRequestChecks::Failed,
                "{conclusion}"
            );
        }
    }

    #[test]
    fn a_failing_status_context_reads_failed() {
        for state in ["FAILURE", "ERROR"] {
            let json = listed("OPEN", &rollup(&[status_context(state)]));
            assert_eq!(pr_of(&json).checks, PullRequestChecks::Failed, "{state}");
        }
    }

    #[test]
    fn an_unfinished_check_reads_pending() {
        for status in ["QUEUED", "IN_PROGRESS", "WAITING", "REQUESTED", "PENDING"] {
            let json = listed("OPEN", &rollup(&[check_run(status, None)]));
            assert_eq!(pr_of(&json).checks, PullRequestChecks::Pending, "{status}");
        }
        let mixed = listed(
            "OPEN",
            &rollup(&[
                check_run("COMPLETED", Some("SUCCESS")),
                check_run("IN_PROGRESS", None),
            ]),
        );
        assert_eq!(pr_of(&mixed).checks, PullRequestChecks::Pending);
    }

    #[test]
    fn a_pending_status_context_reads_pending() {
        for state in ["PENDING", "EXPECTED"] {
            let json = listed("OPEN", &rollup(&[status_context(state)]));
            assert_eq!(pr_of(&json).checks, PullRequestChecks::Pending, "{state}");
        }
    }

    #[test]
    fn neutral_and_skipped_conclusions_read_passed() {
        let json = listed(
            "OPEN",
            &rollup(&[
                check_run("COMPLETED", Some("NEUTRAL")),
                check_run("COMPLETED", Some("SKIPPED")),
                status_context("SUCCESS"),
            ]),
        );
        assert_eq!(pr_of(&json).checks, PullRequestChecks::Passed);
    }

    #[test]
    fn no_checks_reads_none() {
        assert_eq!(
            pr_of(&listed("OPEN", &rollup(&[]))).checks,
            PullRequestChecks::None
        );
        assert_eq!(
            pr_of(&listed_without_a_rollup("OPEN")).checks,
            PullRequestChecks::None,
            "a missing rollup is no checks"
        );
    }

    #[test]
    fn an_empty_list_is_no_pr() {
        assert!(matches!(parse("[]"), Ok(None)));
    }

    #[test]
    fn a_cross_repository_pr_on_the_same_branch_name_is_skipped() {
        let json = list(&[
            entry(3, "OPEN", &rollup(&[]), true),
            entry(12, "MERGED", &rollup(&[]), false),
        ]);
        let pr = pr_of(&json);
        assert_eq!(
            pr.number, 12,
            "the fork's pull request on this branch name is not this worktree's"
        );
        assert_eq!(pr.state, PullRequestState::Merged);
    }

    #[test]
    fn only_cross_repository_prs_is_no_pr() {
        let json = list(&[
            entry(3, "OPEN", &rollup(&[]), true),
            entry(4, "CLOSED", &rollup(&[]), true),
        ]);
        assert!(matches!(parse(&json), Ok(None)));
    }

    #[test]
    fn an_unknown_state_reads_unknown() {
        assert_eq!(
            pr_of(&listed("DRAFT", &rollup(&[]))).state,
            PullRequestState::Unknown
        );
    }

    #[test]
    fn unparsable_json_is_no_pr() {
        assert!(parse("not json").is_err(), "unparsable output is an error");
        assert!(
            parse(r#"[{"number":"twelve","state":"OPEN"}]"#).is_err(),
            "a number that is not one is unparsable"
        );
    }

    #[tokio::test]
    async fn a_missing_gh_gives_no_pr() {
        let dir = TestDir::new("pr-missing-gh");
        let cache = PrCache::with_program(NO_PROGRAM);

        assert_eq!(
            pull_request(&cache, &[member_at(dir.path(), "feat/rt-496")]).await,
            None
        );
        assert_eq!(cache.entries_len(), 1, "the miss is cached too");
    }

    #[tokio::test]
    async fn a_member_without_a_branch_is_skipped() {
        let dir = TestDir::new("pr-no-branch");
        let cache = PrCache::with_program(NO_PROGRAM);

        assert_eq!(
            pull_request(&cache, &[member_at(dir.path(), "")]).await,
            None
        );
        assert_eq!(
            cache.entries_len(),
            0,
            "a member with no branch is never read"
        );
    }

    #[tokio::test]
    async fn the_result_is_cached_for_sixty_seconds() {
        let dir = TestDir::new("pr-cache");
        let cache = PrCache::with_program(NO_PROGRAM);
        let member = member_at(dir.path(), "feat/rt-496");
        let known = WorkPullRequest {
            number: 7,
            state: PullRequestState::Open,
            checks: PullRequestChecks::Pending,
        };
        cache.insert(
            Key {
                worktree: PathBuf::from(&member.worktree_path),
                branch: member.branch.clone(),
            },
            Some(known.clone()),
        );

        assert_eq!(
            pull_request(&cache, std::slice::from_ref(&member)).await,
            Some(known),
            "a read inside sixty seconds is reused"
        );

        expire(&cache);
        assert_eq!(
            pull_request(&cache, std::slice::from_ref(&member)).await,
            None,
            "a read past sixty seconds runs again"
        );
    }

    /// Make every cached entry [`TTL`] old, as a minute of real time would.
    fn expire(cache: &PrCache) {
        let now = Instant::now();
        let mut entries = lock(&cache.entries);
        for cached in entries.values_mut() {
            cached.at = now - TTL;
        }
    }
}
