//! The undo shelf as plain data: an entry for a close or a move that can be
//! taken back for a few seconds, and the messages that take it back.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use protocol::{ClientMessage, GridNode, TabContent, TabEntry};

use crate::tabs::{SessionRef, host_of_wire};
use crate::target::HostKey;

/// How long an entry stays before it goes by itself.
pub const UNDO_LIFETIME: Duration = Duration::from_secs(8);
/// The most entries on screen at once.
pub const MAX_UNDO: usize = 3;
/// The undo button's label.
pub const UNDO_LABEL: &str = "Undo";
/// What the shelf says of a closed pane that shows nothing.
pub const CLOSED_EMPTY_PANE: &str = "Closed empty pane";
/// What the shelf says of a pane moved into another tab.
pub const MOVED_PANE: &str = "Moved pane";
/// What the shelf says of a same-tab centre drop, which swaps two sessions.
pub const SWAPPED_PANES: &str = "Swapped panes";

/// What the shelf says of a closed tab named `name`.
#[must_use]
pub fn closed_tab_message(name: &str) -> String {
    format!("Closed tab \"{name}\"")
}

/// What the shelf says of a closed pane whose session shows as `label`.
#[must_use]
pub fn closed_pane_message(label: &str) -> String {
    format!("Closed pane \"{label}\"")
}

/// A tab as it stood before the action, and how to put it back.
#[derive(Debug, Clone, PartialEq)]
pub struct TabSnapshot {
    pub tab: TabEntry,
    /// Its place in the tab list.
    pub index: usize,
    /// Whether it was the tab shown.
    pub restore_active: bool,
    /// The pane to focus when it comes back; none for a closed tab.
    pub focus_pane: Option<String>,
}

/// One thing that can be taken back.
#[derive(Debug, Clone, PartialEq)]
pub struct UndoEntry {
    pub id: u64,
    /// What the shelf says of it.
    pub message: String,
    pub snapshots: Vec<TabSnapshot>,
    pub expires_at: Instant,
}

impl UndoEntry {
    /// The snapshots it holds, in ascending tab-index order.
    #[must_use]
    pub fn ordered(&self) -> Vec<&TabSnapshot> {
        let mut ordered: Vec<&TabSnapshot> = self.snapshots.iter().collect();
        ordered.sort_by_key(|snapshot| snapshot.index);
        ordered
    }
}

/// The undo entries on screen, newest first.
#[derive(Debug, Default)]
pub struct UndoShelf {
    entries: Vec<UndoEntry>,
    next_id: u64,
}

impl UndoShelf {
    /// The entries, newest first.
    #[must_use]
    pub fn entries(&self) -> &[UndoEntry] {
        &self.entries
    }

    /// Whether anything can be taken back.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Shows `message` for `snapshots`, newest first: any older entry that
    /// touches one of the same tabs goes, and at most [`MAX_UNDO`] stay.
    /// Returns the new entry's id.
    pub fn push(&mut self, message: String, snapshots: Vec<TabSnapshot>, now: Instant) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let touched: HashSet<&str> = snapshots.iter().map(|s| s.tab.id.as_str()).collect();
        self.entries.retain(|entry| {
            !entry
                .snapshots
                .iter()
                .any(|s| touched.contains(s.tab.id.as_str()))
        });
        self.entries.insert(
            0,
            UndoEntry {
                id,
                message,
                snapshots,
                expires_at: now + UNDO_LIFETIME,
            },
        );
        self.entries.truncate(MAX_UNDO);
        id
    }

    /// The ✕ on entry `id`, sending nothing; returns whether it was shown.
    pub fn dismiss(&mut self, id: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        self.entries.len() != before
    }

    /// Takes entry `id` off the shelf, for the undo it stands for.
    pub fn take(&mut self, id: u64) -> Option<UndoEntry> {
        let at = self.entries.iter().position(|entry| entry.id == id)?;
        Some(self.entries.remove(at))
    }

    /// Drops the entries whose time is up at `now`; returns whether any went.
    pub fn expire(&mut self, now: Instant) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.expires_at > now);
        self.entries.len() != before
    }

    /// When the next entry goes; none when nothing is shown.
    #[must_use]
    pub fn next_expiry(&self) -> Option<Instant> {
        self.entries.iter().map(|entry| entry.expires_at).min()
    }

    /// Every entry goes: a fresh or lost connection.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// The messages that put `entry` back, in ascending tab-index order: a tab
