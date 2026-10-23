//! The per-agent store of contested conversation ids: ids that two or more
//! sessions waiting in one folder were each eligible for, so discovery gave
//! them to nobody. A stored id is never claimed until its entry expires.
//!
//! `<config>/contested/<agent id>.json` holds a JSON array of
//! `{id, contested_at}`. Entries older than [`CONTESTED_RETENTION`] are
//! dropped when the file is read and left out of its next write.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use agent_api::is_plain_id;
use anyhow::{Context as _, anyhow};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::orphan;
use crate::paths::Dirs;
use crate::sync::lock;

/// How long a contested id stays contested.
pub(crate) const CONTESTED_RETENTION: TimeDelta = TimeDelta::days(7);

/// Serializes every read-modify-write of a contested file in this daemon.
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// One contested id and when it became contested.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    id: String,
    contested_at: DateTime<Utc>,
}

/// The file for `agent`, or an error when the agent id cannot be a file stem.
fn store_path(dirs: &Dirs, agent: &str) -> anyhow::Result<PathBuf> {
    if !is_plain_id(agent) {
        return Err(anyhow!(
            "agent id {agent:?} is not a plain id; it cannot name a contested store"
        ));
    }
    Ok(dirs.contested_dir().join(format!("{agent}.json")))
}

/// The unexpired entries in `agent`'s file at `now`. A missing file holds
/// none; an unreadable one is warned about and read as none.
fn load(dirs: &Dirs, agent: &str, now: DateTime<Utc>) -> anyhow::Result<Vec<Entry>> {
    let path = store_path(dirs, agent)?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let entries: Vec<Entry> = match serde_json::from_slice(&bytes) {
        Ok(entries) => entries,
        Err(err) => {
            warn!(?err, path = %path.display(), "unparseable contested store; reading it as empty");
            Vec::new()
        }
    };
    Ok(entries
        .into_iter()
        .filter(|entry| now - entry.contested_at <= CONTESTED_RETENTION)
        .collect())
}

/// The ids contested for `agent` at `now`. A store that cannot be read is
/// warned about and answers none.
pub(crate) fn read(dirs: &Dirs, agent: &str, now: DateTime<Utc>) -> HashSet<String> {
    let _guard = lock(&STORE_LOCK);
    match load(dirs, agent, now) {
        Ok(entries) => entries.into_iter().map(|entry| entry.id).collect(),
        Err(err) => {
            warn!(?err, %agent, "could not read the contested store");
            HashSet::new()
        }
    }
}

/// Store `id` as contested for `agent` at `now`, unless it already is, and
/// drop the expired entries.
pub(crate) fn add(dirs: &Dirs, agent: &str, id: &str, now: DateTime<Utc>) -> anyhow::Result<()> {
    let _guard = lock(&STORE_LOCK);
    let mut entries = load(dirs, agent, now)?;
    if !entries.iter().any(|entry| entry.id == id) {
        entries.push(Entry {
            id: id.to_owned(),
            contested_at: now,
        });
    }
    let dir = dirs.contested_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let bytes = serde_json::to_vec_pretty(&entries).context("serializing the contested store")?;
    orphan::write_atomic(&store_path(dirs, agent)?, &bytes)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{CONTESTED_RETENTION, add, read};
    use crate::history::test_support::scratch_dirs;
    use chrono::{TimeDelta, Utc};
    use std::collections::HashSet;

    fn ids(list: &[&str]) -> HashSet<String> {
        list.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn contested_ids_round_trip_per_agent() {
        let dirs = scratch_dirs("contested-round-trip");
        let now = Utc::now();
        assert_eq!(read(&dirs, "agent-a", now), ids(&[]), "nothing stored yet");
        add(&dirs, "agent-a", "c-1", now).expect("store c-1");
        add(&dirs, "agent-a", "c-2", now).expect("store c-2");
        add(&dirs, "agent-a", "c-1", now).expect("store c-1 again");
        add(&dirs, "agent-b", "c-3", now).expect("store c-3");
        assert_eq!(read(&dirs, "agent-a", now), ids(&["c-1", "c-2"]));
        assert_eq!(read(&dirs, "agent-b", now), ids(&["c-3"]));
        assert!(dirs.contested_dir().join("agent-a.json").is_file());
    }

    #[test]
    fn contested_entries_expire_after_seven_days() {
        let dirs = scratch_dirs("contested-expiry");
        let now = Utc::now();
        let old = now - CONTESTED_RETENTION - TimeDelta::minutes(1);
        let recent = now - CONTESTED_RETENTION + TimeDelta::minutes(1);
        add(&dirs, "agent-a", "old", old).expect("store old");
        add(&dirs, "agent-a", "recent", recent).expect("store recent");
        assert_eq!(
            read(&dirs, "agent-a", now),
            ids(&["recent"]),
            "dropped on read"
        );
        add(&dirs, "agent-a", "new", now).expect("store new");
        let text = std::fs::read_to_string(dirs.contested_dir().join("agent-a.json"))
            .expect("read the store");
        assert!(
            !text.contains("\"old\""),
            "dropped on the next write: {text}"
        );
        assert_eq!(read(&dirs, "agent-a", now), ids(&["recent", "new"]));
    }
}
