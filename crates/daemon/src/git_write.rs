//! Write-side git operations exposed by the source-control sidebar:
//! stage / unstage / commit / discard / stash. Each function shells out to
//! `git` with the repo's path as `-C`. Errors propagate via
//! `anyhow::Result` so the server can surface them as
//! [`protocol::DaemonMessage::GitWriteError`].

use anyhow::{Context as _, anyhow};
use protocol::GitStash;
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

/// `git add -- <path>...`. Caller is responsible for filtering empty inputs.
pub async fn stage(repo: &Path, paths: &[String]) -> anyhow::Result<()> {
    if paths.is_empty() {
        return Err(anyhow!("stage: no paths"));
    }
    let mut args: Vec<&str> = vec!["add", "--"];
    args.extend(paths.iter().map(String::as_str));
    run_git(repo, &args).await?;
    Ok(())
}

/// `git restore --staged -- <path>...`. Moves the named entries from the
/// index back to HEAD's content; worktree files are untouched.
pub async fn unstage(repo: &Path, paths: &[String]) -> anyhow::Result<()> {
    if paths.is_empty() {
        return Err(anyhow!("unstage: no paths"));
    }
    let mut args: Vec<&str> = vec!["restore", "--staged", "--"];
    args.extend(paths.iter().map(String::as_str));
    run_git(repo, &args).await?;
    Ok(())
}

/// `git commit -m <message>`. Returns the new commit's full sha and the
/// short (7+ char) sha via a follow-up `git rev-parse HEAD` + `--short`.
pub async fn commit(repo: &Path, message: &str) -> anyhow::Result<(String, String)> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("commit: empty message"));
    }
    run_git(repo, &["commit", "-m", trimmed]).await?;
    let sha = run_git(repo, &["rev-parse", "HEAD"]).await?;
    let sha = sha.trim().to_string();
    let short = run_git(repo, &["rev-parse", "--short", "HEAD"]).await?;
    let short = short.trim().to_string();
    Ok((sha, short))
}

/// Discard unstaged worktree changes for `paths`. Tracked entries are
/// reverted to the index via `git restore -- <paths>`; untracked entries
/// are removed via `git clean -fd -- <paths>`. We probe `git status
/// --porcelain` first to partition the input — running `restore` on an
/// untracked path errors with `pathspec did not match any file`, and
/// running `clean` on a tracked one is a no-op but still spawns a
/// subprocess. Empty input is rejected by the caller-friendly
/// `Err("discard: no paths")` to match the other helpers.
pub async fn discard(repo: &Path, paths: &[String]) -> anyhow::Result<()> {
    if paths.is_empty() {
        return Err(anyhow!("discard: no paths"));
    }
    let (tracked, untracked) = partition_tracked(repo, paths).await?;
    if !tracked.is_empty() {
        let mut args: Vec<&str> = vec!["restore", "--"];
        args.extend(tracked.iter().map(String::as_str));
        run_git(repo, &args).await?;
    }
    if !untracked.is_empty() {
        let mut args: Vec<&str> = vec!["clean", "-fd", "--"];
        args.extend(untracked.iter().map(String::as_str));
        run_git(repo, &args).await?;
    }
    Ok(())
}

/// Probe `git status --porcelain=1 -z -- <paths>` and bucket the input
/// paths into (tracked, untracked). Paths the porcelain output never
/// mentions fall into `tracked` — running `git restore` on a clean path
/// is a no-op and surfaces clearer errors than `git clean` would for
/// typos.
async fn partition_tracked(
    repo: &Path,
    paths: &[String],
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let mut args: Vec<&str> = vec!["status", "--porcelain=1", "-z", "--"];
    args.extend(paths.iter().map(String::as_str));
    let stdout = run_git(repo, &args).await?;
    let mut untracked: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut iter = stdout.split('\0');
    while let Some(entry) = iter.next() {
        if entry.is_empty() || entry.len() < 3 {
            continue;
        }
        let mut chars = entry.chars();
        let x = chars.next().unwrap_or(' ');
        let y = chars.next().unwrap_or(' ');
        if x == '?' && y == '?' {
            untracked.insert(entry[3..].to_string());
        } else if x == 'R' || y == 'R' {
            // Skip the rename source line that follows.
            let _ = iter.next();
        }
    }
    let mut tracked = Vec::new();
    let mut un = Vec::new();
    for p in paths {
        if untracked.contains(p) {
            un.push(p.clone());
        } else {
            tracked.push(p.clone());
        }
    }
    Ok((tracked, un))
}