/// still among `live_tab_ids` is replaced by
/// [`ClientMessage::RestoreTabSnapshot`], a tab that is gone returns at its
/// index by [`ClientMessage::RestoreTab`]. A snapshot pane of a host for
/// which `rehosted` names another comes back on that host; a pane whose
/// session, host and id both, is then not among `known_sessions` comes
/// back empty, with no host.
#[must_use]
pub fn restore_messages(
    entry: &UndoEntry,
    live_tab_ids: &HashSet<String>,
    known_sessions: &HashSet<SessionRef>,
    rehosted: &dyn Fn(&HostKey) -> Option<HostKey>,
) -> Vec<ClientMessage> {
    entry
        .ordered()
        .into_iter()
        .map(|snapshot| {
            let tab = keeping_known_sessions(&snapshot.tab, known_sessions, rehosted);
            if live_tab_ids.contains(&tab.id) {
                ClientMessage::RestoreTabSnapshot { tab }
            } else {
                ClientMessage::RestoreTab {
                    tab,
                    index: snapshot.index,
                }
            }
        })
        .collect()
}

/// Takes out of `entry` every closed viewer tab whose file a tab among
/// `live_tab_ids` already shows, since putting it back would show the same
/// file twice, and every closed Dashboard tab while a Dashboard tab is live,
/// since a client has one. Returns the ids of the open tabs that stand in for a closed
/// tab that was the tab shown, which show instead.
pub fn take_open_viewers(
    entry: &mut UndoEntry,
    tabs: &[TabEntry],
    live_tab_ids: &HashSet<String>,
) -> Vec<String> {
    let mut open = Vec::new();
    entry.snapshots.retain(
        |snapshot| match open_twin_of(&snapshot.tab, tabs, live_tab_ids) {
            Some(tab_id) => {
                if snapshot.restore_active {
                    open.push(tab_id.to_owned());
                }
                false
            }
            None => true,
        },
    );
    open
}

/// The live tab that stands in for `tab`, a tab that is gone: for a viewer
/// tab, a viewer tab of the same file; for a Dashboard tab, the Dashboard
/// tab, of which a client has one.
fn open_twin_of<'a>(
    tab: &TabEntry,
    tabs: &'a [TabEntry],
    live_tab_ids: &HashSet<String>,
) -> Option<&'a str> {
    if live_tab_ids.contains(&tab.id) {
        return None;
    }
    let stands_in = |other: &TabEntry| match (&tab.content, &other.content) {
        (TabContent::Viewer { path }, TabContent::Viewer { path: open }) => open == path,
        (TabContent::Dashboard, TabContent::Dashboard) => true,
        _ => false,
    };
    tabs.iter()
        .find(|other| live_tab_ids.contains(&other.id) && stands_in(other))
        .map(|other| other.id.as_str())
}

/// The listed tabs minus the closing ones: a tab whose removal the client
/// has asked for counts as gone, because the daemon handles the removal
/// before the restore that follows it, so only
/// [`ClientMessage::RestoreTab`] can put it back.
#[must_use]
pub fn live_tabs(tabs: &[TabEntry], closing: &HashSet<String>) -> HashSet<String> {
    tabs.iter()
        .filter(|tab| !closing.contains(&tab.id))
        .map(|tab| tab.id.clone())
        .collect()
}

/// `tab` with every pane of a rehosted host on its new host, and every
/// pane whose session the client no longer knows emptied.
fn keeping_known_sessions(
    tab: &TabEntry,
    known_sessions: &HashSet<SessionRef>,
    rehosted: &dyn Fn(&HostKey) -> Option<HostKey>,
) -> TabEntry {
    let mut tab = tab.clone();
    if let Some(grid) = tab.grid_mut() {
        *grid = emptied(grid, known_sessions, rehosted);
    }
    tab
}

