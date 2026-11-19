//! The phone's host browser, as plain state: the host and its OS, the folder
//! shown and the entries its listing sent so far, the requests out to the
//! host, whether hidden entries show and why the folder could not be listed.
//! [`Inbox`] is the root view's hand-off: it keeps the host's answers for the
//! watched host until the phone takes them, and the host info each
//! connection gave.

use std::fmt::Display;

use chrono::{DateTime, Datelike as _, TimeZone};
use protocol::{ClientMessage, DirEntry, HostOsKind, PathKind};

use crate::fetch_link::HostPath;
use crate::target::HostKey;

/// How many entries one page of a listing asks for.
pub(crate) const PAGE_SIZE: u32 = 200;

/// How close to the last entry loaded the list is drawn before the next page
/// is asked for, in rows.
pub(crate) const NEXT_PAGE_WITHIN: usize = 20;

/// What a host's `HostInfo` said about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostFacts {
    pub(crate) os: HostOsKind,
    pub(crate) roots: Vec<String>,
}

/// An answer the host sent the browser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    Info(HostFacts),
    Listing {
        entries: Vec<DirEntry>,
        next_cursor: Option<String>,
        error: Option<String>,
    },
}

/// What came for the watched host since the last drain, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// The host answered the request `id`.
    Answer {
        host: HostKey,
        id: String,
        answer: Answer,
    },
    /// The host said `Welcome` again: no request out to it will be
    /// answered.
    Reset(HostKey),
}

/// How a folder fetch on the phone ended, as the root view hands it on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FolderEnd {
    /// The folder asked for as `asked` on `host` is saved as `saved`, whose
    /// size is the folder's bytes in all.
    Saved {
        host: HostKey,
        asked: String,
        saved: crate::PhoneFetched,
    },
    /// The folder asked for as `asked` on `host` could not be fetched, for
    /// `reason`.
    Failed {
        host: HostKey,
        asked: String,
        reason: String,
    },
}

/// The root view's hand-off for the browser.
#[derive(Debug, Default)]
pub(crate) struct Inbox {
    watched: Option<HostKey>,
    facts: Vec<(HostKey, HostFacts)>,
    came: Vec<Event>,
    /// The phone's folder fetches that ended since the last take, whether
    /// or not a browser is open: a saved folder is kept either way.
    folders: Vec<FolderEnd>,
}

impl Inbox {
    /// Keeps how a folder fetch on the phone ended, for the phone.
    pub(crate) fn folder_ended(&mut self, end: FolderEnd) {
        self.folders.push(end);
    }

    /// The folder fetches that ended since the last take.
    pub(crate) fn take_folders(&mut self) -> Vec<FolderEnd> {
        std::mem::take(&mut self.folders)
    }

    /// Keeps the answers of `host` from now on, none while `None`.
    pub(crate) fn watch(&mut self, host: Option<HostKey>) {
        if host.is_none() {
            self.came.clear();
        }
        self.watched = host;
    }

    /// Keeps `host`'s answer to `id`; `false` when `host` is not watched.
    pub(crate) fn answer(&mut self, host: &HostKey, id: &str, answer: Answer) -> bool {
        if self.watched.as_ref() != Some(host) {
            return false;
        }
        self.came.push(Event::Answer {
            host: host.clone(),
            id: id.to_owned(),
            answer,
        });
        true
    }

    /// `host` said `Welcome` again: its info is forgotten, and a watched
    /// host's reset is kept for the phone. Returns whether that changed
    /// anything.
    pub(crate) fn host_reset(&mut self, host: &HostKey) -> bool {
        let before = self.facts.len();
        self.facts.retain(|(known, _)| known != host);
        let forgot = self.facts.len() != before;
        if self.watched.as_ref() == Some(host) {
            self.came.push(Event::Reset(host.clone()));
            return true;
        }
        forgot
    }

