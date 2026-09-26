//! Read-only git inspection helpers backing the Phase 6 git panel.

use crate::file_fetch;
use anyhow::{Context as _, anyhow};
use protocol::{GitCommit, GitCommitDetail, GitFileChange, GitRemoteUrl, SnapshotUnavailable};
use std::io;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

async fn run_git(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
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
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("git {args:?} failed: {stderr}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

const COMMIT_FORMAT: &str = "%H%x1f%h%x1f%an%x1f%ae%x1f%aI%x1f%s";

pub async fn list_commits(
    repo: &Path,
    branch: Option<&str>,
    limit: u32,
    offset: u32,
) -> anyhow::Result<Vec<GitCommit>> {
    let limit_arg = format!("-n{limit}");
    let skip_arg = format!("--skip={offset}");
    let format_arg = format!("--format={COMMIT_FORMAT}");
    let mut args = vec![
        "log",
        limit_arg.as_str(),
        skip_arg.as_str(),
        format_arg.as_str(),
    ];
    if let Some(b) = branch {
        args.push(b);
    }
    let stdout = run_git(repo, &args).await?;
    Ok(stdout.lines().filter_map(parse_commit_line).collect())
}

fn parse_commit_line(line: &str) -> Option<GitCommit> {
    let mut parts = line.splitn(6, '\u{1f}');
    let sha = parts.next()?.to_string();
    let short_sha = parts.next()?.to_string();
    let author_name = parts.next()?.to_string();
    let author_email = parts.next()?.to_string();
    let authored_at = parts.next()?.to_string();
    let subject = parts.next()?.to_string();
    Some(GitCommit {
        sha,
        short_sha,
        author_name,
        author_email,
        authored_at,
        subject,
    })
}

pub async fn get_commit(repo: &Path, sha: &str) -> anyhow::Result<GitCommitDetail> {
    let format_arg = format!("--format={COMMIT_FORMAT}%n%P%n%b%n--END--");
    let stdout = run_git(
        repo,
        &[
            "show",
            "--name-status",
            "--no-color",
            format_arg.as_str(),
            sha,
        ],
    )
    .await?;

    let mut lines = stdout.lines();
    let header = lines.next().ok_or_else(|| anyhow!("empty git show"))?;
    let commit = parse_commit_line(header)
        .ok_or_else(|| anyhow!("could not parse commit header: {header}"))?;
    let parents_line = lines.next().unwrap_or("");
    let parent_shas: Vec<String> = parents_line.split_whitespace().map(String::from).collect();

    let mut body = String::new();
    let mut changes = Vec::new();
    let mut in_body = true;
    for line in lines {
        if in_body {
            if line == "--END--" {
                in_body = false;
                continue;
            }
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(line);
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if let Some(ch) = parse_name_status_line(line) {
            changes.push(ch);
        }
    }

    Ok(GitCommitDetail {
        commit,
        body: body.trim().to_string(),
        parent_shas,
        changes,
    })
}

fn parse_name_status_line(line: &str) -> Option<GitFileChange> {
    let mut cols = line.split('\t');
    let status_raw = cols.next()?;
    let path = cols.next()?.to_string();
    let from = cols.next();
    let status = status_raw.chars().next()?.to_string();
    Some(GitFileChange {
        path: from.map_or_else(|| path.clone(), |_| path.clone()),
        status,
        from_path: from.map(String::from).filter(|f| f != &path),
    })
}

pub async fn file_diff(repo: &Path, path: &str, against: Option<&str>) -> anyhow::Result<String> {
    let mut args = vec!["diff", "--no-color"];
    if let Some(rev) = against {
        args.push(rev);
    }
    args.push("--");
    args.push(path);
    run_git(repo, &args).await
}

/// Split working-tree status into two buckets keyed off the porcelain X/Y
/// columns. Returns `(index_changes, worktree_changes)` so the UI can render
/// STAGED + CHANGES sections independently. A file with both staged and
/// unstaged edits appears in both lists with different `status` chars (X
/// for the index entry, Y for the worktree entry); untracked files (`??`)
/// land in `worktree_changes` only with `status = "?"`.
pub async fn repo_status(repo: &Path) -> anyhow::Result<(Vec<GitFileChange>, Vec<GitFileChange>)> {
    // Plain folders (no `.git`) registered as repos have no status to
    // report. Treat them as clean so the source-control sidebar can render
    // an empty list instead of bubbling up a "not a git repository" error.
    if !repo.join(".git").exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let stdout = run_git(repo, &["status", "--porcelain=1", "-z"]).await?;
    Ok(parse_porcelain_z(&stdout))
}

fn parse_porcelain_z(raw: &str) -> (Vec<GitFileChange>, Vec<GitFileChange>) {
    let mut index = Vec::new();
    let mut worktree = Vec::new();
    let mut iter = raw.split('\0').peekable();
    while let Some(entry) = iter.next() {
        if entry.is_empty() {
            continue;
        }
        if entry.len() < 3 {
            continue;
        }
        let mut chars = entry.chars();
        let x = chars.next().unwrap_or(' ');
        let y = chars.next().unwrap_or(' ');
        let path = entry[3..].to_string();
        let from_path = if x == 'R' || y == 'R' {
            iter.next().map(String::from)
        } else {
            None
        };
        if x == '?' && y == '?' {
            worktree.push(GitFileChange {
                path,
                status: "?".to_string(),
                from_path: None,
            });
            continue;
        }
        if x != ' ' && x != '?' {
            index.push(GitFileChange {
                path: path.clone(),
                status: x.to_string(),
                from_path: from_path.clone(),
            });
        }
        if y != ' ' && y != '?' {
            worktree.push(GitFileChange {
                path,
                status: y.to_string(),
                from_path,
            });
        }
    }
    (index, worktree)
}

/// Cap on one side of a snapshot. A side larger than this is reported as
/// [`SnapshotUnavailable::TooLarge`] instead of being decoded and shipped:
/// the diff of a file that size costs more than the client can render.
pub const SNAPSHOT_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Length of the prefix the binary probe reads. git inspects the same 8000
/// bytes when it decides whether a file it is adding is text.
const BINARY_PROBE_BYTES: usize = 8000;

/// The two texts behind a Monaco diff view, or the reason one of them could
/// not be produced. `unavailable` is `Some` only with both texts empty.
pub struct FileSnapshotContent {
    pub old: String,
    pub new: String,
    pub unavailable: Option<SnapshotUnavailable>,
}

/// One side of a comparison, before the two are paired.
enum Side {
    /// Decoded text; empty when the side does not exist (an untracked new
    /// file, or a revision that never carried the old one).
    Text(String),
    /// The side could not be shipped, and why.
    Unavailable(SnapshotUnavailable),
}

/// Snapshot the old + new content backing a Monaco diff view.
///
/// `against = None` → unstaged: old = index entry (`git show :0:<path>`),
///                    new = worktree file. Untracked files show as
///                    additions (empty old).
/// `against = Some("HEAD")` → staged: old = HEAD content
///                    (`git show HEAD:<path>`), new = index entry.
/// `against = Some(sha)` → historical: old = `git show <sha>~:<path>`,
///                    new = `git show <sha>:<path>`. If the file does not
///                    exist on either side (added/removed in the commit),
///                    that side becomes empty.
///
/// A side that is binary or larger than [`SNAPSHOT_MAX_BYTES`] is reported
/// through [`FileSnapshotContent::unavailable`] instead, with both texts
/// empty — a half-rendered diff of bytes the client cannot decode is worse
/// than none.
pub async fn file_snapshot(
    repo: &Path,
    path: &str,
    against: Option<&str>,
) -> anyhow::Result<FileSnapshotContent> {
    let (old, new) = match against {
        None => {
            let new = match file_fetch::confine_path(repo, path) {
                Ok(target) => read_disk_side(&target).await?,
                Err(err) if file_fetch::is_not_found(&err) => Side::Text(String::new()),
                Err(err) => return Err(err),
            };
            let old = read_git_side(repo, &format!(":0:{path}")).await?;
            (old, new)
        }
        Some("HEAD") => (
            read_git_side(repo, &format!("HEAD:{path}")).await?,
            read_git_side(repo, &format!(":0:{path}")).await?,
        ),
        Some(rev) => (
            read_git_side(repo, &format!("{rev}~:{path}")).await?,
            read_git_side(repo, &format!("{rev}:{path}")).await?,
        ),
    };
    Ok(merge_sides(old, new))
}

/// Pair the two sides. A side that could not be shipped empties both texts
/// and names the reason.
fn merge_sides(old: Side, new: Side) -> FileSnapshotContent {
    let unavailable = match (old, new) {
        (Side::Text(old), Side::Text(new)) => {
            return FileSnapshotContent {
                old,
                new,
                unavailable: None,
            };
        }
        (Side::Unavailable(old), Side::Unavailable(new)) => Some(more_severe(old, new)),
        (Side::Unavailable(reason), Side::Text(_)) | (Side::Text(_), Side::Unavailable(reason)) => {
            Some(reason)
        }
    };
    FileSnapshotContent {
        old: String::new(),
        new: String::new(),
        unavailable,
    }
}

/// The more informative of two reasons for one snapshot: an oversized side
/// wins over a binary one, and of two oversized sides the larger byte count
/// is the one reported.
fn more_severe(a: SnapshotUnavailable, b: SnapshotUnavailable) -> SnapshotUnavailable {
    match (oversized_bytes(&a), oversized_bytes(&b)) {
        (Some(a_bytes), Some(b_bytes)) if b_bytes > a_bytes => b,
        (None, Some(_)) => b,
        _ => a,
    }
}

/// The side's length, when the reason is that the side is oversized.
fn oversized_bytes(reason: &SnapshotUnavailable) -> Option<u64> {
    match reason {
        SnapshotUnavailable::TooLarge { bytes, .. } => Some(*bytes),
        SnapshotUnavailable::Binary | SnapshotUnavailable::Unknown => None,
    }
}

/// Read the worktree side of a diff. Its length is read from the directory
/// entry first, so an oversized file is never loaded; a path that has gone
/// missing, or that is not a regular file (a directory entry an untracked
/// folder showed up as), reads as an empty side.
async fn read_disk_side(target: &Path) -> anyhow::Result<Side> {
    let meta = match tokio::fs::metadata(target).await {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Side::Text(String::new())),
        Err(err) => return Err(err).with_context(|| format!("statting {}", target.display())),
    };
    if !meta.is_file() {
        return Ok(Side::Text(String::new()));
    }
    if meta.len() > SNAPSHOT_MAX_BYTES {
        return Ok(Side::Unavailable(too_large(meta.len())));
    }
    let bytes = tokio::fs::read(target)
        .await
        .with_context(|| format!("reading {}", target.display()))?;
    Ok(classify_side(&bytes))
}

/// Read one side out of a git object. Any git failure — a missing object, a
/// revision git cannot resolve — reads as an empty side.
async fn read_git_side(repo: &Path, spec: &str) -> anyhow::Result<Side> {
    if let Some(bytes) = git_blob_size(repo, spec).await?
        && bytes > SNAPSHOT_MAX_BYTES
    {
        return Ok(Side::Unavailable(too_large(bytes)));
    }
    let bytes = run_git_quiet(repo, &["show", spec]).await?;
    Ok(classify_side(&bytes))
}

/// The blob's length as `git cat-file -s` reports it, or `None` when git
/// cannot resolve the object. Asking for the size first is what keeps an
/// oversized blob out of memory: it is never fetched.
async fn git_blob_size(repo: &Path, spec: &str) -> anyhow::Result<Option<u64>> {
    let out = run_git_quiet(repo, &["cat-file", "-s", spec]).await?;
    Ok(std::str::from_utf8(&out)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok()))
}

/// Classify one side's raw bytes: over the cap, binary, or text. Size is
/// checked first, so an oversized binary file reports that rather than its
/// content type.
fn classify_side(bytes: &[u8]) -> Side {
    if bytes.len() as u64 > SNAPSHOT_MAX_BYTES {
        return Side::Unavailable(too_large(bytes.len() as u64));
    }
    if looks_binary(bytes) {
        return Side::Unavailable(SnapshotUnavailable::Binary);
    }
    Side::Text(String::from_utf8_lossy(bytes).into_owned())
}

/// git's NUL probe plus a UTF-8 check: a NUL byte in the first 8000 bytes,
/// or bytes that are not valid UTF-8, mean the side is not text.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_PROBE_BYTES).any(|&b| b == 0) || std::str::from_utf8(bytes).is_err()
}

