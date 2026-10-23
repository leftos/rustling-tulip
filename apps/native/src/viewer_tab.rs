//! The viewer tabs' state, without the UI: the `OpenViewerTab` requests
//! this client sent and the tab one of them waits to activate, and per tab
//! its path, where its file comes from, its load state, the line an open
//! asked for and its Markdown mode. The root view renders it.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use crate::diff_tab::{PENDING_ACTIVATE_TTL, split_path};
use crate::file_fetch::is_under;
use crate::remote_mode::LOCAL_FILES_REASON;

/// The toast an `OpenViewerTab` the daemon refused raises.
pub const OPEN_FAILED_TITLE: &str = "Couldn't open the file";

/// Where a viewer tab's file comes from. Not stored in the tab: it is
/// derived again whenever the tab is built, so a tab restored on a remote
/// host never reads a path of this PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerOrigin {
    /// A copy fetched from a remote host, under the download root.
    Fetched,
    /// A file of this PC, opened from a link while on the local daemon.
    Link,
    /// A file of this PC while connected to a remote host: never read.
    NotAvailable,
}

/// How far a viewer tab got with its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerLoad {
    /// Not read yet.
    Unread,
    /// Never read: the file is not available on this connection.
    NotAvailable,
}

/// How a Markdown file shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownMode {
    Rendered,
    Source,
}

/// The tab an `OpenViewerTab` answer activates, and the line it asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Activation {
    pub tab_id: String,
    pub line: Option<u32>,
}

/// The `OpenViewerTab`s out, and the tab one of them waits to activate.
#[derive(Debug, Default)]
pub(crate) struct ViewerOpens {
    next_open: u64,
    /// The `OpenViewerTab` ids the daemon has not answered, with the line
    /// each asked for.
    sent: HashMap<String, Option<u32>>,
    /// A tab the daemon named before its `TabUpdated` arrived, and when.
    pending: Option<(Activation, Instant)>,
}

impl ViewerOpens {
    /// A fresh `OpenViewerTab` id for an open at `line`, recorded as sent.
    pub(crate) fn open_id(&mut self, line: Option<u32>) -> String {
        self.next_open += 1;
        let id = format!("viewer-open-{}", self.next_open);
        self.sent.insert(id.clone(), line);
        id
    }

    /// `ViewerTabOpened`: the tab to activate now, when this client asked
    /// for it and the tab list already holds it (`known`). A tab the list
    /// does not hold yet waits for its `TabUpdated`; another client's open
    /// is ignored.
    pub(crate) fn opened(
        &mut self,
        id: &str,
        tab_id: &str,
        known: bool,
        now: Instant,
    ) -> Option<Activation> {
        let line = self.sent.remove(id)?;
        let activation = Activation {
            tab_id: tab_id.to_owned(),
            line,
        };
        if known {
            self.pending = None;
            return Some(activation);
        }
        self.pending = Some((activation, now));
        None
    }

    /// The waiting tab, once `known` says the tab list holds it; one that
    /// waited [`PENDING_ACTIVATE_TTL`] is dropped instead.
    pub(crate) fn take_ready(
        &mut self,
        known: impl Fn(&str) -> bool,
        now: Instant,
    ) -> Option<Activation> {
        let (activation, at) = self.pending.as_ref()?;
        if now.saturating_duration_since(*at) >= PENDING_ACTIVATE_TTL {
            self.pending = None;
            return None;
        }
        if !known(&activation.tab_id) {
            return None;
        }
        self.pending.take().map(|(activation, _)| activation)
    }

    /// Whether `id` is an `OpenViewerTab` of this client, which is
    /// forgotten.
    pub(crate) fn failed(&mut self, id: &str) -> bool {
        self.sent.remove(id).is_some()
    }

    /// A new connection: no answer to an old request will come.
    pub(crate) fn reset(&mut self) {
        self.sent.clear();
        self.pending = None;
    }
}