    /// Keeps what `host` said about itself until its next `Welcome`.
    pub(crate) fn remember(&mut self, host: &HostKey, facts: HostFacts) {
        self.facts.retain(|(known, _)| known != host);
        self.facts.push((host.clone(), facts));
    }

    /// What `host` said about itself on this connection, if it did.
    pub(crate) fn facts(&self, host: &HostKey) -> Option<&HostFacts> {
        self.facts
            .iter()
            .find(|(known, _)| known == host)
            .map(|(_, facts)| facts)
    }

    /// What came since the last drain.
    pub(crate) fn drain(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.came)
    }
}

/// What [`Browser::take`] made of an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Taken {
    /// Not an answer the browser waits for.
    Dropped,
    /// The host's info: the folder can be listed now.
    Info(HostFacts),
    /// A page of the folder, or why it could not be listed.
    Listing,
}

/// The browser's state.
#[derive(Clone, Debug)]
pub(crate) struct Browser {
    host: HostKey,
    facts: Option<HostFacts>,
    path: String,
    entries: Vec<DirEntry>,
    next_cursor: Option<String>,
    info_id: Option<String>,
    list_id: Option<String>,
    paged: Option<String>,
    show_hidden: bool,
    error: Option<String>,
    fetch: Option<FolderFetch>,
}

/// Where a folder fetch the browser started has got.
#[derive(Clone, Debug, PartialEq, Eq)]
enum FetchState {
    Running,
    Done(u64),
    Failed(String),
}

/// The folder fetch the browser's result line is about.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FolderFetch {
    /// The host path the fetch asked for.
    host_path: String,
    /// The folder's name, as the line says it.
    name: String,
    state: FetchState,
}

impl Browser {
    /// The browser on `host`'s folder `path`, nothing asked yet.
    pub(crate) fn new(host: HostKey, path: String) -> Self {
        Self {
            host,
            facts: None,
            path,
            entries: Vec::new(),
            next_cursor: None,
            info_id: None,
            list_id: None,
            paged: None,
            show_hidden: false,
            error: None,
            fetch: None,
        }
    }

    /// The fetch of the folder `name` at `host_path` started: the result
    /// line says so, in place of any line before it.
    pub(crate) fn fetch_started(&mut self, host_path: String, name: String) {
        self.fetch = Some(FolderFetch {
            host_path,
            name,
            state: FetchState::Running,
        });
    }

    /// The fetch of `host_path` is saved, `size` bytes in all; nothing
    /// changes unless the line is about that fetch.
    pub(crate) fn fetch_done(&mut self, host_path: &str, size: u64) {
        self.fetch_ended(host_path, FetchState::Done(size));
    }

    /// The fetch of `host_path` failed or was refused, for `reason`;
    /// nothing changes unless the line is about that fetch.
    pub(crate) fn fetch_failed(&mut self, host_path: &str, reason: String) {
        self.fetch_ended(host_path, FetchState::Failed(reason));
    }

    fn fetch_ended(&mut self, host_path: &str, state: FetchState) {
        if let Some(fetch) = self
            .fetch
            .as_mut()
            .filter(|fetch| fetch.host_path == host_path)
        {
            fetch.state = state;
        }
    }

    /// The result line under the header, while the browser shows one.
    pub(crate) fn fetch_line(&self) -> Option<String> {
        let fetch = self.fetch.as_ref()?;
        let name = &fetch.name;
        Some(match &fetch.state {
            FetchState::Running => format!("Fetching {name}…"),
            FetchState::Done(size) => {
                format!("Fetched {name} · {}", crate::spawn_form::human_size(*size))
            }
            FetchState::Failed(reason) => format!("Couldn't fetch {name}: {reason}"),
        })
    }

    /// Whether the result line tells of a failure.
    pub(crate) fn fetch_failed_line(&self) -> bool {
        self.fetch
            .as_ref()
            .is_some_and(|fetch| matches!(fetch.state, FetchState::Failed(_)))
    }