fn too_large(bytes: u64) -> SnapshotUnavailable {
    SnapshotUnavailable::TooLarge {
        bytes,
        limit: SNAPSHOT_MAX_BYTES,
    }
}

/// Like [`run_git`] but does not error on non-zero exit — callers that
/// want "empty output when the object is missing" semantics (e.g. a file
/// added since HEAD has no HEAD side) use this. The bytes come back
/// undecoded so the caller can tell binary content from text.
async fn run_git_quiet(repo: &Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
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
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(output.stdout)
}

/// Map a file path's extension to a Monaco language id. Defaults to
/// `"plaintext"` for unknown extensions; Monaco renders unstyled.
#[must_use]
pub fn language_for_path(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map_or("", |(_, e)| e);
    match ext {
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "rs" => "rust",
        "py" => "python",
        "json" => "json",
        "md" | "markdown" => "markdown",
        "css" => "css",
        "scss" | "sass" => "scss",
        "html" | "htm" => "html",
        "xml" => "xml",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "sh" | "bash" | "zsh" => "shell",
        "ps1" | "psm1" | "psd1" => "powershell",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "sql" => "sql",
        "dockerfile" => "dockerfile",
        _ => "plaintext",
    }
}

pub async fn remote_url(repo_id: &str, repo: &Path) -> anyhow::Result<GitRemoteUrl> {
    let raw = run_git(repo, &["remote", "get-url", "origin"]).await?;
    let raw = raw.trim().to_string();
    let (web_url, forge) = parse_forge(&raw);
    Ok(GitRemoteUrl {
        repo_id: repo_id.to_string(),
        raw_url: raw,
        web_url,
        forge,
    })
}

