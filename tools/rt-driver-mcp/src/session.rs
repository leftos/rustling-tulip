//! A named session: a driven client, the isolated daemon it talks to, and
//! the pipe to its driver endpoint. Dropping a session kills the client's
//! tree, then the daemon, which removes the scratch folder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Child;

use dev_daemon::IsolatedDaemon;
use serde_json::{Value, json};

use crate::pipe::PipeClient;

/// How many of `native.log`'s last lines an error carries.
pub const FEED_LINES: usize = 20;

/// One launched client and what it runs against.
pub struct Session {
    name: String,
    client: Child,
    pipe: PipeClient,
    scratch: PathBuf,
    native_log: PathBuf,
    /// Dropped after the client is killed (fields drop after `Drop::drop`).
    _daemon: IsolatedDaemon,
}

impl Session {
    #[must_use]
    pub fn new(name: String, client: Child, pipe: PipeClient, daemon: IsolatedDaemon) -> Self {
        Self {
            name,
            client,
            pipe,
            scratch: daemon.dir().to_path_buf(),
            native_log: native_log(&daemon.config_dir()),
            _daemon: daemon,
        }
    }

    pub fn pipe(&mut self) -> &mut PipeClient {
        &mut self.pipe
    }

    /// What `launch` returns: `{session, pid, pipe, scratch}`.
    #[must_use]
    pub fn summary(&self) -> Value {
        json!({
            "session": self.name,
            "pid": self.client.id(),
            "pipe": self.pipe.path(),
            "scratch": self.scratch.display().to_string(),
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
    use super::{FEED_LINES, with_feed};

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