    pub(crate) fn host(&self) -> &HostKey {
        &self.host
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    /// Whether a request is out that the browser waits for.
    #[cfg(test)]
    fn pending(&self) -> bool {
        self.info_id.is_some() || self.list_id.is_some()
    }

    /// Asks for the host's info under `id`.
    pub(crate) fn ask_info(&mut self, id: String) -> ClientMessage {
        self.info_id = Some(id.clone());
        ClientMessage::GetHostInfo { id }
    }

    /// The host's info, from this connection's cache.
    pub(crate) fn set_facts(&mut self, facts: HostFacts) {
        self.facts = Some(facts);
    }

    /// Lists `path` from its first page under `id`, in place of the folder
    /// shown; another folder than the one shown drops the result line.
    pub(crate) fn list(&mut self, path: String, id: String) -> ClientMessage {
        if path != self.path {
            // The result line belongs to the folder it was started in.
            self.fetch = None;
        }
        self.path = path;
        self.entries.clear();
        self.next_cursor = None;
        self.paged = None;
        self.error = None;
        self.list_id = Some(id.clone());
        ClientMessage::ListDir {
            id,
            path: self.path.clone(),
            cursor: None,
            limit: Some(PAGE_SIZE),
        }
    }

    /// Asks for the next page under `id` when the list is drawn to its row
    /// `drawn`; `None` when it is not near the end, the listing ended, a
    /// request is out, or this cursor was asked for already.
    pub(crate) fn next_page(&mut self, drawn: usize, id: String) -> Option<ClientMessage> {
        if self.facts.is_none() || self.error.is_some() {
            return None;
        }
        let cursor = self.next_cursor.clone()?;
        let near_end = drawn + NEXT_PAGE_WITHIN >= self.rows().len();
        if !near_end || self.list_id.is_some() || self.paged.as_ref() == Some(&cursor) {
            return None;
        }
        self.paged = Some(cursor.clone());
        self.list_id = Some(id.clone());
        Some(ClientMessage::ListDir {
            id,
            path: self.path.clone(),
            cursor: Some(cursor),
            limit: Some(PAGE_SIZE),
        })
    }

    /// Takes the answer to `id`: an answer to a request no longer out (the
    /// folder left, the host started over, the id answered already) is
    /// dropped.
    pub(crate) fn take(&mut self, id: &str, answer: Answer) -> Taken {
        match answer {
            Answer::Info(facts) => {
                if self.info_id.as_deref() != Some(id) {
                    return Taken::Dropped;
                }
                self.info_id = None;
                self.facts = Some(facts.clone());
                Taken::Info(facts)
            }
            Answer::Listing {
                entries,
                next_cursor,
                error,
            } => {
                if self.list_id.as_deref() != Some(id) {
                    return Taken::Dropped;
                }
                self.list_id = None;
                if let Some(error) = error {
                    self.entries.clear();
                    self.next_cursor = None;
                    self.error = Some(error);
                    return Taken::Listing;
                }
                self.entries.extend(entries);
                // A stable sort: each group keeps the host's name order.
                self.entries.sort_by_key(|entry| !is_folder(entry));
                self.next_cursor = next_cursor;
                Taken::Listing
            }
        }
    }

    /// The host started over: every request out is dropped, and with them
    /// the host's info, which is asked for again.
    pub(crate) fn reset(&mut self) {
        self.info_id = None;
        self.list_id = None;
        self.paged = None;
        self.facts = None;
        // A cursor belongs to the listing it came with, on the connection
        // that sent it.
        self.next_cursor = None;
    }

    /// Says the folder cannot be listed, for `error`.
    pub(crate) fn fail(&mut self, error: String) {
        self.entries.clear();
        self.next_cursor = None;
        self.error = Some(error);
    }

    /// Drops the error line, as the host can be asked again.
    pub(crate) fn clear_error(&mut self) {
        self.error = None;
    }

    /// Shows or hides the hidden entries.
    pub(crate) fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
    }

    /// The entries the list shows, folders first.
    pub(crate) fn rows(&self) -> Vec<&DirEntry> {
        self.entries
            .iter()
            .filter(|entry| self.show_hidden || !entry.hidden)
            .collect()
    }

