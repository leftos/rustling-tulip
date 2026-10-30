//! The client's copy of the Dashboard's facts: what the daemon streams to a
//! watcher, merged as it arrives.
//!
//! The daemon sends every listed session's facts to a fresh watcher and only
//! the entries that changed after that, so the store merges by session id and
//! keeps the sessions a message leaves out. Its totals and its Linear sync
//! stamp come with every message and simply replace the ones held.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use protocol::{DaemonMessage, DayTotals, SessionFacts};

use crate::target::HostKey;

/// The facts a Dashboard watcher holds: one entry per session, the day's
/// totals and the issue cache's sync stamp.
#[derive(Debug, Clone, Default)]
pub(crate) struct FactsStore {
    facts: BTreeMap<String, SessionFacts>,
    totals: DayTotals,
    linear_synced_at: Option<DateTime<Utc>>,
}

impl FactsStore {
    /// Merges a `DashboardFacts` message: an entry replaces that session's
    /// entry, and a session the message leaves out keeps its own. Any other
    /// message changes nothing.
    pub(crate) fn apply(&mut self, msg: &DaemonMessage) {
        let DaemonMessage::DashboardFacts {
            facts,
            totals,
            linear_synced_at,
        } = msg
        else {
            return;
        };
        for entry in facts {
            self.facts.insert(entry.session_id.clone(), entry.clone());
        }
        self.totals.clone_from(totals);
        self.linear_synced_at = *linear_synced_at;
    }

    /// The facts by session id.
    pub(crate) fn facts(&self) -> &BTreeMap<String, SessionFacts> {
        &self.facts
    }

    /// One session's facts, `None` until the daemon sends them.
    pub(crate) fn facts_for(&self, session_id: &str) -> Option<&SessionFacts> {
        self.facts.get(session_id)
    }

    /// The day's totals, for the header's figures and release bars.
    pub(crate) fn totals(&self) -> &DayTotals {
        &self.totals
    }

    /// When the issue cache last synced; `None` before the first sync.
    pub(crate) fn linear_synced_at(&self) -> Option<DateTime<Utc>> {
        self.linear_synced_at
    }

    /// Drops the facts of every session `listed` does not name. The daemon
    /// never resends an entry that did not change, so call this with the
    /// sessions the Dashboard lists rather than with a listing that is not
    /// known yet.
    pub(crate) fn prune<'a>(&mut self, listed: impl IntoIterator<Item = &'a str>) {
        let listed: HashSet<&str> = listed.into_iter().collect();
        self.facts
            .retain(|session_id, _| listed.contains(session_id.as_str()));
    }

    /// Whether the header's day figures mean anything: false while the
    /// totals are all zero and the issue cache never synced.
    pub(crate) fn figures_known(&self) -> bool {
        self.linear_synced_at.is_some()
            || self.totals.cost_usd > 0.0
            || self.totals.tokens != 0
            || self.totals.commits != 0
            || self.totals.issues_done != 0
            || !self.totals.releases.is_empty()
    }

    /// Forgets everything: a fresh watch is sent every fact again.
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
}

/// The Dashboard's facts from every host the window watches, one store per
/// host, so one host's relist, prune or reset leaves the others' alone.
#[derive(Debug, Default)]
pub(crate) struct HostFacts {
    stores: Vec<(HostKey, FactsStore)>,
}

impl HostFacts {
    /// Merges a `DashboardFacts` message from `host` into that host's store.
    pub(crate) fn apply(&mut self, host: &HostKey, msg: &DaemonMessage) {
        if let Some((_, store)) = self.stores.iter_mut().find(|(key, _)| key == host) {
            store.apply(msg);
        } else {
            let mut store = FactsStore::default();
            store.apply(msg);
            self.stores.push((host.clone(), store));
        }
    }

    /// Every host's facts by session id; session ids are unique across
    /// hosts.
    pub(crate) fn facts(&self) -> BTreeMap<String, SessionFacts> {
        self.stores
            .iter()
            .flat_map(|(_, store)| store.facts())
            .map(|(id, facts)| (id.clone(), facts.clone()))
            .collect()
    }

    /// One session's facts from whichever host sent them, `None` until one
    /// does.
    pub(crate) fn facts_for(&self, session_id: &str) -> Option<&SessionFacts> {
        self.stores
            .iter()
            .find_map(|(_, store)| store.facts_for(session_id))
    }