/// Where the file at `path` comes from: a fetched copy when it lies under
/// `download_root`, else a file of this PC, which only a client that opens
/// local files reads.
pub(crate) fn derive_origin(
    path: &Path,
    download_root: Option<&Path>,
    opens_local_files: bool,
) -> ViewerOrigin {
    if download_root.is_some_and(|root| is_under(root, path)) {
        ViewerOrigin::Fetched
    } else if opens_local_files {
        ViewerOrigin::Link
    } else {
        ViewerOrigin::NotAvailable
    }
}

/// `raw` as the daemon stores a viewer tab's path: trimmed, absolute, with
/// this OS's separators.
pub(crate) fn normalise_path(raw: &str) -> std::io::Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the path is empty",
        ));
    }
    let absolute = std::path::absolute(trimmed)?;
    absolute.into_os_string().into_string().map_err(|path| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("the path is not valid Unicode: {}", path.to_string_lossy()),
        )
    })
}

/// One viewer tab's state.
#[derive(Debug)]
pub struct ViewerTab {
    path: String,
    origin: ViewerOrigin,
    load: ViewerLoad,
    /// The line the last open asked for.
    line_target: Option<u32>,
    /// How a Markdown file shows; `None` for any other file.
    markdown: Option<MarkdownMode>,
}

impl ViewerTab {
    pub(crate) fn new(path: String, origin: ViewerOrigin) -> Self {
        let load = match origin {
            ViewerOrigin::Fetched | ViewerOrigin::Link => ViewerLoad::Unread,
            ViewerOrigin::NotAvailable => ViewerLoad::NotAvailable,
        };
        let markdown = is_markdown(&path).then_some(MarkdownMode::Rendered);
        Self {
            path,
            origin,
            load,
            line_target: None,
            markdown,
        }
    }

    /// The file's path on this PC.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The file's name: the path's last component.
    #[must_use]
    pub fn name(&self) -> &str {
        display_name(&self.path)
    }

    #[must_use]
    pub fn origin(&self) -> ViewerOrigin {
        self.origin
    }

    #[must_use]
    pub fn load(&self) -> ViewerLoad {
        self.load
    }

    /// The line the last open asked for.
    #[must_use]
    pub fn line_target(&self) -> Option<u32> {
        self.line_target
    }

    /// An open asked for `line`: a line moves the target, none leaves it.
    pub(crate) fn set_line_target(&mut self, line: Option<u32>) {
        if line.is_some() {
            self.line_target = line;
        }
    }

    /// How a Markdown file shows; `None` for any other file.
    #[must_use]
    pub fn markdown(&self) -> Option<MarkdownMode> {
        self.markdown
    }

    /// The line under the file name when the file is not read.
    #[must_use]
    pub fn note(&self) -> Option<&'static str> {
        match self.load {
            ViewerLoad::Unread => None,
            ViewerLoad::NotAvailable => Some(LOCAL_FILES_REASON),
        }
    }
}

/// What names a viewer tab of `path`: its last component, or the whole path
/// when that is empty (a path ending in a separator).
pub(crate) fn display_name(path: &str) -> &str {
    match split_path(path).0 {
        "" => path,
        name => name,
    }
}

