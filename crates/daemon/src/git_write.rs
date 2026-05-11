//! Write-side git operations exposed by the source-control sidebar:
//! stage / unstage / commit. Each function shells out to `git` with the
//! repo's path as `-C`. Errors propagate via `anyhow::Result` so the
//! server can surface them as [`protocol::DaemonMessage::GitWriteError`].

use anyhow::{Context as _, anyhow};
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