/// `grid` with the panes of a host `rehosted` names another for on that
/// host, and the panes of forgotten sessions emptied, an emptied pane
/// naming no host.
fn emptied(
    grid: &GridNode,
    known_sessions: &HashSet<SessionRef>,
    rehosted: &dyn Fn(&HostKey) -> Option<HostKey>,
) -> GridNode {
    match grid {
        GridNode::Pane {
            pane_id,
            session_id,
            host,
        } => {
            let named = host_of_wire(host.as_deref());
            let host_key = rehosted(&named).unwrap_or(named);
            let known = session_id.as_ref().is_some_and(|id| {
                known_sessions.contains(&SessionRef::new(host_key.clone(), id.clone()))
            });
            GridNode::Pane {
                pane_id: pane_id.clone(),
                session_id: session_id.clone().filter(|_| known),
                host: host_key.wire().map(str::to_owned).filter(|_| known),
            }
        }
        GridNode::Split {
            direction,
            ratio,
            first,
            second,
        } => GridNode::Split {
            direction: *direction,
            ratio: *ratio,
            first: Box::new(emptied(first, known_sessions, rehosted)),
            second: Box::new(emptied(second, known_sessions, rehosted)),
        },
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    /// A tab of one pane showing `session`, or an empty pane when none.
    fn tab_of(id: &str, session: Option<&str>) -> TabEntry {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": id,
            "content": {
                "kind": "grid",
                "grid": { "kind": "pane", "pane_id": format!("p{id}"), "session_id": session },
            },
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .expect("a tab fixture")
    }

    /// A snapshot of `id` at `index`, restoring the active tab when `active`.
    fn snapshot(id: &str, index: usize, active: bool, focus: Option<&str>) -> TabSnapshot {
        TabSnapshot {
            tab: tab_of(id, Some("s1")),
            index,
            restore_active: active,
            focus_pane: focus.map(str::to_owned),
        }
    }

    fn ids(values: &[&str]) -> HashSet<String> {
        values.iter().map(|id| (*id).to_owned()).collect()
    }

    /// The tab each restore message carries, in the order they went out.
    fn restored(messages: &[ClientMessage]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|msg| restored_tab(msg).map(|tab| tab.id.clone()))
            .collect()
    }

    /// The tab a restore message carries, whichever kind it is.
    fn restored_tab(msg: &ClientMessage) -> Option<&TabEntry> {
        match msg {
            ClientMessage::RestoreTab { tab, .. } | ClientMessage::RestoreTabSnapshot { tab } => {
                Some(tab)
            }
            _ => None,
        }
    }

    /// The panes of `tab`, left to right, as `(pane id, session)`.
    fn panes_of(tab: &TabEntry) -> Vec<(String, Option<String>)> {
        fn walk(node: &GridNode, out: &mut Vec<(String, Option<String>)>) {
            match node {
                GridNode::Pane {
                    pane_id,
                    session_id,
                    ..
                } => {
                    out.push((pane_id.clone(), session_id.clone()));
                }
                GridNode::Split { first, second, .. } => {
                    walk(first, out);
                    walk(second, out);
                }
            }
        }
        let mut out = Vec::new();
        if let Some(grid) = tab.grid() {
            walk(grid, &mut out);
        }
        out
    }

    fn messages(shelf: &UndoShelf) -> Vec<String> {
        shelf
            .entries()
            .iter()
            .map(|entry| entry.message.clone())
            .collect()
    }

    #[test]
    fn newest_entry_shows_first() {
        let start = Instant::now();
        let mut shelf = UndoShelf::default();
        shelf.push(
            closed_tab_message("a"),
            vec![snapshot("t1", 0, false, None)],
            start,
        );
        shelf.push(
            closed_tab_message("b"),
            vec![snapshot("t2", 1, false, None)],
            start,
        );

        assert_eq!(
            messages(&shelf),
            ["Closed tab \"b\"", "Closed tab \"a\""],
            "the new entry goes on top"
        );
    }

    #[test]
    fn shelf_keeps_the_three_newest() {
        let start = Instant::now();
        let mut shelf = UndoShelf::default();
        for (i, id) in ["t1", "t2", "t3", "t4"].into_iter().enumerate() {
            shelf.push(
                closed_tab_message(id),
                vec![snapshot(id, i, false, None)],
                start,
            );
        }

        assert_eq!(shelf.entries().len(), MAX_UNDO);
        assert_eq!(
            messages(&shelf),
            [
                "Closed tab \"t4\"",
                "Closed tab \"t3\"",
                "Closed tab \"t2\""
            ]
        );
        assert!(!shelf.is_empty());
    }

    #[test]
    fn an_older_entry_touching_the_same_tab_goes() {
        let start = Instant::now();
        let mut shelf = UndoShelf::default();
        shelf.push(
            closed_tab_message("a"),
            vec![snapshot("t1", 0, false, None)],
            start,
        );
        shelf.push(
            closed_tab_message("b"),
            vec![snapshot("t2", 1, false, None)],
            start,
        );
        shelf.push(
            closed_tab_message("c"),
            vec![snapshot("t1", 0, false, Some("p"))],
            start,
        );

        assert_eq!(
            messages(&shelf),
            ["Closed tab \"c\"", "Closed tab \"b\""],
            "the older entry of t1 goes, the one of another tab stays"
        );
    }

    #[test]
    fn entry_expires_at_eight_seconds() {
        let start = Instant::now();
        let mut shelf = UndoShelf::default();
        shelf.push(
            closed_tab_message("a"),
            vec![snapshot("t1", 0, false, None)],
            start,
        );
        assert_eq!(shelf.next_expiry(), Some(start + UNDO_LIFETIME));

        assert!(!shelf.expire(start + Duration::from_millis(7_999)));
        assert_eq!(shelf.entries().len(), 1, "it is still there");
        assert!(
            shelf.expire(start + UNDO_LIFETIME),
            "at eight seconds it goes"
        );
        assert!(shelf.is_empty());
        assert_eq!(shelf.next_expiry(), None);
    }

    #[test]
    fn next_expiry_is_the_earliest() {
        let start = Instant::now();
        let mut shelf = UndoShelf::default();
        let late = start + Duration::from_secs(4);
        shelf.push(
            closed_tab_message("a"),
            vec![snapshot("t1", 0, false, None)],
            late,
        );
        shelf.push(
            closed_tab_message("b"),
            vec![snapshot("t2", 1, false, None)],
            start,
        );

        assert_eq!(
            shelf.next_expiry(),
            Some(start + UNDO_LIFETIME),
            "the entry whose time is up first sets the timer"
        );
        assert!(shelf.expire(start + UNDO_LIFETIME), "that one goes");
        assert_eq!(
            messages(&shelf),
            ["Closed tab \"a\""],
            "the later one stays"
        );
        assert_eq!(shelf.next_expiry(), Some(late + UNDO_LIFETIME));
    }

    #[test]
    fn dismiss_and_take_remove_one_entry() {
        let now = Instant::now();
        let mut shelf = UndoShelf::default();
        let first = shelf.push(
            closed_tab_message("a"),
            vec![snapshot("t1", 0, false, None)],
            now,
        );
        let second = shelf.push(
            closed_tab_message("b"),
            vec![snapshot("t2", 1, false, None)],
            now,
        );
        assert_ne!(first, second, "each entry has its own id");

        assert!(shelf.dismiss(first));
        assert!(!shelf.dismiss(first), "an entry goes once");
        assert_eq!(messages(&shelf), ["Closed tab \"b\""]);

        let taken = shelf.take(second).expect("the entry is there");
        assert_eq!(taken.id, second);
        assert_eq!(taken.message, "Closed tab \"b\"");
        assert!(shelf.take(second).is_none(), "and it goes once");
    }

    #[test]
    fn restore_revives_a_gone_tab_and_replaces_a_live_one_in_index_order() {
        let now = Instant::now();
        let mut shelf = UndoShelf::default();
        let id = shelf.push(
            MOVED_PANE.to_owned(),
            vec![
                snapshot("t2", 3, false, Some("p2")),
                snapshot("t1", 1, false, Some("p1")),
            ],
            now,
        );
        let entry = shelf.take(id).expect("the entry is there");

        let sent = restore_messages(
            &entry,
            &ids(&["t2"]),
            &known(&[(HostKey::Local, "s1")]),
            &|_| None,
        );
        assert_eq!(restored(&sent), ["t1", "t2"], "ascending index");
        assert!(
            matches!(&sent[0], ClientMessage::RestoreTab { tab, index } if tab.id == "t1" && *index == 1),
            "the gone tab returns at its index: {sent:?}"
        );
        assert!(
            matches!(&sent[1], ClientMessage::RestoreTabSnapshot { tab } if tab.id == "t2"),
            "the live tab is replaced in place: {sent:?}"
        );
    }

    #[test]
    fn a_move_whose_source_tab_is_gone_restores_it_and_replaces_the_other() {
        let now = Instant::now();
        let mut shelf = UndoShelf::default();
        let id = shelf.push(
            MOVED_PANE.to_owned(),
            vec![
                snapshot("t1", 0, true, Some("p1")),
                snapshot("t2", 1, false, Some("p2")),
            ],
            now,
        );
        let entry = shelf.take(id).expect("the entry is there");

        let sent = restore_messages(
            &entry,
            &ids(&["t2"]),
            &known(&[(HostKey::Local, "s1")]),
            &|_| None,
        );
        assert_eq!(restored(&sent), ["t1", "t2"]);
        assert!(
            matches!(&sent[0], ClientMessage::RestoreTab { tab, index } if tab.id == "t1" && *index == 0),
            "the source tab is gone, so it comes back: {sent:?}"
        );
        assert!(
            matches!(&sent[1], ClientMessage::RestoreTabSnapshot { tab } if tab.id == "t2"),
            "the destination is still open: {sent:?}"
        );
    }

    #[test]
    fn a_tab_being_closed_counts_as_gone() {
        let now = Instant::now();
        let mut shelf = UndoShelf::default();
        let id = shelf.push(
            closed_tab_message("t1"),
            vec![snapshot("t1", 1, false, None)],
            now,
        );
        let entry = shelf.take(id).expect("the entry is there");

        let listed = [tab_of("t2", Some("s1")), tab_of("t1", Some("s1"))];
        let live = live_tabs(&listed, &ids(&["t1"]));
        assert_eq!(live, ids(&["t2"]), "the closing tab is not live");

        let sent = restore_messages(&entry, &live, &known(&[(HostKey::Local, "s1")]), &|_| None);
        assert!(
            matches!(sent.as_slice(), [ClientMessage::RestoreTab { tab, index }] if tab.id == "t1" && *index == 1),
            "its removal is on its way, so it comes back at its index: {sent:?}"
        );
    }

    #[test]
    fn a_move_whose_source_is_closing_restores_it_and_replaces_the_destination() {
        let now = Instant::now();
        let mut shelf = UndoShelf::default();
        let id = shelf.push(
            MOVED_PANE.to_owned(),
            vec![
                snapshot("t1", 0, true, Some("p1")),
                snapshot("t2", 1, false, Some("p2")),
            ],
            now,
        );
        let entry = shelf.take(id).expect("the entry is there");

        let listed = [tab_of("t1", Some("s1")), tab_of("t2", Some("s1"))];
        let live = live_tabs(&listed, &ids(&["t1"]));
        let sent = restore_messages(&entry, &live, &known(&[(HostKey::Local, "s1")]), &|_| None);
        assert_eq!(restored(&sent), ["t1", "t2"]);
        assert!(
            matches!(&sent[0], ClientMessage::RestoreTab { tab, index } if tab.id == "t1" && *index == 0),
            "the tab the pane left is closing, so it comes back: {sent:?}"
        );
        assert!(
            matches!(&sent[1], ClientMessage::RestoreTabSnapshot { tab } if tab.id == "t2"),
            "the destination is still open: {sent:?}"
        );
    }

    #[test]
    fn a_snapshot_pane_of_a_forgotten_session_comes_back_empty() {
        let now = Instant::now();
        let grid: GridNode = serde_json::from_value(serde_json::json!({
            "kind": "split",
            "direction": "horizontal",
            "ratio": 0.5,
            "first": { "kind": "pane", "pane_id": "p1", "session_id": "s1" },
            "second": { "kind": "pane", "pane_id": "p2", "session_id": "s2" },
        }))
        .expect("a grid fixture");
        let mut tab = tab_of("t1", None);
        tab.content = protocol::TabContent::Grid { grid };
        let mut shelf = UndoShelf::default();
        let id = shelf.push(
            closed_pane_message("s2"),
            vec![TabSnapshot {
                tab,
                index: 0,
                restore_active: true,
                focus_pane: Some("p2".to_owned()),
            }],
            now,
        );
        let entry = shelf.take(id).expect("the entry is there");

        let sent = restore_messages(
            &entry,
            &ids(&["t1"]),
            &known(&[(HostKey::Local, "s1")]),
            &|_| None,
        );
        assert!(
            matches!(sent.as_slice(), [ClientMessage::RestoreTabSnapshot { .. }]),
            "the tab is still open, so it is replaced in place: {sent:?}"
        );
        let tab = restored_tab(&sent[0]).expect("a restore message");
        assert_eq!(
            panes_of(tab),
            [
                ("p1".to_owned(), Some("s1".to_owned())),
                ("p2".to_owned(), None)
            ],
            "the pane whose session is gone comes back empty"
        );
    }

    use crate::target::HostKey;

    /// The sessions the client knows, each with its host.
    fn known(sessions: &[(HostKey, &str)]) -> HashSet<SessionRef> {
        sessions
            .iter()
            .map(|(host, id)| SessionRef::new(host.clone(), *id))
            .collect()
    }

    #[test]
    fn a_snapshot_pane_of_a_rehosted_key_restores_under_its_target() {
        let grid: GridNode = serde_json::from_value(serde_json::json!({
            "kind": "split",
            "direction": "horizontal",
            "ratio": 0.5,
            "first": { "kind": "pane", "pane_id": "p1", "session_id": "d1", "host": "ssh:me@devbox" },
            "second": { "kind": "pane", "pane_id": "p2", "session_id": "s1" },
        }))
        .expect("a grid fixture");
        let mut tab = tab_of("t1", None);
        tab.content = protocol::TabContent::Grid { grid };
        let mut shelf = UndoShelf::default();
        let id = shelf.push(
            closed_pane_message("d1"),
            vec![TabSnapshot {
                tab,
                index: 0,
                restore_active: true,
                focus_pane: None,
            }],
            Instant::now(),
        );
        let entry = shelf.take(id).expect("the entry is there");
        let ssh = HostKey::Remote("ssh:me@devbox".to_owned());
        let owner = HostKey::Remote("box".to_owned());
        let rehosted = |host: &HostKey| (*host == ssh).then(|| owner.clone());

        let sent = restore_messages(
            &entry,
            &ids(&[]),
            &known(&[(owner.clone(), "d1"), (HostKey::Local, "s1")]),
            &rehosted,
        );

        let grid = restored_tab(&sent[0])
            .and_then(TabEntry::grid)
            .expect("a restored grid");
        let expected: GridNode = serde_json::from_value(serde_json::json!({
            "kind": "split",
            "direction": "horizontal",
            "ratio": 0.5,
            "first": { "kind": "pane", "pane_id": "p1", "session_id": "d1", "host": "box" },
            "second": { "kind": "pane", "pane_id": "p2", "session_id": "s1" },
        }))
        .expect("the expected grid");
        assert_eq!(
            grid, &expected,
            "the rehosted key's pane comes back on its target; This PC's pane keeps its host"
        );
    }

    #[test]
    fn a_pane_whose_host_no_longer_lists_its_session_comes_back_empty_and_hostless() {
        let grid: GridNode = serde_json::from_value(serde_json::json!({
            "kind": "split",
            "direction": "horizontal",
            "ratio": 0.5,
            "first": { "kind": "pane", "pane_id": "p1", "session_id": "b1", "host": "box" },
            "second": {
                "kind": "split",
                "direction": "vertical",
                "ratio": 0.5,
                "first": { "kind": "pane", "pane_id": "p2", "session_id": "b2", "host": "box" },
                "second": { "kind": "pane", "pane_id": "p3", "session_id": "b1" },
            },
        }))
        .expect("a grid fixture");
        let mut tab = tab_of("t1", None);
        tab.content = protocol::TabContent::Grid { grid };
        let mut shelf = UndoShelf::default();
        let id = shelf.push(
            closed_pane_message("b2"),
            vec![TabSnapshot {
                tab,
                index: 0,
                restore_active: true,
                focus_pane: None,
            }],
            Instant::now(),
        );
        let entry = shelf.take(id).expect("the entry is there");
        let box_host = HostKey::Remote("box".to_owned());

        let sent = restore_messages(&entry, &ids(&[]), &known(&[(box_host, "b1")]), &|_| None);

        let pane = |id: &str, session: Option<&str>, host: Option<&str>| serde_json::json!({ "kind": "pane", "pane_id": id, "session_id": session, "host": host });
        let grid = restored_tab(&sent[0])
            .and_then(TabEntry::grid)
            .expect("a restored grid");
        let expected: GridNode = serde_json::from_value(serde_json::json!({
                "kind": "split",
                "direction": "horizontal",
                "ratio": 0.5,
                "first": pane("p1", Some("b1"), Some("box")),
                "second": {
                    "kind": "split",
                    "direction": "vertical",
                    "ratio": 0.5,
                    "first": pane("p2", None, None),
                "second": pane("p3", None, None),
            },
        }))
        .expect("the expected grid");
        assert_eq!(
            grid, &expected,
            "the remote host's b1 stays; its b2 and This PC's b1 come back empty, with no host"
        );
    }
}
