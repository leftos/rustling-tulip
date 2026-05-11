//! Read-only git inspection helpers backing the Phase 6 git panel.

use anyhow::{Context as _, anyhow};
use protocol::{GitCommit, GitCommitDetail, GitFileChange, GitRemoteUrl};
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
pub async fn file_snapshot(
    repo: &Path,
    path: &str,
    against: Option<&str>,
) -> anyhow::Result<(String, String)> {
    match against {
        None => {
            let old = run_git_quiet(repo, &["show", &format!(":0:{path}")]).await;
            let old = old.unwrap_or_default();
            let new = tokio::fs::read_to_string(repo.join(path))
                .await
                .unwrap_or_default();
            Ok((old, new))
        }
        Some("HEAD") => {
            let old = run_git_quiet(repo, &["show", &format!("HEAD:{path}")])
                .await
                .unwrap_or_default();
            let new = run_git_quiet(repo, &["show", &format!(":0:{path}")])
                .await
                .unwrap_or_default();
            Ok((old, new))
        }
        Some(rev) => {
            let old = run_git_quiet(repo, &["show", &format!("{rev}~:{path}")])
                .await
                .unwrap_or_default();
            let new = run_git_quiet(repo, &["show", &format!("{rev}:{path}")])
                .await
                .unwrap_or_default();
            Ok((old, new))
        }
    }
}

/// Like [`run_git`] but does not error on non-zero exit — callers that
/// want "empty string when the object is missing" semantics (e.g. file
/// added since HEAD has no HEAD side) use this.
async fn run_git_quiet(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
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
        return Ok(String::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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
}