    /// The line drawn in the rows' place when the folder could not be
    /// listed.
    pub(crate) fn error_line(&self) -> Option<String> {
        self.error
            .as_ref()
            .map(|error| format!("Can't list {}: {error}", self.path))
    }

    /// The folder holding the one shown; `None` at a root, one the host
    /// listed among its roots included.
    pub(crate) fn parent(&self) -> Option<String> {
        let facts = self.facts.as_ref()?;
        let windows = facts.os == HostOsKind::Windows;
        let at_root = facts.roots.iter().any(|root| {
            if windows {
                root.eq_ignore_ascii_case(&self.path)
            } else {
                *root == self.path
            }
        });
        if at_root {
            return None;
        }
        parent_of(Some(facts.os), &self.path)
    }

    /// The path of the entry `name` in the folder shown.
    pub(crate) fn child(&self, name: &str) -> String {
        child_of(self.facts.as_ref().map(|facts| facts.os), &self.path, name)
    }
}

/// When an entry last changed, `ms` after the Unix epoch, as its row says
/// it in `now`'s time zone: the time on `now`'s day, the day in `now`'s
/// year, else the day and the year.
pub(crate) fn short_date<Tz: TimeZone>(ms: i64, now: &DateTime<Tz>) -> Option<String>
where
    Tz::Offset: Display,
{
    let local = DateTime::from_timestamp_millis(ms)?.with_timezone(&now.timezone());
    let form = if local.date_naive() == now.date_naive() {
        "%H:%M"
    } else if local.year() == now.year() {
        "%b %-d"
    } else {
        "%b %-d, %Y"
    };
    Some(local.format(form).to_string())
}

/// Whether `entry` opens as a folder: a folder, or a link the host reports
/// as one.
pub(crate) fn is_folder(entry: &DirEntry) -> bool {
    entry.kind == PathKind::Dir
}

/// The folder holding `path` on a host running `os`; `None` at a root: a
/// drive, a network share or `/`.
pub(crate) fn parent_of(os: Option<HostOsKind>, path: &str) -> Option<String> {
    let mut parsed = host_path(os, path)?;
    // A network path's first two segments are its server and share.
    if parsed.anchor == "//" && parsed.segments.len() <= 2 {
        return None;
    }
    parsed.segments.pop()?;
    Some(parsed.render())
}

/// The path of `name` in the folder `path` on a host running `os`, joined
/// with that host's separator.
pub(crate) fn child_of(os: Option<HostOsKind>, path: &str, name: &str) -> String {
    if let Some(mut parsed) = host_path(os, path) {
        parsed.segments.push(name.to_owned());
        parsed.render()
    } else {
        let separator = if is_windows(os, path) { '\\' } else { '/' };
        format!("{}{separator}{name}", path.trim_end_matches(separator))
    }
}

/// Whether `path` is spelled as a Windows host spells it: the host's OS
/// says, and with no OS known a path not starting with `/` is.
fn is_windows(os: Option<HostOsKind>, path: &str) -> bool {
    match os {
        Some(HostOsKind::Windows) => true,
        Some(HostOsKind::Linux | HostOsKind::MacOs) => false,
        Some(HostOsKind::Unknown) | None => !path.starts_with('/'),
    }
}

