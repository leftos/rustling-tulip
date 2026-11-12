//! A listed run's transcript: the chat entries its stream's records parse
//! into, the newest [`CHAT_REPLY_CAP`] of them kept, and the connections that
//! follow it. Every method runs under the runs lock, so a follow's backfill
//! and the appends after it reach a follower in the log's own order.

use crate::chat_mirror::{CHAT_REPLY_CAP, entry_id};
use agent_api::AgentPlugin;
use protocol::{ChatEntry, DaemonMessage, TranscriptSource};
use tokio::sync::mpsc;

/// A connection's id, unique for the daemon's run.
pub(crate) type ConnectionId = u64;

/// A connection's outbound queue.
pub(crate) type Outbox = mpsc::UnboundedSender<DaemonMessage>;

/// One run's entries and followers.
#[derive(Debug, Default)]
pub(super) struct TranscriptLog {
    entries: Vec<ChatEntry>,
    /// How many entries the cap dropped in front of `entries`.
    hidden: u32,
    /// One per following connection.
    followers: Vec<(ConnectionId, Outbox)>,
}

impl TranscriptLog {
    /// Upsert `new` by id in order: a held id is replaced in place, any other
    /// entry appended. Then the oldest entries past the cap are dropped and
    /// counted in `hidden`. Returns the entries applied, in order.
    pub(super) fn apply(&mut self, new: Vec<ChatEntry>) -> Vec<ChatEntry> {
        for entry in &new {
            let id = entry_id(entry);
            let held = self
                .entries
                .iter_mut()
                .find(|held| id.is_some() && entry_id(held) == id);
            match held {
                Some(held) => *held = entry.clone(),
                None => self.entries.push(entry.clone()),
            }
        }
        let excess = self.entries.len().saturating_sub(CHAT_REPLY_CAP);
        if excess > 0 {
            self.entries.drain(..excess);
            let dropped = u32::try_from(excess).unwrap_or(u32::MAX);
            self.hidden = self.hidden.saturating_add(dropped);
        }
        new
    }

    /// The held entry under `id`, for the parser's `known`.
    pub(super) fn get(&self, id: &str) -> Option<ChatEntry> {
        self.entries
            .iter()
            .find(|held| entry_id(held) == Some(id))
            .cloned()
    }

    /// Clear the entries and `hidden`; the followers stay.
    pub(super) fn reset(&mut self) {
        self.entries.clear();
        self.hidden = 0;
    }

    /// Parse `lines` with `plugin` and apply each line's entries in line
    /// order, then tell the followers: after a `restart` (cleared first) the
    /// whole log, else the entries applied, when there are any.
    pub(super) fn fold(
        &mut self,
        source: &TranscriptSource,
        restart: bool,
        lines: &[String],
        plugin: &dyn AgentPlugin,
    ) {
        if restart {
            self.reset();
        }
        let mut applied = Vec::new();
        for line in lines {
            let new = plugin.chat_entries(line, &|id| self.get(id));
            applied.extend(self.apply(new));
        }
        if restart {
            let whole = self.backfill(source, None);
            self.publish(&whole);
        } else if !applied.is_empty() {
            self.publish(&DaemonMessage::TranscriptAppend {
                source: source.clone(),
                entries: applied,
                ended: false,
            });
        }
    }

    /// Clear the log and send the followers the empty log in place of theirs.
    pub(super) fn restarted(&mut self, source: &TranscriptSource) {
        self.reset();
        let empty = self.backfill(source, None);
        self.publish(&empty);
    }

    /// The whole log as a `Transcript` answering `request_id`.
    fn backfill(&self, source: &TranscriptSource, request_id: Option<String>) -> DaemonMessage {
        DaemonMessage::Transcript {
            source: source.clone(),
            entries: self.entries.clone(),
            hidden: self.hidden,
            request_id,
        }
    }

    /// Send `message` to every follower, dropping each whose connection is
    /// closed.
    fn publish(&mut self, message: &DaemonMessage) {
        self.followers
            .retain(|(_, outbox)| outbox.send(message.clone()).is_ok());
    }

