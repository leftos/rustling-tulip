//! The summary view's model: one session's summary lines in the daemon's
//! order with the lines the user opened, the chat entries an open line
//! shows, and which sessions' chats and summaries the panes want loaded.
//!
//! It is pure data over the protocol's `StretchLine` and the chat model.

use std::collections::{BTreeSet, HashSet};

use protocol::{ChatEntry, StretchLine};

use crate::chat::{self, ChatLog, PaneView};

/// One session's summary lines in the daemon's order, the lines the user
/// opened (by line id), and the `LoadSummary` whose reply is still awaited.
#[derive(Debug, Default)]
pub(crate) struct SummaryLog {
    lines: Vec<StretchLine>,
    open: HashSet<String>,
    request: Option<String>,
}

impl SummaryLog {
    /// A summary asked for under `request_id`, its reply on the way.
    pub(crate) fn asked(request_id: String) -> Self {
        Self {
            request: Some(request_id),
            ..Self::default()
        }
    }

    /// Whether the reply to this summary's load is still awaited.
    pub(crate) fn is_loading(&self) -> bool {
        self.request.is_some()
    }

    /// The request id of the load still awaited.
    pub(crate) fn request(&self) -> Option<&str> {
        self.request.as_deref()
    }

    /// Applies a `Summary`: the reply to the awaited load, or one the
    /// daemon sent unasked (`request_id` none), replaces every line, and the
    /// open lines that survive stay open. A reply to an older load changes
    /// nothing. Returns whether it applied.
    pub(crate) fn apply_summary(
        &mut self,
        lines: Vec<StretchLine>,
        request_id: Option<&str>,
    ) -> bool {
        match request_id {
            Some(id) if self.request.as_deref() == Some(id) => self.request = None,
            Some(_) => return false,
            None => {}
        }
        self.lines = lines;
        let lines = &self.lines;
        self.open
            .retain(|id| lines.iter().any(|line| &line.id == id));
        true
    }

    /// Applies a `SummaryAppend`: a known id is replaced in its place, a
    /// new one is added at the end.
    pub(crate) fn append(&mut self, lines: Vec<StretchLine>) {
        for line in lines {
            match self.lines.iter().position(|known| known.id == line.id) {
                Some(index) => self.lines[index] = line,
                None => self.lines.push(line),
            }
        }
    }

    /// The lines in the daemon's order.
    pub(crate) fn lines(&self) -> &[StretchLine] {
        &self.lines
    }

    /// Opens or closes line `id`, returning its new state; an id the
    /// summary does not hold stays closed.
    pub(crate) fn toggle_open(&mut self, id: &str) -> bool {
        if !self.lines.iter().any(|line| line.id == id) {
            return false;
        }
        if self.open.remove(id) {
            return false;
        }
        self.open.insert(id.to_owned());
        true
    }

    /// Whether line `id` is open.
    pub(crate) fn is_open(&self, id: &str) -> bool {
        self.open.contains(id)
    }
}

/// The chat entries an open line shows, and whether entries before them
/// are no longer held.
#[derive(Debug)]
pub(crate) struct LineEntries<'a> {
    pub(crate) earlier_missing: bool,
    pub(crate) entries: Vec<&'a ChatEntry>,
}

/// The entries of `log` that `line` covers, in arrival order: from its
/// first entry to its last. A first entry no longer held starts the slice
/// at the oldest entry held, with `earlier_missing` set; a last entry not
/// held runs it to the newest; with neither held it is empty.
pub(crate) fn line_entries<'a>(log: &'a ChatLog, line: &StretchLine) -> LineEntries<'a> {
    let held: Vec<&ChatEntry> = log.entries().collect();
    let position = |id: &str| {
        held.iter()
            .position(|entry| chat::entry_id(entry) == Some(id))
    };
    let end = position(&line.to_entry);
    let Some(start) = position(&line.from_entry) else {
        let count = end.map_or(0, |end| end + 1);
        return LineEntries {
            earlier_missing: true,
            entries: held.into_iter().take(count).collect(),
        };
    };
    let count = end
        .filter(|end| *end >= start)
        .map_or(usize::MAX, |end| end - start + 1);
    LineEntries {
        earlier_missing: false,
        entries: held.into_iter().skip(start).take(count).collect(),
    }
}

/// What an open line shows under its text.
#[derive(Debug)]
pub(crate) enum OpenLine<'a> {
    /// The chat is still on its way, so the line's entries are not known.
    Loading,
    /// The line's entries, from [`line_entries`].
    Entries(LineEntries<'a>),
}