    /// `host`'s store, `None` before its first facts arrive.
    pub(crate) fn store(&self, host: &HostKey) -> Option<&FactsStore> {
        self.stores
            .iter()
            .find(|(key, _)| key == host)
            .map(|(_, store)| store)
    }

    /// Drops `host`'s facts of every session `listed` does not name.
    pub(crate) fn prune<'a>(&mut self, host: &HostKey, listed: impl IntoIterator<Item = &'a str>) {
        if let Some((_, store)) = self.stores.iter_mut().find(|(key, _)| key == host) {
            store.prune(listed);
        }
    }

    /// Forgets `host`'s facts: its fresh watch is sent every fact again.
    pub(crate) fn clear(&mut self, host: &HostKey) {
        if let Some((_, store)) = self.stores.iter_mut().find(|(key, _)| key == host) {
            store.clear();
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use protocol::ReleaseProgress;
    use serde_json::json;

    fn facts(id: &str, issue: &str) -> SessionFacts {
        serde_json::from_value(json!({
            "session_id": id,
            "now": { "kind": "thinking", "text": "Thinking" },
            "issue": issue,
        }))
        .expect("facts fixture")
    }

    fn message(facts: Vec<SessionFacts>) -> DaemonMessage {
        DaemonMessage::DashboardFacts {
            facts,
            totals: DayTotals::default(),
            linear_synced_at: None,
        }
    }

    fn issue_of(store: &FactsStore, id: &str) -> Option<String> {
        store.facts_for(id).and_then(|facts| facts.issue.clone())
    }

    #[test]
    fn first_message_fills_an_empty_store() {
        let mut store = FactsStore::default();
        store.apply(&message(vec![facts("s1", "RT-1"), facts("s2", "RT-2")]));
        assert_eq!(store.facts().len(), 2);
        assert_eq!(issue_of(&store, "s1").as_deref(), Some("RT-1"));
        assert_eq!(issue_of(&store, "s2").as_deref(), Some("RT-2"));
    }

    #[test]
    fn a_later_message_merges_changed_entries_only() {
        let mut store = FactsStore::default();
        store.apply(&message(vec![facts("s1", "RT-1"), facts("s2", "RT-2")]));
        store.apply(&message(vec![facts("s1", "RT-9")]));
        assert_eq!(
            issue_of(&store, "s1").as_deref(),
            Some("RT-9"),
            "the entry the message names replaces the one held"
        );
        assert_eq!(
            issue_of(&store, "s2").as_deref(),
            Some("RT-2"),
            "a session the message leaves out keeps its entry"
        );
    }

    #[test]
    fn an_entry_for_a_removed_session_is_pruned() {
        let mut store = FactsStore::default();
        store.apply(&message(vec![facts("s1", "RT-1"), facts("s2", "RT-2")]));
        store.prune(["s1"]);
        assert!(store.facts_for("s1").is_some());
        assert!(store.facts_for("s2").is_none());
    }

    #[test]
    fn clear_empties_facts_and_totals() {
        let mut store = FactsStore::default();
        let totals = DayTotals {
            cost_usd: 1.5,
            tokens: 2000,
            commits: 3,
            issues_done: 1,
            releases: vec![ReleaseProgress {
                name: "vNext".to_owned(),
                done: 1,
                total: 2,
            }],
        };
        store.apply(&DaemonMessage::DashboardFacts {
            facts: vec![facts("s1", "RT-1")],
            totals,
            linear_synced_at: Some(Utc::now()),
        });
        store.clear();
        assert!(store.facts().is_empty());
        assert_eq!(store.totals(), &DayTotals::default());
        assert_eq!(store.linear_synced_at(), None);
    }

    #[test]
    fn zero_totals_and_no_sync_read_as_unknown_figures() {
        let store = FactsStore::default();
        assert!(!store.figures_known(), "nothing measured yet");
        let mut counted = FactsStore::default();
        counted.apply(&DaemonMessage::DashboardFacts {
            facts: Vec::new(),
            totals: DayTotals {
                tokens: 1,
                ..DayTotals::default()
            },
            linear_synced_at: None,
        });
        assert!(counted.figures_known(), "a measured total is a fact");
        let mut synced = FactsStore::default();
        synced.apply(&DaemonMessage::DashboardFacts {
            facts: Vec::new(),
            totals: DayTotals::default(),
            linear_synced_at: Some(Utc::now()),
        });
        assert!(synced.figures_known(), "a sync stamps the figures as read");
    }
}