/// `git stash push -u -m <message>`. Empty `message` is sent without the
/// `-m` flag so git falls back to its default subject.
pub async fn stash_push(repo: &Path, message: &str) -> anyhow::Result<()> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        run_git(repo, &["stash", "push", "-u"]).await?;
    } else {
        run_git(repo, &["stash", "push", "-u", "-m", trimmed]).await?;
    }
    Ok(())
}

/// `git stash list --format=...`. Returns the current stash list newest-first
/// (matching `git stash list`'s native order — `stash@{0}` is the most
/// recent push).
pub async fn stash_list(repo: &Path) -> anyhow::Result<Vec<GitStash>> {
    // Non-git folders registered as repos have no stashes; short-circuit
    // so the sidebar gets an empty list instead of a git error.
    if !repo.join(".git").exists() {
        return Ok(Vec::new());
    }
    let stdout = run_git(repo, &["stash", "list", "--format=%gd%x1f%gs%x1f%aI"]).await?;
    Ok(stdout.lines().filter_map(parse_stash_line).collect())
}

fn parse_stash_line(line: &str) -> Option<GitStash> {
    let mut parts = line.splitn(3, '\u{1f}');
    let id = parts.next()?.to_string();
    let subject = parts.next()?.to_string();
    let created_at = parts.next()?.to_string();
    Some(GitStash {
        id,
        subject,
        created_at,
    })
}

/// `git stash pop <stash_id>` — apply then drop.
pub async fn stash_pop(repo: &Path, stash_id: &str) -> anyhow::Result<()> {
    if stash_id.is_empty() {
        return Err(anyhow!("stash_pop: empty stash id"));
    }
    run_git(repo, &["stash", "pop", stash_id]).await?;
    Ok(())
}

/// `git stash apply <stash_id>` — apply without dropping.
pub async fn stash_apply(repo: &Path, stash_id: &str) -> anyhow::Result<()> {
    if stash_id.is_empty() {
        return Err(anyhow!("stash_apply: empty stash id"));
    }
    run_git(repo, &["stash", "apply", stash_id]).await?;
    Ok(())
}

/// `git stash drop <stash_id>` — drop without applying.
pub async fn stash_drop(repo: &Path, stash_id: &str) -> anyhow::Result<()> {
    if stash_id.is_empty() {
        return Err(anyhow!("stash_drop: empty stash id"));
    }
    run_git(repo, &["stash", "drop", stash_id]).await?;
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; panic messages aid debugging"
)]
mod tests {
    use super::*;

    #[test]
    fn parse_stash_line_extracts_fields() {
        let line = "stash@{0}\u{1f}WIP on main: abcdef0 fix: something\u{1f}2024-05-01T12:00:00Z";
        let parsed = parse_stash_line(line).expect("valid stash line parses");
        assert_eq!(parsed.id, "stash@{0}");
        assert_eq!(parsed.subject, "WIP on main: abcdef0 fix: something");
        assert_eq!(parsed.created_at, "2024-05-01T12:00:00Z");
    }

    #[test]
    fn parse_stash_line_rejects_short() {
        assert!(parse_stash_line("only-one-field").is_none());
        assert!(parse_stash_line("two\u{1f}fields").is_none());
    }
}
