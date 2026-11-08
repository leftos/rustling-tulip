//! A named session: a driven client, the isolated daemon it talks to, and
//! the pipe to its driver endpoint. Dropping a session kills the client's
//! tree, then the daemon, which removes the scratch folder, then removes the
//! scratch repo's folder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Child;

use dev_daemon::IsolatedDaemon;
use serde_json::{Value, json};

use crate::pipe::PipeClient;

/// How many of `native.log`'s last lines an error carries.
pub const FEED_LINES: usize = 20;

/// The folder under `<parent of the checkout>/.tmp/` that holds a session's
/// scratch repo.
const SCRATCH_REPO_FOLDER: &str = "rt-driver";

/// A session's scratch repo,
/// `<parent of repo_root>/.tmp/rt-driver/<session>-<server pid>/repo`: the
/// folder a driven agent works in, outside the checkout so no file of the
/// checkout's applies to it. The server's pid keeps a crashed server's
/// leftover folder and another server's from colliding with this one.
/// Dropping it removes `<parent of repo_root>/.tmp/rt-driver/<session>-<server pid>`.
pub struct ScratchRepo {
    root: PathBuf,
    repo: PathBuf,
}

impl ScratchRepo {
    /// Makes the scratch repo for session `name` of the checkout `repo_root`,
    /// refusing a session folder that is already there, which this server
    /// did not make and must not remove.
    ///
    /// # Errors
    ///
    /// Fails when `repo_root` has no parent folder, the session folder
    /// exists, or the folder cannot be made.
    pub fn create(repo_root: &Path, name: &str) -> Result<Self, String> {
        let repo_root = std::path::absolute(repo_root)
            .map_err(|err| format!("resolving {}: {err}", repo_root.display()))?;
        let Some(parent) = repo_root.parent() else {
            return Err(format!(
                "{} has no parent folder to hold the scratch repo",
                repo_root.display()
            ));
        };
        let root = parent
            .join(".tmp")
            .join(SCRATCH_REPO_FOLDER)
            .join(format!("{name}-{}", std::process::id()));
        if root.exists() {
            return Err(format!(
                "{} already exists: a run of an earlier server with this pid; \
                 remove it or pick another session name",
                root.display()
            ));
        }
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo)
            .map_err(|err| format!("making the scratch repo {}: {err}", repo.display()))?;
        Ok(Self { root, repo })
    }

    /// The scratch repo's folder.
    #[must_use]
    pub fn repo(&self) -> &Path {
        &self.repo
    }
}

impl Drop for ScratchRepo {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.root)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(folder = %self.root.display(), "removing the scratch repo: {err}");
        }
    }
}

/// One launched client and what it runs against.
pub struct Session {
    name: String,
    client: Child,
    pipe: PipeClient,
    scratch: PathBuf,
    native_log: PathBuf,
    repo_root: PathBuf,
    /// Dropped after the client is killed (fields drop after `Drop::drop`).
    _daemon: IsolatedDaemon,
    /// Dropped last, after the daemon and the client are gone.
    scratch_repo: ScratchRepo,
}

impl Session {
    #[must_use]
    pub fn new(
        name: String,
        client: Child,
        pipe: PipeClient,
        daemon: IsolatedDaemon,
        repo_root: PathBuf,
        scratch_repo: ScratchRepo,
    ) -> Self {
        Self {
            name,
            client,
            pipe,
            scratch: daemon.dir().to_path_buf(),
            native_log: native_log(&daemon.config_dir()),
            repo_root,
            _daemon: daemon,
            scratch_repo,
        }
    }

    pub fn pipe(&mut self) -> &mut PipeClient {
        &mut self.pipe
    }

    /// The checkout the session runs from.
    #[must_use]
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// What `launch` returns: `{session, pid, pipe, scratch, scratch_repo}`.
    #[must_use]
    pub fn summary(&self) -> Value {
        json!({
            "session": self.name,
            "pid": self.client.id(),
            "pipe": self.pipe.path(),
            "scratch": self.scratch.display().to_string(),
            "scratch_repo": self.scratch_repo.repo().display().to_string(),
        })
    }