/// What `line`, open, shows: the loading note while the chat it reads is
/// `loading`, else its entries in `log`.
pub(crate) fn open_line<'a>(log: &'a ChatLog, loading: bool, line: &StretchLine) -> OpenLine<'a> {
    if loading {
        OpenLine::Loading
    } else {
        OpenLine::Entries(line_entries(log, line))
    }
}

/// The sessions whose chat and whose summary the panes want loaded.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct WantedLoads {
    pub(crate) chat: BTreeSet<String>,
    pub(crate) summary: BTreeSet<String>,
}

/// What the panes want loaded, from each pane's session and view: a chat or
/// a summary pane wants its session's chat (a summary pane draws the
/// pinned question, the composer and the open lines' entries from it), and
/// only a summary pane wants its summary.
pub(crate) fn wanted_loads<'a>(
    shown: impl IntoIterator<Item = (&'a str, PaneView)>,
) -> WantedLoads {
    let mut wanted = WantedLoads::default();
    for (session_id, view) in shown {
        match view {
            PaneView::Terminal => {}
            PaneView::Chat => {
                wanted.chat.insert(session_id.to_owned());
            }
            PaneView::Summary => {
                wanted.chat.insert(session_id.to_owned());
                wanted.summary.insert(session_id.to_owned());
            }
        }
    }
    wanted
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use protocol::{ChatEntry, StretchLine, SummarySource};

    use super::{OpenLine, SummaryLog, line_entries, open_line, wanted_loads};
    use crate::chat::{ChatLog, PaneView};

    fn line(id: &str, from: &str, to: &str, text: &str) -> StretchLine {
        StretchLine {
            id: id.to_owned(),
            from_entry: from.to_owned(),
            to_entry: to.to_owned(),
            started_at: None,
            ended_at: None,
            text: text.to_owned(),
            source: SummarySource::Template,
        }
    }

    fn user(id: &str) -> ChatEntry {
        ChatEntry::User {
            id: id.to_owned(),
            text: format!("message {id}"),
            pending: false,
        }
    }

    fn log_of(ids: &[&str]) -> ChatLog {
        let mut log = ChatLog::default();
        log.upsert(ids.iter().map(|id| user(id)).collect());
        log
    }

    fn line_ids(summary: &SummaryLog) -> Vec<&str> {
        summary
            .lines()
            .iter()
            .map(|line| line.id.as_str())
            .collect()
    }

    fn entry_ids(entries: &[&ChatEntry]) -> Vec<String> {
        entries
            .iter()
            .filter_map(|entry| crate::chat::entry_id(entry).map(ToOwned::to_owned))
            .collect()
    }

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn a_reply_to_the_pending_load_fills_the_lines_and_ends_the_loading() {
        let mut summary = SummaryLog::asked("r1".to_owned());
        assert!(summary.is_loading());
        assert!(summary.apply_summary(
            vec![line("u1", "u1", "a1", "Fix it (2m, 1 tool)")],
            Some("r1")
        ));
        assert!(!summary.is_loading());
        assert_eq!(line_ids(&summary), ["u1"]);
        assert_eq!(summary.lines()[0].text, "Fix it (2m, 1 tool)");
    }

    #[test]
    fn a_reply_to_an_older_load_is_dropped() {
        let mut summary = SummaryLog::asked("r2".to_owned());
        assert!(!summary.apply_summary(vec![line("u1", "u1", "a1", "old")], Some("r1")));
        assert!(summary.is_loading(), "the load is still awaited");
        assert_eq!(line_ids(&summary), Vec::<&str>::new());
    }

    #[test]
    fn an_unsolicited_summary_replaces_every_line_and_drops_gone_open_ids() {
        let mut summary = SummaryLog::asked("r1".to_owned());
        summary.apply_summary(
            vec![line("u1", "u1", "a1", "one"), line("u2", "u2", "a2", "two")],
            Some("r1"),
        );
        assert!(summary.toggle_open("u1"));
        assert!(summary.toggle_open("u2"));
        assert!(summary.apply_summary(
            vec![
                line("u2", "u2", "a3", "two again"),
                line("u3", "u3", "a4", "three")
            ],
            None
        ));
        assert_eq!(line_ids(&summary), ["u2", "u3"]);
        assert!(summary.is_open("u2"), "a surviving line stays open");
        assert!(
            !summary.is_open("u1"),
            "a line the reset dropped is forgotten"
        );
        summary.append(vec![line("u1", "u1", "a1", "back")]);
        assert!(
            !summary.is_open("u1"),
            "a line that comes back starts closed"
        );
    }

    #[test]
    fn an_append_replaces_a_known_line_in_place_and_adds_a_new_one() {
        let mut summary = SummaryLog::asked("r1".to_owned());
        summary.apply_summary(
            vec![line("u1", "u1", "a1", "one"), line("u2", "u2", "a2", "two")],
            Some("r1"),
        );
        summary.toggle_open("u1");
        summary.append(vec![
            line("u1", "u1", "a5", "one, longer"),
            line("u3", "u3", "a6", "three"),
        ]);
        assert_eq!(line_ids(&summary), ["u1", "u2", "u3"]);
        assert_eq!(summary.lines()[0].text, "one, longer");
        assert_eq!(summary.lines()[0].to_entry, "a5");
        assert!(
            summary.is_open("u1"),
            "a line keeps open when its text changes"
        );
    }

    #[test]
    fn a_line_opens_and_closes_and_an_unknown_id_stays_closed() {
        let mut summary = SummaryLog::asked("r1".to_owned());
        summary.apply_summary(vec![line("u1", "u1", "a1", "one")], Some("r1"));
        assert!(summary.toggle_open("u1"));
        assert!(summary.is_open("u1"));
        assert!(!summary.toggle_open("u1"));
        assert!(!summary.is_open("u1"));
        assert!(!summary.toggle_open("nope"));
        assert!(!summary.is_open("nope"));
    }

    #[test]
    fn an_open_line_takes_exactly_its_entries() {
        let log = log_of(&["u1", "t1", "a1", "e1", "u2", "a2"]);
        let slice = line_entries(&log, &line("u1", "u1", "e1", "one"));
        assert!(!slice.earlier_missing);
        assert_eq!(entry_ids(&slice.entries), ["u1", "t1", "a1", "e1"]);
        let slice = line_entries(&log, &line("u2", "u2", "a2", "two"));
        assert_eq!(entry_ids(&slice.entries), ["u2", "a2"]);
    }

    #[test]
    fn a_line_whose_start_is_gone_starts_at_the_oldest_entry_held_behind_the_note() {
        let log = log_of(&["t1", "a1", "e1", "u2"]);
        let slice = line_entries(&log, &line("u1", "u1", "e1", "one"));
        assert!(slice.earlier_missing);
        assert_eq!(entry_ids(&slice.entries), ["t1", "a1", "e1"]);
    }

    #[test]
    fn a_line_whose_end_is_not_held_runs_to_the_newest_entry() {
        let log = log_of(&["u1", "t1", "live:1"]);
        let slice = line_entries(&log, &line("u1", "u1", "a9", "one"));
        assert!(!slice.earlier_missing);
        assert_eq!(entry_ids(&slice.entries), ["u1", "t1", "live:1"]);
    }

    #[test]
    fn a_line_with_neither_end_held_shows_only_the_note() {
        let log = log_of(&["u5", "a5"]);
        let slice = line_entries(&log, &line("u1", "u1", "e1", "one"));
        assert!(slice.earlier_missing);
        assert_eq!(entry_ids(&slice.entries), Vec::<String>::new());
        let empty = ChatLog::default();
        let slice = line_entries(&empty, &line("u1", "u1", "e1", "one"));
        assert_eq!(entry_ids(&slice.entries), Vec::<String>::new());
    }

    #[test]
    fn a_line_opened_while_the_chat_loads_says_loading() {
        let one = line("u1", "u1", "e1", "one");
        let empty = ChatLog::default();
        assert!(matches!(open_line(&empty, true, &one), OpenLine::Loading));
        let log = log_of(&["u1", "e1"]);
        assert!(matches!(open_line(&log, true, &one), OpenLine::Loading));
        assert!(matches!(
            open_line(&log, false, &one),
            OpenLine::Entries(slice)
                if !slice.earlier_missing && entry_ids(&slice.entries) == ["u1", "e1"]
        ));
    }

    #[test]
    fn chat_and_summary_panes_want_the_chat_and_only_summary_panes_want_the_summary() {
        let wanted = wanted_loads([
            ("s1", PaneView::Chat),
            ("s2", PaneView::Summary),
            ("s3", PaneView::Terminal),
            ("s2", PaneView::Chat),
            ("s4", PaneView::Summary),
        ]);
        assert_eq!(wanted.chat, set(&["s1", "s2", "s4"]));
        assert_eq!(wanted.summary, set(&["s2", "s4"]));
        let none = wanted_loads([("s3", PaneView::Terminal)]);
        assert!(none.chat.is_empty() && none.summary.is_empty());
    }
}