    /// Make `connection` a follower, in place of any earlier follow of its,
    /// and send it the whole log answering `request_id`.
    pub(super) fn follow(
        &mut self,
        connection: ConnectionId,
        outbox: Outbox,
        source: &TranscriptSource,
        request_id: Option<String>,
    ) {
        self.unfollow(connection);
        if outbox.send(self.backfill(source, request_id)).is_ok() {
            self.followers.push((connection, outbox));
        }
    }

    /// Stop sending to `connection`; nothing when it does not follow.
    pub(super) fn unfollow(&mut self, connection: ConnectionId) {
        self.followers
            .retain(|(follower, _)| *follower != connection);
    }

    /// Tell the followers the run ended.
    pub(super) fn end(&self, source: &TranscriptSource) {
        for (_, outbox) in &self.followers {
            // A closed connection is fine: the log goes with the run.
            let _ = outbox.send(DaemonMessage::TranscriptAppend {
                source: source.clone(),
                entries: Vec::new(),
                ended: true,
            });
        }
    }

    /// The following connections, oldest follow first.
    #[cfg(test)]
    pub(super) fn followers(&self) -> Vec<ConnectionId> {
        self.followers
            .iter()
            .map(|(connection, _)| *connection)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(id: &str, text: &str) -> ChatEntry {
        ChatEntry::Answer {
            id: id.to_owned(),
            text: text.to_owned(),
            live: false,
        }
    }

    fn ids(entries: &[ChatEntry]) -> Vec<&str> {
        entries.iter().filter_map(entry_id).collect()
    }

    #[test]
    fn apply_appends_new_ids_and_replaces_known_ones_in_place() {
        let mut log = TranscriptLog::default();
        let first = log.apply(vec![answer("a", "one"), answer("b", "two")]);
        assert_eq!(first, [answer("a", "one"), answer("b", "two")]);

        let second = log.apply(vec![answer("a", "one, again"), answer("c", "three")]);
        assert_eq!(second, [answer("a", "one, again"), answer("c", "three")]);
        assert_eq!(
            log.entries,
            [
                answer("a", "one, again"),
                answer("b", "two"),
                answer("c", "three")
            ]
        );
        assert_eq!(log.get("a"), Some(answer("a", "one, again")));
        assert_eq!(log.get("z"), None);
    }

    #[test]
    fn apply_drops_the_oldest_past_the_cap_and_counts_them_hidden() {
        let mut log = TranscriptLog::default();
        let many: Vec<ChatEntry> = (0..CHAT_REPLY_CAP + 3)
            .map(|n| answer(&n.to_string(), "x"))
            .collect();
        let applied = log.apply(many);
        assert_eq!(applied.len(), CHAT_REPLY_CAP + 3);
        assert_eq!(log.entries.len(), CHAT_REPLY_CAP);
        assert_eq!(ids(&log.entries).first(), Some(&"3"));
        assert_eq!(log.hidden, 3);
        assert_eq!(log.get("0"), None, "a dropped entry is no longer known");

        log.apply(vec![answer("new", "y")]);
        assert_eq!(log.entries.len(), CHAT_REPLY_CAP);
        assert_eq!(log.hidden, 4);
    }

    #[test]
    fn reset_clears_entries_and_hidden_and_keeps_followers() {
        let mut log = TranscriptLog::default();
        let (outbox, _inbox) = mpsc::unbounded_channel();
        let source = TranscriptSource::DispatchRun {
            run_id: "r1".to_owned(),
        };
        log.follow(7, outbox, &source, None);
        log.apply(
            (0..=CHAT_REPLY_CAP)
                .map(|n| answer(&n.to_string(), "x"))
                .collect(),
        );

        log.reset();
        assert_eq!(log.entries, Vec::<ChatEntry>::new());
        assert_eq!(log.hidden, 0);
        assert_eq!(log.followers(), [7]);
    }
}