    /// `text` with the client's `native.log` tail after it.
    #[must_use]
    pub fn with_feed(&self, text: &str) -> String {
        with_feed(text, &self.native_log)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if matches!(self.client.try_wait(), Ok(None)) {
            // The handle is held, so the pid cannot name another process.
            dev_daemon::kill_tree(self.client.id());
            let _ = self.client.kill();
        }
        if let Err(err) = self.client.wait() {
            tracing::warn!(session = %self.name, "waiting for the client to exit: {err}");
        }
    }
}

/// The client's log under its config dir.
#[must_use]
pub fn native_log(config_dir: &Path) -> PathBuf {
    config_dir.join("logs").join("native.log")
}

/// `text`, then the error feed: the last [`FEED_LINES`] lines of `log`.
#[must_use]
pub fn with_feed(text: &str, log: &Path) -> String {
    format!(
        "{text}\n\n--- error feed: the last {FEED_LINES} lines of {} ---\n{}",
        log.display(),
        log_tail(log)
    )
}

/// The last [`FEED_LINES`] lines of `log`, or why it cannot be read.
#[must_use]
pub fn log_tail(log: &Path) -> String {
    match std::fs::read(log) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(FEED_LINES)..].join("\n")
        }
        Err(err) => format!("({}: {err})", log.display()),
    }
}

/// The live sessions by name.
#[derive(Default)]
pub struct Sessions {
    live: BTreeMap<String, Session>,
}

impl Sessions {
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.live.contains_key(name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Session> {
        self.live.get_mut(name)
    }

    pub fn insert(&mut self, session: Session) {
        self.live.insert(session.name.clone(), session);
    }

    pub fn remove(&mut self, name: &str) -> Option<Session> {
        self.live.remove(name)
    }

    /// `live sessions: a, b`, or `live sessions: none`.
    #[must_use]
    pub fn listing(&self) -> String {
        if self.live.is_empty() {
            return "live sessions: none".to_owned();
        }
        let names: Vec<&str> = self.live.keys().map(String::as_str).collect();
        format!("live sessions: {}", names.join(", "))
    }

    /// Stops every session.
    pub fn stop_all(&mut self) {
        self.live.clear();
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the step it lost"
)]
mod tests {
    use super::{FEED_LINES, ScratchRepo, with_feed};

    /// The scratch repo sits beside the checkout, under
    /// `.tmp/rt-driver/<session>-<server pid>/repo`; that exact folder
    /// already there is refused, and drop removes the session folder.
    #[test]
    fn the_scratch_repo_is_named_by_session_and_server_pid() {
        let parent =
            std::env::temp_dir().join(format!("rt-driver-mcp-scratch-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        let checkout = parent.join("checkout");
        let folder = parent
            .join(".tmp")
            .join("rt-driver")
            .join(format!("unit-{}", std::process::id()));

        let made = ScratchRepo::create(&checkout, "unit");
        let again = ScratchRepo::create(&checkout, "unit").err();
        let repo = made.as_ref().ok().map(|made| made.repo().to_path_buf());
        let existed = folder.join("repo").is_dir();
        drop(made);
        let removed = !folder.exists();
        let _ = std::fs::remove_dir_all(&parent);

        assert_eq!(repo.expect("the scratch repo is made"), folder.join("repo"));
        assert!(existed, "{} was not made", folder.display());
        let again = again.expect("the same folder is refused while it exists");
        assert!(again.contains("already exists"), "{again}");
        assert!(removed, "drop left {}", folder.display());
    }

    #[test]
    fn error_feed_appends_the_native_log_tail() {
        let dir = std::env::temp_dir().join(format!("rt-driver-mcp-feed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("makes the fixture folder");
        let log = dir.join("native.log");
        let lines: Vec<String> = (1..=25).map(|n| format!("line {n}")).collect();
        std::fs::write(&log, lines.join("\n")).expect("writes the fixture log");

        let text = with_feed("call failed", &log);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(text.starts_with("call failed\n"), "{text}");
        let feed: Vec<&str> = text
            .lines()
            .skip_while(|line| !line.starts_with("--- error feed"))
            .skip(1)
            .collect();
        assert_eq!(feed.len(), FEED_LINES, "{text}");
        assert_eq!(feed.first(), Some(&"line 6"), "{text}");
        assert_eq!(feed.last(), Some(&"line 25"), "{text}");
    }
}