fn parse_forge(raw: &str) -> (Option<String>, String) {
    // git@github.com:owner/repo.git → https://github.com/owner/repo
    // ssh://git@github.com/owner/repo.git → https://github.com/owner/repo
    // https://github.com/owner/repo.git → https://github.com/owner/repo
    let candidates = [
        ("github.com", "github"),
        ("gitlab.com", "gitlab"),
        ("bitbucket.org", "bitbucket"),
    ];
    let normalized = raw.trim_end_matches(".git").trim_end_matches('/');
    for (host, forge) in candidates {
        let needle_ssh = format!("git@{host}:");
        let needle_ssh2 = format!("ssh://git@{host}/");
        let needle_https = format!("https://{host}/");
        let owner_repo = if let Some(rest) = normalized.strip_prefix(&needle_ssh) {
            Some(rest.to_string())
        } else if let Some(rest) = normalized.strip_prefix(&needle_ssh2) {
            Some(rest.to_string())
        } else {
            normalized.strip_prefix(&needle_https).map(String::from)
        };
        if let Some(owner_repo) = owner_repo {
            return (
                Some(format!("https://{host}/{owner_repo}")),
                forge.to_string(),
            );
        }
    }
    (None, "unknown".to_string())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; panic messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::file_fetch::test_support::TestDir;

    #[test]
    fn parse_splits_staged_and_worktree_buckets() {
        // M=modified-and-staged, MM=staged AND further unstaged edits,
        // ??=untracked, A=newly staged add, D=worktree delete only.
        let raw = "M  staged.txt\0MM both.txt\0?? untracked.txt\0A  added.txt\0 D removed.txt\0";
        let (index, worktree) = parse_porcelain_z(raw);

        let idx_paths: Vec<_> = index.iter().map(|c| c.path.as_str()).collect();
        let wt_paths: Vec<_> = worktree.iter().map(|c| c.path.as_str()).collect();

        assert!(idx_paths.contains(&"staged.txt"));
        assert!(idx_paths.contains(&"both.txt"));
        assert!(idx_paths.contains(&"added.txt"));
        assert!(!idx_paths.contains(&"untracked.txt"));
        assert!(!idx_paths.contains(&"removed.txt"));

        assert!(wt_paths.contains(&"both.txt"));
        assert!(wt_paths.contains(&"untracked.txt"));
        assert!(wt_paths.contains(&"removed.txt"));
        assert!(!wt_paths.contains(&"staged.txt"));
        assert!(!wt_paths.contains(&"added.txt"));

        let untracked = worktree
            .iter()
            .find(|c| c.path == "untracked.txt")
            .expect("untracked entry present");
        assert_eq!(untracked.status, "?");
    }

    #[test]
    fn parse_handles_renames_with_old_path() {
        // `R  newname\0oldname` — old name is the follow-up entry.
        let raw = "R  newname.txt\0oldname.txt\0";
        let (index, _worktree) = parse_porcelain_z(raw);
        assert_eq!(index.len(), 1);
        let rename = &index[0];
        assert_eq!(rename.path, "newname.txt");
        assert_eq!(rename.status, "R");
        assert_eq!(rename.from_path.as_deref(), Some("oldname.txt"));
    }

    #[test]
    fn parse_empty_input_returns_empty_buckets() {
        let (index, worktree) = parse_porcelain_z("");
        assert!(index.is_empty());
        assert!(worktree.is_empty());
    }

    /// A throwaway git repo with an identity configured, so `git commit`
    /// works whatever the machine's global git config says.
    async fn init_repo(tag: &str) -> TestDir {
        let dir = TestDir::new(tag);
        let root = dir.path();
        run_git(root, &["init"]).await.expect("git init");
        run_git(root, &["config", "user.email", "t@example.com"])
            .await
            .expect("config email");
        run_git(root, &["config", "user.name", "Test"])
            .await
            .expect("config name");
        // A global commit.gpgsign=true without a usable key would block commits.
        run_git(root, &["config", "commit.gpgsign", "false"])
            .await
            .expect("config gpgsign");
        dir
    }

    /// Write `body` and commit it, so a later write is an uncommitted edit.
    async fn commit_file(repo: &Path, name: &str, body: &[u8]) {
        std::fs::write(repo.join(name), body).expect("write file");
        run_git(repo, &["add", "--", name]).await.expect("git add");
        let message = format!("add {name}");
        run_git(repo, &["commit", "-m", &message])
            .await
            .expect("git commit");
    }

    /// The full SHA of the current commit.
    async fn head_sha(repo: &Path) -> String {
        run_git(repo, &["rev-parse", "HEAD"])
            .await
            .expect("rev-parse HEAD")
            .trim()
            .to_string()
    }

    /// A PNG header plus IHDR bytes: NUL bytes, so git calls it binary.
    const PNG_HEADER: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";

    #[tokio::test]
    async fn file_snapshot_of_text_file_carries_both_sides() {
        let dir = init_repo("text").await;
        let repo = dir.path();
        commit_file(repo, "note.txt", b"one\n").await;
        std::fs::write(repo.join("note.txt"), "one\ntwo\n").expect("edit file");

        let unstaged = file_snapshot(repo, "note.txt", None)
            .await
            .expect("unstaged snapshot");
        assert_eq!(unstaged.unavailable, None);
        assert_eq!(unstaged.old, "one\n");
        assert_eq!(unstaged.new, "one\ntwo\n");

        let staged = file_snapshot(repo, "note.txt", Some("HEAD"))
            .await
            .expect("staged snapshot");
        assert_eq!(staged.unavailable, None);
        assert_eq!(staged.old, "one\n");
        assert_eq!(staged.new, "one\n");
    }

    #[tokio::test]
    async fn file_snapshot_of_binary_file_is_unavailable() {
        let dir = init_repo("binary").await;
        let repo = dir.path();
        commit_file(repo, "logo.png", b"placeholder\n").await;

        // Worktree vs index: the disk copy is binary.
        std::fs::write(repo.join("logo.png"), PNG_HEADER).expect("write png");
        let worktree = file_snapshot(repo, "logo.png", None)
            .await
            .expect("worktree snapshot");
        assert_eq!(worktree.unavailable, Some(SnapshotUnavailable::Binary));
        assert!(worktree.old.is_empty() && worktree.new.is_empty());

        // HEAD vs index: the staged copy is binary.
        run_git(repo, &["add", "logo.png"]).await.expect("git add");
        let staged = file_snapshot(repo, "logo.png", Some("HEAD"))
            .await
            .expect("staged snapshot");
        assert_eq!(staged.unavailable, Some(SnapshotUnavailable::Binary));
        assert!(staged.old.is_empty() && staged.new.is_empty());
    }

    #[tokio::test]
    async fn file_snapshot_of_invalid_utf8_disk_file_is_binary() {
        let dir = init_repo("latin1").await;
        let repo = dir.path();
        // Latin-1 "é" is a lone 0xE9: no NUL byte, but not valid UTF-8.
        std::fs::write(repo.join("latin.txt"), b"caf\xe9\n").expect("write latin-1 file");

        let snapshot = file_snapshot(repo, "latin.txt", None)
            .await
            .expect("snapshot");
        assert_eq!(snapshot.unavailable, Some(SnapshotUnavailable::Binary));
        assert!(snapshot.old.is_empty() && snapshot.new.is_empty());
    }

    #[tokio::test]
    async fn file_snapshot_of_oversized_file_is_too_large() {
        let dir = init_repo("oversize").await;
        let repo = dir.path();
        // NUL bytes as well as oversize, so the size check has to win over
        // the binary probe.
        let body = vec![0u8; 3 * 1024 * 1024];
        std::fs::write(repo.join("big.dat"), &body).expect("write big file");

        let worktree = file_snapshot(repo, "big.dat", None)
            .await
            .expect("worktree snapshot");
        assert_eq!(
            worktree.unavailable,
            Some(SnapshotUnavailable::TooLarge {
                bytes: body.len() as u64,
                limit: SNAPSHOT_MAX_BYTES,
            })
        );
        assert!(worktree.old.is_empty() && worktree.new.is_empty());

        // The git side is measured too, once the blob is committed.
        commit_file(repo, "big.dat", &body).await;
        let committed = file_snapshot(repo, "big.dat", Some("HEAD"))
            .await
            .expect("staged snapshot");
        assert_eq!(
            committed.unavailable,
            Some(SnapshotUnavailable::TooLarge {
                bytes: body.len() as u64,
                limit: SNAPSHOT_MAX_BYTES,
            })
        );
        assert!(committed.old.is_empty() && committed.new.is_empty());
    }

    #[tokio::test]
    async fn file_snapshot_against_a_sha_compares_the_commit_to_its_parent() {
        let dir = init_repo("sha").await;
        let repo = dir.path();
        commit_file(repo, "note.txt", b"one\n").await;
        let root = head_sha(repo).await;
        commit_file(repo, "note.txt", b"two\n").await;
        let tip = head_sha(repo).await;

        let change = file_snapshot(repo, "note.txt", Some(tip.as_str()))
            .await
            .expect("change snapshot");
        assert_eq!(change.unavailable, None);
        assert_eq!(change.old, "one\n");
        assert_eq!(change.new, "two\n");

        // A root commit has no parent, so its old side is empty.
        let root_snapshot = file_snapshot(repo, "note.txt", Some(root.as_str()))
            .await
            .expect("root snapshot");
        assert_eq!(root_snapshot.unavailable, None);
        assert!(root_snapshot.old.is_empty());
        assert_eq!(root_snapshot.new, "one\n");
    }

    #[tokio::test]
    async fn file_snapshot_of_directory_reads_empty() {
        let dir = init_repo("directory").await;
        let repo = dir.path();
        std::fs::create_dir(repo.join("sub")).expect("create subdir");
        std::fs::write(repo.join("sub/inner.txt"), "text\n").expect("write nested file");

        let snapshot = file_snapshot(repo, "sub", None)
            .await
            .expect("a directory snapshots as empty");
        assert_eq!(snapshot.unavailable, None);
        assert!(snapshot.old.is_empty() && snapshot.new.is_empty());
    }

    #[tokio::test]
    async fn file_snapshot_reports_an_oversized_head_side() {
        let dir = init_repo("oversize-head").await;
        let repo = dir.path();
        let body = vec![b'a'; 3 * 1024 * 1024];
        commit_file(repo, "big.dat", &body).await;
        // Stage a small edit, so only the HEAD side is oversized.
        std::fs::write(repo.join("big.dat"), "small\n").expect("shrink file");
        run_git(repo, &["add", "big.dat"]).await.expect("git add");

        let staged = file_snapshot(repo, "big.dat", Some("HEAD"))
            .await
            .expect("staged snapshot");
        assert_eq!(
            staged.unavailable,
            Some(SnapshotUnavailable::TooLarge {
                bytes: body.len() as u64,
                limit: SNAPSHOT_MAX_BYTES,
            })
        );
        assert!(staged.old.is_empty() && staged.new.is_empty());
    }

    #[tokio::test]
    async fn file_snapshot_prefers_too_large_over_binary() {
        let dir = init_repo("oversize-binary").await;
        let repo = dir.path();
        let body = vec![b'a'; 3 * 1024 * 1024];
        commit_file(repo, "mixed.dat", &body).await;
        // The HEAD side is oversized, the staged side binary.
        std::fs::write(repo.join("mixed.dat"), PNG_HEADER).expect("write png");
        run_git(repo, &["add", "mixed.dat"]).await.expect("git add");

        let staged = file_snapshot(repo, "mixed.dat", Some("HEAD"))
            .await
            .expect("staged snapshot");
        assert_eq!(
            staged.unavailable,
            Some(SnapshotUnavailable::TooLarge {
                bytes: body.len() as u64,
                limit: SNAPSHOT_MAX_BYTES,
            })
        );
        assert!(staged.old.is_empty() && staged.new.is_empty());
    }

    #[tokio::test]
    async fn file_snapshot_refuses_escape_but_tolerates_missing_file() {
        let dir = TestDir::new("snapshot");
        assert!(file_snapshot(dir.path(), "../x", None).await.is_err());
        let missing = file_snapshot(dir.path(), "untracked.txt", None)
            .await
            .expect("missing file snapshots as empty");
        assert_eq!(missing.unavailable, None);
        assert!(missing.old.is_empty() && missing.new.is_empty());
    }
}