/// `path` as an absolute host path on a host running `os`: a Windows path
/// reads `\` and `/` as separators, a Unix one `/` alone, so a `\` stays in
/// its name. `None` for a relative path or one that climbs above its root.
fn host_path(os: Option<HostOsKind>, path: &str) -> Option<HostPath> {
    if is_windows(os, path) {
        return HostPath::parse(path);
    }
    let rest = path.strip_prefix('/')?;
    let mut segments = Vec::new();
    for segment in rest.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            name => segments.push(name.to_owned()),
        }
    }
    Some(HostPath {
        anchor: "/".to_owned(),
        windows: false,
        segments,
    })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    fn host() -> HostKey {
        HostKey::Local
    }

    fn entry(name: &str, kind: PathKind) -> DirEntry {
        DirEntry {
            name: name.to_owned(),
            kind,
            size: 10,
            modified: Some(1_700_000_000_000),
            link: false,
            hidden: name.starts_with('.'),
        }
    }

    fn page(entries: Vec<DirEntry>, next_cursor: Option<&str>) -> Answer {
        Answer::Listing {
            entries,
            next_cursor: next_cursor.map(str::to_owned),
            error: None,
        }
    }

    fn linux() -> HostFacts {
        HostFacts {
            os: HostOsKind::Linux,
            roots: vec!["/".to_owned()],
        }
    }

    /// A browser on `/home/a` whose host is Linux and whose first listing
    /// is out under `l1`.
    fn listing() -> Browser {
        let mut browser = Browser::new(host(), "/home/a".to_owned());
        browser.set_facts(linux());
        browser.list("/home/a".to_owned(), "l1".to_owned());
        browser
    }

    fn names(browser: &Browser) -> Vec<&str> {
        browser
            .rows()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect()
    }

    #[test]
    fn a_windows_parent_drops_the_last_folder_and_a_drive_has_none() {
        let os = Some(HostOsKind::Windows);
        assert_eq!(parent_of(os, r"C:\Users\a").as_deref(), Some(r"C:\Users"));
        assert_eq!(parent_of(os, r"C:\Users").as_deref(), Some(r"C:\"));
        assert_eq!(parent_of(os, r"C:\"), None);
    }

    #[test]
    fn a_network_share_is_a_root() {
        let os = Some(HostOsKind::Windows);
        assert_eq!(
            parent_of(os, r"\\server\share\x").as_deref(),
            Some(r"\\server\share")
        );
        assert_eq!(parent_of(os, r"\\server\share"), None);
    }

    #[test]
    fn a_unix_parent_drops_the_last_folder_and_the_root_has_none() {
        let os = Some(HostOsKind::Linux);
        assert_eq!(parent_of(os, "/home/a").as_deref(), Some("/home"));
        assert_eq!(parent_of(os, "/home").as_deref(), Some("/"));
        assert_eq!(parent_of(os, "/"), None);
        assert_eq!(
            parent_of(Some(HostOsKind::MacOs), "/Users/a").as_deref(),
            Some("/Users")
        );
    }

    #[test]
    fn a_child_joins_with_the_hosts_separator() {
        let windows = Some(HostOsKind::Windows);
        assert_eq!(child_of(windows, r"C:\Users", "a"), r"C:\Users\a");
        assert_eq!(child_of(windows, r"C:\", "Users"), r"C:\Users");
        assert_eq!(
            child_of(windows, r"\\server\share", "x"),
            r"\\server\share\x"
        );
        let linux = Some(HostOsKind::Linux);
        assert_eq!(child_of(linux, "/home", "a"), "/home/a");
        assert_eq!(child_of(linux, "/", "etc"), "/etc");
        assert_eq!(
            child_of(linux, "/home", r"a\b"),
            r"/home/a\b",
            "a backslash is part of a Unix name"
        );
    }

    #[test]
    fn hidden_entries_are_left_out_until_show_hidden() {
        let mut browser = listing();
        let taken = browser.take(
            "l1",
            page(
                vec![entry(".git", PathKind::Dir), entry("a.txt", PathKind::File)],
                None,
            ),
        );
        assert_eq!(taken, Taken::Listing);
        assert_eq!(names(&browser), ["a.txt"]);
        browser.toggle_hidden();
        assert_eq!(names(&browser), [".git", "a.txt"]);
    }

    #[test]
    fn a_second_page_appends_and_keeps_folders_first() {
        let mut browser = listing();
        browser.take(
            "l1",
            page(
                vec![entry("a", PathKind::Dir), entry("b", PathKind::Dir)],
                Some("c2"),
            ),
        );
        let ask = browser
            .next_page(1, "l2".to_owned())
            .expect("the end of the list asks for the next page");
        assert!(
            matches!(
                &ask,
                ClientMessage::ListDir { id, path, cursor, limit }
                    if id == "l2"
                        && path == "/home/a"
                        && cursor.as_deref() == Some("c2")
                        && *limit == Some(PAGE_SIZE)
            ),
            "{ask:?}"
        );
        assert!(
            browser.next_page(1, "l3".to_owned()).is_none(),
            "one request per cursor"
        );
        browser.take(
            "l2",
            page(
                vec![entry("z.txt", PathKind::File), entry("c", PathKind::Dir)],
                None,
            ),
        );
        assert_eq!(names(&browser), ["a", "b", "c", "z.txt"]);
        assert!(
            browser.next_page(3, "l4".to_owned()).is_none(),
            "the listing ended"
        );
    }

    #[test]
    fn the_next_page_waits_until_the_list_nears_its_end() {
        let mut browser = listing();
        let many = (0..50)
            .map(|n| entry(&format!("f{n:02}"), PathKind::File))
            .collect();
        browser.take("l1", page(many, Some("c2")));
        assert!(browser.next_page(10, "l2".to_owned()).is_none());
        assert!(browser.next_page(30, "l2".to_owned()).is_some());
    }

    #[test]
    fn an_answer_with_a_stale_id_is_dropped() {
        let mut browser = listing();
        browser.list("/home".to_owned(), "l2".to_owned());
        let taken = browser.take("l1", page(vec![entry("old", PathKind::File)], None));
        assert_eq!(taken, Taken::Dropped);
        assert_eq!(
            names(&browser),
            [] as [&str; 0],
            "the old folder's page is not shown"
        );
        assert_eq!(
            browser.take("l2", page(vec![entry("new", PathKind::File)], None)),
            Taken::Listing
        );
        assert_eq!(
            browser.take("l2", page(vec![entry("again", PathKind::File)], None)),
            Taken::Dropped,
            "an id is answered once"
        );
        assert_eq!(names(&browser), ["new"]);
    }

    #[test]
    fn the_host_info_answers_its_own_id_only() {
        let mut browser = Browser::new(host(), "/home/a".to_owned());
        browser.ask_info("i1".to_owned());
        assert_eq!(browser.take("i0", Answer::Info(linux())), Taken::Dropped);
        assert_eq!(
            browser.take("i1", Answer::Info(linux())),
            Taken::Info(linux())
        );
        assert_eq!(browser.parent().as_deref(), Some("/home"));
    }

    #[test]
    fn a_listing_error_sets_the_error_line() {
        let mut browser = listing();
        browser.take(
            "l1",
            Answer::Listing {
                entries: Vec::new(),
                next_cursor: None,
                error: Some("permission denied".to_owned()),
            },
        );
        assert_eq!(
            browser.error_line().as_deref(),
            Some("Can't list /home/a: permission denied")
        );
        assert_eq!(names(&browser), [] as [&str; 0]);
    }

    #[test]
    fn a_reset_clears_the_pending_ids() {
        let mut browser = listing();
        browser.ask_info("i1".to_owned());
        assert!(browser.pending());
        browser.reset();
        assert!(!browser.pending());
        assert_eq!(
            browser.take("l1", page(vec![entry("late", PathKind::File)], None)),
            Taken::Dropped
        );
        assert_eq!(browser.take("i1", Answer::Info(linux())), Taken::Dropped);
    }

    #[test]
    fn a_reset_drops_the_old_cursor() {
        let mut browser = listing();
        browser.take("l1", page(vec![entry("a", PathKind::File)], Some("c2")));
        browser.reset();
        browser.set_facts(linux());
        assert!(
            browser.next_page(0, "l2".to_owned()).is_none(),
            "the old connection's cursor is not sent"
        );
        browser.list("/home/a".to_owned(), "l3".to_owned());
        browser.take("l3", page(vec![entry("a", PathKind::File)], Some("c3")));
        let ask = browser.next_page(0, "l4".to_owned());
        assert!(
            matches!(&ask, Some(ClientMessage::ListDir { cursor, .. }) if cursor.as_deref() == Some("c3")),
            "the new listing's cursor is: {ask:?}"
        );
    }

    #[test]
    fn no_next_page_while_facts_are_unknown() {
        let mut browser = Browser::new(host(), "/home/a".to_owned());
        browser.list("/home/a".to_owned(), "l1".to_owned());
        browser.take("l1", page(vec![entry("a", PathKind::File)], Some("c2")));
        assert!(browser.next_page(0, "l2".to_owned()).is_none());
    }

    #[test]
    fn no_next_page_after_fail() {
        let mut browser = listing();
        browser.take("l1", page(vec![entry("a", PathKind::File)], Some("c2")));
        browser.fail("this host does not list folders".to_owned());
        assert!(browser.next_page(0, "l2".to_owned()).is_none());
    }

    #[test]
    fn the_result_line_says_fetching_then_fetched_with_the_size() {
        let mut browser = listing();
        assert_eq!(browser.fetch_line(), None, "no line before a fetch");

        browser.fetch_started("/home/a/docs".to_owned(), "docs".to_owned());
        assert_eq!(browser.fetch_line().as_deref(), Some("Fetching docs…"));

        browser.fetch_done("/home/a/docs", 2048);
        assert_eq!(
            browser.fetch_line(),
            Some(format!(
                "Fetched docs · {}",
                crate::spawn_form::human_size(2048)
            ))
        );
    }

    #[test]
    fn the_result_line_says_why_a_fetch_failed() {
        let mut browser = listing();
        browser.fetch_started("/home/a/docs".to_owned(), "docs".to_owned());

        browser.fetch_failed("/home/a/docs", "permission denied".to_owned());

        assert_eq!(
            browser.fetch_line().as_deref(),
            Some("Couldn't fetch docs: permission denied")
        );
    }

    #[test]
    fn the_result_line_ignores_another_fetchs_end() {
        let mut browser = listing();
        browser.fetch_started("/home/a/docs".to_owned(), "docs".to_owned());

        browser.fetch_done("/home/a/other", 10);
        browser.fetch_failed("/home/a/other", "gone".to_owned());

        assert_eq!(browser.fetch_line().as_deref(), Some("Fetching docs…"));
    }

    #[test]
    fn the_result_line_clears_when_the_browser_moves_to_another_folder() {
        let mut browser = listing();
        browser.fetch_started("/home/a/docs".to_owned(), "docs".to_owned());
        browser.list("/home/a".to_owned(), "l2".to_owned());
        assert_eq!(
            browser.fetch_line().as_deref(),
            Some("Fetching docs…"),
            "listing the same folder again keeps it"
        );

        browser.list("/home/a/docs".to_owned(), "l3".to_owned());

        assert_eq!(browser.fetch_line(), None, "another folder clears it");
        browser.fetch_done("/home/a/docs", 2048);
        assert_eq!(
            browser.fetch_line(),
            None,
            "the cleared fetch's end does not bring it back"
        );
    }

    #[test]
    fn the_inbox_keeps_the_watched_hosts_answers_and_resets() {
        let mut inbox = Inbox::default();
        assert!(!inbox.answer(&host(), "l1", page(Vec::new(), None)));
        inbox.watch(Some(host()));
        inbox.remember(&host(), linux());
        assert!(inbox.answer(&host(), "l1", page(Vec::new(), None)));
        assert!(inbox.host_reset(&host()));
        assert_eq!(
            inbox.facts(&host()),
            None,
            "a Welcome forgets the host's info"
        );
        assert_eq!(
            inbox.drain(),
            [
                Event::Answer {
                    host: host(),
                    id: "l1".to_owned(),
                    answer: page(Vec::new(), None),
                },
                Event::Reset(host()),
            ]
        );
        assert_eq!(inbox.drain(), []);
    }
}
