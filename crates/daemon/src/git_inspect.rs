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
) -> anyhow::Result<Vec<GitCommit>> {
    let limit_arg = format!("-n{limit}");
    let format_arg = format!("--format={COMMIT_FORMAT}");
    let mut args = vec!["log", limit_arg.as_str(), format_arg.as_str()];
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

pub async fn repo_status(repo: &Path) -> anyhow::Result<Vec<GitFileChange>> {
    let stdout = run_git(repo, &["status", "--porcelain=1", "-z"]).await?;
    let mut out = Vec::new();
    let mut iter = stdout.split('\0').peekable();
    while let Some(entry) = iter.next() {
        if entry.is_empty() {
            continue;
        }
        let bytes = entry.as_bytes();
        if bytes.len() < 3 {
            continue;
        }
        let xy = &entry[..2];
        let path = entry[3..].to_string();
        let status = first_meaningful_status(xy).to_string();
        let from_path = if status == "R" {
            iter.next().map(String::from)
        } else {
            None
        };
        out.push(GitFileChange {
            path,
            status,
            from_path,
        });
    }
    Ok(out)
}

fn first_meaningful_status(xy: &str) -> char {
    let chars: Vec<char> = xy.chars().collect();
    match (chars.first().copied(), chars.get(1).copied()) {
        (Some('?'), _) => '?',
        (Some(c), _) | (_, Some(c)) if c != ' ' => c,
        _ => '?',
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