/// Whether `path` names a Markdown file, by its extension.
fn is_markdown(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Instant;

    use super::*;
    use crate::remote_mode::LOCAL_FILES_REASON;

    const NOTES: &str = r"C:\notes\a.md";

    fn activation(tab_id: &str, line: Option<u32>) -> Activation {
        Activation {
            tab_id: tab_id.to_owned(),
            line,
        }
    }

    #[test]
    fn an_open_answered_before_its_tab_arrived_waits_for_it() {
        let mut opens = ViewerOpens::default();
        let now = Instant::now();
        let id = opens.open_id(Some(12));
        assert_eq!(id, "viewer-open-1");
        assert_eq!(opens.opened(&id, "v9", false, now), None);
        assert_eq!(
            opens.take_ready(|tab| tab == "t1", now),
            None,
            "v9 not listed yet"
        );
        assert_eq!(
            opens.take_ready(|tab| tab == "v9", now),
            Some(activation("v9", Some(12)))
        );
        assert_eq!(opens.take_ready(|_| true, now), None, "activated once");
        assert!(!opens.failed(&id), "the answered id is forgotten");
    }

    #[test]
    fn a_second_open_moves_the_target_line() {
        let mut opens = ViewerOpens::default();
        let now = Instant::now();
        let mut tab = ViewerTab::new(NOTES.to_owned(), ViewerOrigin::Link);
        assert_eq!(tab.line_target(), None);
        let first = opens.open_id(Some(10));
        let second = opens.open_id(Some(42));

        let opened = opens.opened(&first, "v1", true, now);
        assert_eq!(opened, Some(activation("v1", Some(10))));
        tab.set_line_target(opened.and_then(|open| open.line));
        assert_eq!(tab.line_target(), Some(10));

        let opened = opens.opened(&second, "v1", true, now);
        assert_eq!(opened, Some(activation("v1", Some(42))));
        tab.set_line_target(opened.and_then(|open| open.line));
        assert_eq!(tab.line_target(), Some(42), "the second open moves it");

        let third = opens.open_id(None);
        let opened = opens.opened(&third, "v1", true, now);
        assert_eq!(opened, Some(activation("v1", None)));
        tab.set_line_target(opened.and_then(|open| open.line));
        assert_eq!(
            tab.line_target(),
            Some(42),
            "an open without a line leaves the target"
        );
    }

    #[test]
    fn origin_is_derived_fetched_under_the_download_root() {
        let root = Path::new(r"C:\data\remote-files");
        let fetched = root.join("box").join("r1").join("a.md");
        assert_eq!(
            derive_origin(&fetched, Some(root), false),
            ViewerOrigin::Fetched,
            "a fetched copy reads on a remote host"
        );
        assert_eq!(
            derive_origin(&fetched, Some(root), true),
            ViewerOrigin::Fetched
        );
        assert_eq!(
            derive_origin(root, Some(root), true),
            ViewerOrigin::Link,
            "the root itself is no fetched copy"
        );
        let escape = root.join("..").join("x.md");
        assert_eq!(
            derive_origin(&escape, Some(root), false),
            ViewerOrigin::NotAvailable,
            "a path climbing out of the root is not under it"
        );
    }

    #[test]
    fn origin_is_link_on_local_and_unavailable_on_a_remote_host() {
        let path = Path::new(NOTES);
        let root = Path::new(r"C:\data\remote-files");
        assert_eq!(derive_origin(path, Some(root), true), ViewerOrigin::Link);
        assert_eq!(
            derive_origin(path, None, true),
            ViewerOrigin::Link,
            "no download root"
        );
        assert_eq!(
            derive_origin(path, Some(root), false),
            ViewerOrigin::NotAvailable
        );

        let local = ViewerTab::new(NOTES.to_owned(), ViewerOrigin::Link);
        assert_eq!(local.load(), ViewerLoad::Unread);
        assert_eq!(local.note(), None);
        assert_eq!(local.name(), "a.md");
        let remote = ViewerTab::new(NOTES.to_owned(), ViewerOrigin::NotAvailable);
        assert_eq!(remote.load(), ViewerLoad::NotAvailable, "it reads nothing");
        assert_eq!(remote.note(), Some(LOCAL_FILES_REASON));
        assert_eq!(remote.name(), "a.md");
    }

    #[test]
    fn a_path_ending_in_a_separator_is_named_by_the_whole_path() {
        let tab = ViewerTab::new(r"C:\notes\".to_owned(), ViewerOrigin::Link);
        assert_eq!(tab.name(), r"C:\notes\");
        assert_eq!(display_name("C:/notes/"), "C:/notes/");
        assert_eq!(display_name(NOTES), "a.md");
    }
}
