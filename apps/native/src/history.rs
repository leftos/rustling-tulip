//! The source-control History: each section's commit list, read 50 commits
//! at a time, the commit selected in it and the details fetched for it, the
//! `origin` remote each repo's forge button opens, and the split heights the
//! panel keeps.
//!
//! Plain Rust, so every rule is unit-tested; the view renders
//! [`HistoryModel::block`] and sends the requests these methods return.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use protocol::{ClientMessage, DaemonMessage, GitCommit, GitCommitDetail, GitRemoteUrl};

use crate::new_request_id;
use crate::source_control::{ScKey, ScUiState, Section};

/// How many commits one request reads.
pub const PAGE_SIZE: u32 = 50;
/// The changes area's height before the user drags the split.
pub const DEFAULT_CHANGES_HEIGHT: f32 = 420.0;
/// The least the changes area and the History area each keep.
pub const MIN_CHANGES_SIDE: f32 = 140.0;
/// A commit list's height above its detail pane before the user drags it.
pub const DEFAULT_LIST_HEIGHT: f32 = 220.0;
/// The least a commit list and its detail pane each keep.
pub const MIN_LIST_SIDE: f32 = 90.0;

/// The body or row text while a read is out.
pub const LOADING: &str = "loading…";
/// The body of a branch with no commits.
pub const NO_COMMITS: &str = "no commits";
/// The row after the commits while more may follow.
pub const LOAD_MORE: &str = "load more";
/// The detail's file list of a commit that changed no file.
pub const NO_CHANGES: &str = "no file changes";
/// The detail's file list of a merge commit that changed no file.
pub const NO_MERGE_CHANGES: &str = "no file changes (merge commit)";
/// The forge button's tooltip while the remote is being read.
pub const LOOKING_UP_ORIGIN: &str = "Looking up origin…";
/// The forge button's tooltip when `origin` is on no forge it knows.
pub const NOT_A_FORGE: &str = "origin isn't on GitHub, GitLab or Bitbucket";
/// The forge button's tooltip when the repo has no `origin`.
pub const NO_ORIGIN: &str = "This repository has no origin remote";

/// The prefixes the daemon puts on a failed read's message, which the panel
/// drops since its own text already says what failed.
const DAEMON_PREFIXES: [&str; 2] = ["commit list failed: ", "commit detail failed: "];

/// What [`HistoryModel::apply`] did with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Not a History message, or a reply nothing waits for any more.
    Ignored,
    /// A reply the History shows.
    Changed,
    /// An `Error` answering one of the History's reads: it is shown in the
    /// panel, and nothing else may take it.
    Consumed,
}

/// A read out and not yet answered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    request_id: String,
    offset: u32,
}

/// One section's commits.
#[derive(Debug, Default)]
struct CommitList {
    commits: Vec<GitCommit>,
    /// The first page has arrived.
    loaded: bool,
    /// The last page held fewer than [`PAGE_SIZE`] commits.
    exhausted: bool,
    pending: Option<Pending>,
    /// The offset of the read that failed last, and why.
    error: Option<(u32, String)>,
    selected: Option<String>,
}

impl CommitList {
    /// Marks a read of the page at `offset` as out and returns it.
    fn request(&mut self, key: &ScKey, offset: u32) -> ClientMessage {
        let request_id = new_request_id();
        self.pending = Some(Pending {
            request_id: request_id.clone(),
            offset,
        });
        ClientMessage::ListCommits {
            repo_id: key.repo_id.clone(),
            branch: None,
            limit: PAGE_SIZE,
            offset,
            worktree_path: key.worktree.clone(),
            request_id: Some(request_id),
        }
    }

    /// The loaded commit count as the wire's offset.
    fn next_offset(&self) -> u32 {
        u32::try_from(self.commits.len()).unwrap_or(u32::MAX)
    }
}

/// A commit's detail, per (repo, sha).
#[derive(Debug, Clone)]
enum Detail {
    Loading(String),
    Loaded(Box<GitCommitDetail>),
    Failed(String),
}

/// A repo's `origin`, as its forge button reads it.
#[derive(Debug, Clone)]
enum Remote {
    Loading(String),
    Known(GitRemoteUrl),
    NoOrigin,
}

/// Every section's History state and the reads out for it.
#[derive(Debug, Default)]
pub struct HistoryModel {
    lists: HashMap<ScKey, CommitList>,
    details: HashMap<(String, String), Detail>,
    remotes: HashMap<String, Remote>,
    /// Commit-list reads whose answer no longer matters, by request id, with
    /// the key and offset their page would carry: their errors are taken
    /// silently, so a collapsed list's late failure raises no toast, and
    /// their late pages retire them.
    stale: HashMap<String, StaleRead>,
}

/// The key and offset of a commit-list read whose answer no longer matters.
type StaleRead = (ScKey, u32);

impl HistoryModel {
    /// Folds a daemon message in.
    pub fn apply(&mut self, msg: &DaemonMessage) -> Applied {
        match msg {
            DaemonMessage::Welcome { .. } => {
                let had =
                    !self.lists.is_empty() || !self.details.is_empty() || !self.remotes.is_empty();
                self.lists.clear();
                self.details.clear();
                self.remotes.clear();
                self.stale.clear();
                changed(had)
            }
            DaemonMessage::Commits {
                repo_id,
                commits,
                offset,
                worktree_path,
            } => {
                let key = ScKey {
                    repo_id: repo_id.clone(),
                    worktree: worktree_path.clone(),
                };
                let taken = self.take_page(&key, *offset, commits);
                if !taken {
                    self.retire_stale(&(key, *offset));
                }
                changed(taken)
            }
            DaemonMessage::CommitDetail { repo_id, detail } => {
                let entry = (repo_id.clone(), detail.commit.sha.clone());
                let waiting = matches!(self.details.get(&entry), Some(Detail::Loading(_)));
                if waiting {
                    self.details
                        .insert(entry, Detail::Loaded(Box::new(detail.clone())));
                }
                changed(waiting)
            }
            DaemonMessage::RemoteUrl(remote) => {
                let waiting = matches!(self.remotes.get(&remote.repo_id), Some(Remote::Loading(_)));
                if waiting {
                    self.remotes
                        .insert(remote.repo_id.clone(), Remote::Known(remote.clone()));
                }
                changed(waiting)
            }
            DaemonMessage::Error {
                message,
                request_id: Some(request_id),
            } => {
                if self.stale.remove(request_id).is_some() || self.fail(request_id, message) {
                    Applied::Consumed
                } else {
                    Applied::Ignored
                }
            }
            _ => Applied::Ignored,
        }
    }

    /// A page for `key`, taken only when it answers the read out for it.
    fn take_page(&mut self, key: &ScKey, offset: u32, commits: &[GitCommit]) -> bool {
        let Some(list) = self.lists.get_mut(key) else {
            return false;
        };
        if list
            .pending
            .as_ref()
            .is_none_or(|pending| pending.offset != offset)
        {
            return false;
        }
        list.pending = None;
        list.error = None;
        list.loaded = true;
        list.exhausted = commits.len() < PAGE_SIZE as usize;
        if offset == 0 {
            list.commits = commits.to_vec();
            if list
                .selected
                .as_ref()
                .is_some_and(|sha| !list.commits.iter().any(|commit| commit.sha == *sha))
            {
                list.selected = None;
            }
        } else {
            let mut seen: HashSet<String> = list
                .commits
                .iter()
                .map(|commit| commit.sha.clone())
                .collect();
            list.commits.extend(
                commits
                    .iter()
                    .filter(|commit| seen.insert(commit.sha.clone()))
                    .cloned(),
            );
        }
        true
    }

    /// Records the failure of read `request_id`; `false` when it is none of
    /// the History's.
    fn fail(&mut self, request_id: &str, message: &str) -> bool {
        let message = strip_daemon_prefix(message).to_owned();
        for list in self.lists.values_mut() {
            let Some(offset) = list
                .pending
                .as_ref()
                .filter(|pending| pending.request_id == request_id)
                .map(|pending| pending.offset)
            else {
                continue;
            };
            list.pending = None;
            list.error = Some((offset, message));
            return true;
        }
        for detail in self.details.values_mut() {
            if matches!(detail, Detail::Loading(id) if id == request_id) {
                *detail = Detail::Failed(message);
                return true;
            }
        }
        for remote in self.remotes.values_mut() {
            if matches!(remote, Remote::Loading(id) if id == request_id) {
                *remote = Remote::NoOrigin;
                return true;
            }
        }
        false
    }

    /// The reads the shown History needs and has not made: the first page of
    /// every `expanded` key with no list, and the remote of every repo in
    /// `repos` not looked up.
    pub fn request_missing(&mut self, expanded: &[ScKey], repos: &[&str]) -> Vec<ClientMessage> {
        let mut out = Vec::new();
        for key in expanded {
            if !self.lists.contains_key(key) {
                let mut list = CommitList::default();
                out.push(list.request(key, 0));
                self.lists.insert(key.clone(), list);
            }
        }
        for repo in repos {
            if !self.remotes.contains_key(*repo) {
                let request_id = new_request_id();
                self.remotes
                    .insert((*repo).to_owned(), Remote::Loading(request_id.clone()));
                out.push(ClientMessage::GetRemoteUrl {
                    repo_id: (*repo).to_owned(),
                    request_id: Some(request_id),
                });
            }
        }
        out
    }

    /// Drops the lists of keys that are no longer sections.
    pub fn retain(&mut self, keys: &[ScKey]) {
        let gone: Vec<ScKey> = self
            .lists
            .keys()
            .filter(|key| !keys.contains(key))
            .cloned()
            .collect();
        for key in gone {
            self.collapse(&key);
        }
    }

    /// A collapsed History forgets its commits and its selection.
    pub fn collapse(&mut self, key: &ScKey) {
        if let Some(pending) = self.lists.remove(key).and_then(|list| list.pending) {
            self.stale
                .insert(pending.request_id, (key.clone(), pending.offset));
        }
    }

    /// A reply nobody waits for arrived: one stale read it answers is done.
    fn retire_stale(&mut self, answered: &StaleRead) {
        let found = self
            .stale
            .iter()
            .find(|(_, read)| *read == answered)
            .map(|(request_id, _)| request_id.clone());
        if let Some(request_id) = found {
            self.stale.remove(&request_id);
        }
    }

    /// The `load more` row: the next page, or the failed one again. `None`
    /// while a read is out, before the first page, or once exhausted.
    pub fn load_more(&mut self, key: &ScKey) -> Option<ClientMessage> {
        let list = self.lists.get_mut(key)?;
        if !list.loaded || list.pending.is_some() {
            return None;
        }
        let offset = match &list.error {
            Some((offset, _)) => *offset,
            None if !list.exhausted => list.next_offset(),
            None => return None,
        };
        list.error = None;
        Some(list.request(key, offset))
    }

    /// Refresh: every `expanded` key reads again from the first page, its
    /// commits and selection kept until the answer replaces them, and every
    /// remote is looked up again. A first-page read or a lookup still out
    /// already answers the refresh, so it is kept, not sent again.
    pub fn refresh(&mut self, expanded: &[ScKey]) -> Vec<ClientMessage> {
        self.remotes
            .retain(|_, remote| matches!(remote, Remote::Loading(_)));
        let mut out = Vec::new();
        for key in expanded {
            let list = self.lists.entry(key.clone()).or_default();
            match list.pending.take() {
                Some(pending) if pending.offset == 0 => {
                    list.pending = Some(pending);
                    continue;
                }
                Some(pending) => {
                    self.stale
                        .insert(pending.request_id, (key.clone(), pending.offset));
                }
                None => {}
            }
            list.error = None;
            out.push(list.request(key, 0));
        }
        out
    }

    /// A click on a commit row: selects it, or deselects it when it is the
    /// selected one. Returns the detail read when it is not cached.
    pub fn select(&mut self, key: &ScKey, sha: &str) -> Option<ClientMessage> {
        let list = self.lists.get_mut(key)?;
        if list.selected.as_deref() == Some(sha) {
            list.selected = None;
            return None;
        }
        list.selected = Some(sha.to_owned());
        let entry = (key.repo_id.clone(), sha.to_owned());
        if matches!(
            self.details.get(&entry),
            Some(Detail::Loaded(_) | Detail::Loading(_))
        ) {
            return None;
        }
        let request_id = new_request_id();
        self.details
            .insert(entry, Detail::Loading(request_id.clone()));
        Some(ClientMessage::GetCommit {
            repo_id: key.repo_id.clone(),
            sha: sha.to_owned(),
            request_id: Some(request_id),
        })
    }

    /// The selected commit of `key`.
    #[must_use]
    pub fn selected(&self, key: &ScKey) -> Option<&str> {
        self.lists.get(key)?.selected.as_deref()
    }

    /// What one section's History block shows. `show_title` names the
    /// section in the header, for a panel of several sections.
    #[must_use]
    pub fn block(&self, section: &Section, expanded: bool, show_title: bool) -> HistoryBlock {
        let list = self.lists.get(&section.key).filter(|_| expanded);
        HistoryBlock {
            id: section.key.id(),
            expanded,
            title: show_title.then(|| section_title(section)),
            count: list
                .filter(|list| list.loaded && !list.commits.is_empty())
                .map(|list| list.commits.len()),
            forge: forge_button(
                self.remotes.get(&section.key.repo_id),
                section.branch.as_deref(),
            ),
            body: if expanded {
                self.body(&section.key, list)
            } else {
                HistoryBody::Collapsed
            },
        }
    }

    fn body(&self, key: &ScKey, list: Option<&CommitList>) -> HistoryBody {
        let Some(list) = list else {
            return HistoryBody::Loading;
        };
        if !list.loaded {
            return match &list.error {
                Some((_, message)) => HistoryBody::Failed(history_error(message)),
                None => HistoryBody::Loading,
            };
        }
        if list.commits.is_empty() {
            return HistoryBody::Empty;
        }
        let more = match (&list.error, &list.pending) {
            (Some((_, message)), _) => Some(MoreRow::Failed(history_error(message))),
            (None, Some(pending)) if pending.offset > 0 => Some(MoreRow::Loading),
            (None, None) if !list.exhausted => Some(MoreRow::LoadMore),
            // Exhausted, or a refresh's first page is out: the list is about
            // to be replaced, so there is no next page to offer yet.
            (None, _) => None,
        };
        let rows = list
            .commits
            .iter()
            .map(|commit| CommitRow {
                sha: commit.sha.clone(),
                short_sha: short_sha(&commit.sha),
                subject: commit.subject.clone(),
                author: commit.author_name.clone(),
                tooltip: commit_hover_text(commit),
                selected: list.selected.as_deref() == Some(commit.sha.as_str()),
            })
            .collect();
        let detail = list.selected.as_ref().map(|sha| {
            match self.details.get(&(key.repo_id.clone(), sha.clone())) {
                Some(Detail::Loaded(detail)) => DetailPane::Loaded(detail_view(detail)),
                Some(Detail::Failed(message)) => {
                    DetailPane::Failed(format!("couldn't load commit: {message}"))
                }
                Some(Detail::Loading(_)) | None => DetailPane::Loading,
            }
        });
        HistoryBody::Commits { rows, more, detail }
    }
}

fn changed(yes: bool) -> Applied {
    if yes {
        Applied::Changed
    } else {
        Applied::Ignored
    }
}

fn strip_daemon_prefix(message: &str) -> &str {
    DAEMON_PREFIXES
        .iter()
        .find_map(|prefix| message.strip_prefix(prefix))
        .unwrap_or(message)
}

fn history_error(message: &str) -> String {
    format!("couldn't load history: {message}")
}

/// A section's name as its header shows it: the repo, then ` · <branch>`
/// when the section has one.
#[must_use]
pub fn section_title(section: &Section) -> String {
    match &section.branch {
        Some(branch) => format!("{} · {branch}", section.repo_name),
        None => section.repo_name.clone(),
    }
}

/// One section's History block, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryBlock {
    /// The section's key id, which the block's selectors end in.
    pub id: String,
    pub expanded: bool,
    /// The section's title, when the panel shows more than one section.
    pub title: Option<String>,
    /// The loaded commit count, when there are commits.
    pub count: Option<usize>,
    pub forge: ForgeButton,
    pub body: HistoryBody,
}

/// The header's forge button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeButton {
    /// The page a click opens; `None` disables the button.
    pub url: Option<String>,
    pub tooltip: String,
}

/// Under a History header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryBody {
    Collapsed,
    /// The first page is being read.
    Loading,
    /// The first page came back empty.
    Empty,
    /// `couldn't load history: <message>`.
    Failed(String),
    Commits {
        rows: Vec<CommitRow>,
        more: Option<MoreRow>,
        /// The selected commit's pane.
        detail: Option<DetailPane>,
    },
}

impl HistoryBody {
    /// The one line a body without rows shows.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Loading => Some(LOADING),
            Self::Empty => Some(NO_COMMITS),
            Self::Failed(text) => Some(text),
            Self::Collapsed | Self::Commits { .. } => None,
        }
    }
}

/// One commit of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRow {
    pub sha: String,
    /// The sha's first seven characters.
    pub short_sha: String,
    pub subject: String,
    pub author: String,
    pub tooltip: String,
    pub selected: bool,
}

/// The row after the commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoreRow {
    LoadMore,
    Loading,
    /// The page failed; a click reads it again.
    Failed(String),
}

impl MoreRow {
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::LoadMore => LOAD_MORE,
            Self::Loading => LOADING,
            Self::Failed(text) => text,
        }
    }
}

/// The selected commit's pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailPane {
    Loading,
    /// `couldn't load commit: <message>`.
    Failed(String),
    Loaded(CommitDetailView),
}

/// A loaded commit, as its pane shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDetailView {
    /// `<short sha> <subject>`.
    pub heading: String,
    /// `Author: <name> <<email>>`.
    pub author: String,
    /// `Date: <date>`.
    pub date: String,
    /// The message body, when it has one.
    pub body: Option<String>,
    pub files: Vec<DetailFile>,
    /// Shown instead of files when the commit changed none.
    pub no_files: Option<&'static str>,
}

/// One changed file of a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailFile {
    pub status: String,
    pub path: String,
    /// `<from> → <path>` for a rename.
    pub tooltip: Option<String>,
}

fn detail_view(detail: &GitCommitDetail) -> CommitDetailView {
    let commit = &detail.commit;
    let body = detail.body.trim();
    let no_files = detail
        .changes
        .is_empty()
        .then_some(if detail.parent_shas.len() > 1 {
            NO_MERGE_CHANGES
        } else {
            NO_CHANGES
        });
    CommitDetailView {
        heading: format!("{} {}", short_sha(&commit.sha), commit.subject),
        author: format!(
            "Author: {}",
            author_text(&commit.author_name, &commit.author_email)
        ),
        date: format!("Date: {}", display_date(&commit.authored_at)),
        body: (!body.is_empty()).then(|| body.to_owned()),
        files: detail
            .changes
            .iter()
            .map(|change| DetailFile {
                status: change.status.clone(),
                path: change.path.clone(),
                tooltip: change
                    .from_path
                    .as_ref()
                    .map(|from| format!("{from} → {}", change.path)),
            })
            .collect(),
        no_files,
    }
}

/// A sha's first seven characters.
#[must_use]
pub fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// `<name> <<email>>`, or the name alone when the email is blank.
#[must_use]
pub fn author_text(name: &str, email: &str) -> String {
    let email = email.trim();
    if email.is_empty() {
        name.to_owned()
    } else {
        format!("{name} <{email}>")
    }
}

/// A commit row's tooltip: subject, sha, author and date, one per line.
#[must_use]
pub fn commit_hover_text(commit: &GitCommit) -> String {
    format!(
        "{}\nSHA: {}\nAuthor: {}\nDate: {}",
        commit.subject,
        commit.sha,
        author_text(&commit.author_name, &commit.author_email),
        display_date(&commit.authored_at)
    )
}

/// An ISO date as `YYYY-MM-DD HH:MM`, cut from the text as written with no
/// timezone conversion; the text itself when it is not of that shape.
#[must_use]
pub fn display_date(iso: &str) -> String {
    let bytes = iso.as_bytes();
    let digits = |from: usize, to: usize| {
        bytes
            .get(from..to)
            .is_some_and(|run| run.iter().all(u8::is_ascii_digit))
    };
    let at = |index: usize, want: &[u8]| bytes.get(index).is_some_and(|byte| want.contains(byte));
    let shaped = digits(0, 4)
        && at(4, b"-")
        && digits(5, 7)
        && at(7, b"-")
        && digits(8, 10)
        && at(10, b"T ")
        && digits(11, 13)
        && at(13, b":")
        && digits(14, 16);
    match (shaped, iso.get(..10), iso.get(11..16)) {
        (true, Some(day), Some(time)) => format!("{day} {time}"),
        _ => iso.to_owned(),
    }
}

/// The forge's display name, for a forge the button opens.
fn forge_name(forge: &str) -> Option<&'static str> {
    match forge {
        "github" => Some("GitHub"),
        "gitlab" => Some("GitLab"),
        "bitbucket" => Some("Bitbucket"),
        _ => None,
    }
}

/// The page for `branch` on the forge `web_url` lives on, or the repo's
/// home when the branch is unknown; `None` for a forge it does not know.
#[must_use]
pub fn branch_url(web_url: &str, forge: &str, branch: Option<&str>) -> Option<String> {
    let home = web_url.trim_end_matches('/');
    let path = match forge {
        "github" => "/tree/",
        "gitlab" => "/-/tree/",
        "bitbucket" => "/src/",
        _ => return None,
    };
    Some(match branch {
        Some(branch) => format!("{home}{path}{}", encode_branch(branch)),
        None => home.to_owned(),
    })
}

/// `branch` percent-encoded as `encodeURIComponent` does, `/` kept.
fn encode_branch(branch: &str) -> String {
    let mut out = String::with_capacity(branch.len());
    for byte in branch.bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' | b'/'
            );
        if keep {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn forge_button(remote: Option<&Remote>, branch: Option<&str>) -> ForgeButton {
    let disabled = |tooltip: &str| ForgeButton {
        url: None,
        tooltip: tooltip.to_owned(),
    };
    match remote {
        None | Some(Remote::Loading(_)) => disabled(LOOKING_UP_ORIGIN),
        Some(Remote::NoOrigin) => disabled(NO_ORIGIN),
        Some(Remote::Known(remote)) => {
            let target = remote.web_url.as_deref().and_then(|web_url| {
                let name = forge_name(&remote.forge)?;
                Some((branch_url(web_url, &remote.forge, branch)?, name))
            });
            match target {
                Some((url, name)) => ForgeButton {
                    url: Some(url),
                    tooltip: match branch {
                        Some(branch) => format!("Open {branch} on {name}"),
                        None => format!("Open the repository on {name}"),
                    },
                },
                None => disabled(NOT_A_FORGE),
            }
        }
    }
}

/// `height` kept at least `min_side` from either end of `total`; half of
/// `total` when it cannot hold both sides.
#[must_use]
pub fn clamp_split(height: f32, total: f32, min_side: f32) -> f32 {
    if total < min_side * 2.0 {
        (total / 2.0).max(0.0)
    } else {
        height.clamp(min_side, total - min_side)
    }
}

/// The changes area's height in a body `total` pixels tall; the stored or
/// default height as it is while the body has not been laid out.
#[must_use]
pub fn changes_height(state: &ScUiState, total: Option<f32>) -> f32 {
    let height = state.changes_height.unwrap_or(DEFAULT_CHANGES_HEIGHT);
    total.map_or(height, |total| clamp_split(height, total, MIN_CHANGES_SIDE))
}

/// The height of `key`'s commit list above its detail pane, in a block
/// `total` pixels tall.
#[must_use]
pub fn list_height(state: &ScUiState, key: &ScKey, total: Option<f32>) -> f32 {
    let height = state
        .history_list_height
        .get(&key.id())
        .copied()
        .unwrap_or(DEFAULT_LIST_HEIGHT);
    total.map_or(height, |total| clamp_split(height, total, MIN_LIST_SIDE))
}

/// Where the panel's body and each History block's content were last laid
/// out, as (top, height) in window pixels, for the split drags.
#[derive(Debug, Default)]
pub struct ScLayout {
    pub body: Option<(f32, f32)>,
    /// By section key id.
    pub blocks: HashMap<String, (f32, f32)>,
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::source_control::Part;
    use protocol::RepoEntry;
    use serde_json::json;

    fn key(repo_id: &str, worktree: Option<&str>) -> ScKey {
        ScKey {
            repo_id: repo_id.to_owned(),
            worktree: worktree.map(str::to_owned),
        }
    }

    fn section(repo_id: &str, branch: Option<&str>) -> Section {
        Section {
            key: key(repo_id, None),
            repo_name: repo_id.to_owned(),
            branch: branch.map(str::to_owned),
        }
    }

    fn commit(sha: &str) -> GitCommit {
        GitCommit {
            sha: sha.to_owned(),
            short_sha: sha.chars().take(9).collect(),
            author_name: "Ada".to_owned(),
            author_email: "ada@example.com".to_owned(),
            authored_at: "2026-09-26T14:03:12+02:00".to_owned(),
            subject: format!("subject {sha}"),
        }
    }

    fn page(from: usize, count: usize) -> Vec<GitCommit> {
        (from..from + count)
            .map(|i| commit(&format!("{i:040}")))
            .collect()
    }

    fn commits_message(key: &ScKey, offset: u32, commits: Vec<GitCommit>) -> DaemonMessage {
        DaemonMessage::Commits {
            repo_id: key.repo_id.clone(),
            commits,
            offset,
            worktree_path: key.worktree.clone(),
        }
    }

    fn error(message: &str, request_id: &str) -> DaemonMessage {
        DaemonMessage::Error {
            message: message.to_owned(),
            request_id: Some(request_id.to_owned()),
        }
    }

    fn welcome() -> DaemonMessage {
        DaemonMessage::Welcome {
            protocol_version: 23,
            supported_versions: vec![23],
        }
    }

    /// The offset and request id of a `ListCommits`.
    fn list_read(msg: &ClientMessage) -> (u32, String) {
        match msg {
            ClientMessage::ListCommits {
                offset,
                limit,
                request_id: Some(request_id),
                ..
            } => {
                assert_eq!(*limit, PAGE_SIZE);
                (*offset, request_id.clone())
            }
            other => panic!("expected a commit list read, got {other:?}"),
        }
    }

    fn request_id(msg: &ClientMessage) -> String {
        match msg {
            ClientMessage::ListCommits { request_id, .. }
            | ClientMessage::GetCommit { request_id, .. }
            | ClientMessage::GetRemoteUrl { request_id, .. } => {
                request_id.clone().expect("a History read carries an id")
            }
            other => panic!("not a History read: {other:?}"),
        }
    }

    fn body(model: &HistoryModel, key: &ScKey) -> HistoryBody {
        let section = Section {
            key: key.clone(),
            repo_name: key.repo_id.clone(),
            branch: None,
        };
        model.block(&section, true, false).body
    }

    fn rows_and_more(model: &HistoryModel, key: &ScKey) -> (usize, Option<MoreRow>) {
        match body(model, key) {
            HistoryBody::Commits { rows, more, .. } => (rows.len(), more),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    /// A model with `key`'s first page (`count` commits) loaded.
    fn loaded(key: &ScKey, count: usize) -> HistoryModel {
        let mut model = HistoryModel::default();
        let reads = model.request_missing(std::slice::from_ref(key), &[]);
        assert_eq!(list_read(&reads[0]).0, 0);
        assert_eq!(
            model.apply(&commits_message(key, 0, page(0, count))),
            Applied::Changed
        );
        model
    }

    #[test]
    fn paging_reads_the_next_offset_and_dedupes() {
        let k = key("r1", Some("D:/wt"));
        let mut model = loaded(&k, 50);
        assert_eq!(rows_and_more(&model, &k), (50, Some(MoreRow::LoadMore)));

        let read = model.load_more(&k).expect("a full page offers more");
        assert!(matches!(
            &read,
            ClientMessage::ListCommits { worktree_path: Some(path), .. } if path == "D:/wt"
        ));
        assert_eq!(list_read(&read).0, 50);
        assert_eq!(rows_and_more(&model, &k).1, Some(MoreRow::Loading));
        assert!(model.load_more(&k).is_none(), "one read at a time");

        // The second page repeats the last commit of the first.
        assert_eq!(
            model.apply(&commits_message(&k, 50, page(49, 50))),
            Applied::Changed
        );
        assert_eq!(
            rows_and_more(&model, &k),
            (99, Some(MoreRow::LoadMore)),
            "the repeated sha is dropped"
        );

        let read = model.load_more(&k).expect("still full");
        assert_eq!(
            list_read(&read).0,
            99,
            "the next offset is the loaded count"
        );
        model.apply(&commits_message(&k, 99, page(99, 3)));
        assert_eq!(
            rows_and_more(&model, &k),
            (102, None),
            "a short page exhausts the list"
        );
        assert!(model.load_more(&k).is_none());
    }

    #[test]
    fn a_page_nobody_asked_for_is_ignored() {
        let k = key("r1", None);
        let mut model = loaded(&k, 50);
        assert_eq!(
            model.apply(&commits_message(&k, 50, page(50, 10))),
            Applied::Ignored,
            "no read is out"
        );
        assert_eq!(
            model.apply(&commits_message(&key("r2", None), 0, page(0, 1))),
            Applied::Ignored
        );
    }

    #[test]
    fn a_failed_page_stays_retryable() {
        let k = key("r1", None);
        let mut model = loaded(&k, 50);
        let read = model.load_more(&k).expect("more");
        let (_, id) = list_read(&read);
        assert_eq!(
            model.apply(&error("commit list failed: boom", &id)),
            Applied::Consumed
        );
        assert_eq!(
            rows_and_more(&model, &k),
            (
                50,
                Some(MoreRow::Failed("couldn't load history: boom".to_owned()))
            )
        );
        let retry = model.load_more(&k).expect("the failed row retries");
        assert_eq!(list_read(&retry).0, 50, "the same page again");
        model.apply(&commits_message(&k, 50, page(50, 2)));
        assert_eq!(rows_and_more(&model, &k), (52, None));
    }

    #[test]
    fn a_failed_first_page_is_the_body() {
        let k = key("r1", None);
        let mut model = HistoryModel::default();
        let reads = model.request_missing(std::slice::from_ref(&k), &[]);
        assert_eq!(body(&model, &k), HistoryBody::Loading);
        model.apply(&error("commit list failed: bad", &request_id(&reads[0])));
        assert_eq!(
            body(&model, &k),
            HistoryBody::Failed("couldn't load history: bad".to_owned())
        );
        assert!(model.load_more(&k).is_none());
        assert!(
            model
                .request_missing(std::slice::from_ref(&k), &[])
                .is_empty(),
            "a failed list is not read again until refreshed"
        );

        let empty_key = key("r2", None);
        let mut empty = HistoryModel::default();
        empty.request_missing(std::slice::from_ref(&empty_key), &[]);
        empty.apply(&commits_message(&empty_key, 0, Vec::new()));
        assert_eq!(body(&empty, &empty_key), HistoryBody::Empty);
    }

    #[test]
    fn errors_route_by_request_id_for_all_three_reads() {
        let k = key("r1", None);
        let mut model = loaded(&k, 3);
        let sha = page(0, 1)[0].sha.clone();
        let detail_read = model.select(&k, &sha).expect("a detail read");
        let reads = model.request_missing(&[], &["r1"]);
        let remote_id = request_id(&reads[0]);

        assert_eq!(
            model.apply(&error("unrelated", "someone-else")),
            Applied::Ignored,
            "an id the History did not send is left for the toast"
        );
        assert_eq!(
            model.apply(&DaemonMessage::Error {
                message: "no id".to_owned(),
                request_id: None
            }),
            Applied::Ignored
        );
        assert_eq!(
            model.apply(&error(
                "commit detail failed: gone",
                &request_id(&detail_read)
            )),
            Applied::Consumed
        );
        match body(&model, &k) {
            HistoryBody::Commits {
                detail: Some(DetailPane::Failed(text)),
                ..
            } => assert_eq!(text, "couldn't load commit: gone"),
            other => panic!("expected a failed detail, got {other:?}"),
        }
        assert_eq!(
            model.apply(&error("remote url failed: no origin", &remote_id)),
            Applied::Consumed
        );
        let block = model.block(&section("r1", Some("main")), true, false);
        assert_eq!(block.forge.url, None);
        assert_eq!(block.forge.tooltip, NO_ORIGIN);
        assert_eq!(
            model.apply(&error("again", &remote_id)),
            Applied::Ignored,
            "an answered id is not taken twice"
        );
    }

    #[test]
    fn collapse_drops_the_list_and_its_late_error_quietly() {
        let k = key("r1", None);
        let mut model = loaded(&k, 50);
        model.select(&k, &page(0, 1)[0].sha);
        let read = model.load_more(&k).expect("more");
        model.collapse(&k);
        assert_eq!(model.selected(&k), None, "the selection went");
        assert_eq!(
            body(&model, &k),
            HistoryBody::Loading,
            "nothing is loaded any more"
        );
        assert_eq!(
            model.apply(&error("late", &request_id(&read))),
            Applied::Consumed,
            "the collapsed list's failure raises no toast"
        );
        let reads = model.request_missing(std::slice::from_ref(&k), &[]);
        assert_eq!(list_read(&reads[0]).0, 0, "expanding reads from the start");

        model.retain(&[]);
        assert!(
            model.request_missing(std::slice::from_ref(&k), &[]).len() == 1,
            "a key that stopped being a section was dropped"
        );
        assert_eq!(model.apply(&welcome()), Applied::Changed);
        assert_eq!(model.apply(&welcome()), Applied::Ignored);
    }

    #[test]
    fn selection_toggles_and_details_are_cached() {
        let k = key("r1", None);
        let mut model = loaded(&k, 3);
        let sha = page(0, 1)[0].sha.clone();
        let read = model.select(&k, &sha).expect("first selection reads");
        assert!(
            matches!(&read, ClientMessage::GetCommit { sha: s, repo_id, .. } if *s == sha && repo_id == "r1")
        );
        assert_eq!(model.selected(&k), Some(sha.as_str()));
        assert!(model.select(&k, &sha).is_none(), "a second click deselects");
        assert_eq!(model.selected(&k), None);
        assert!(
            model.select(&k, &sha).is_none(),
            "still loading, so not read twice"
        );

        let mut detail = GitCommitDetail {
            commit: page(0, 1).remove(0),
            body: "  why\n".to_owned(),
            parent_shas: vec!["p1".to_owned()],
            changes: vec![protocol::GitFileChange {
                path: "new.rs".to_owned(),
                status: "R".to_owned(),
                from_path: Some("old.rs".to_owned()),
            }],
        };
        assert_eq!(
            model.apply(&DaemonMessage::CommitDetail {
                repo_id: "r1".to_owned(),
                detail: detail.clone(),
            }),
            Applied::Changed
        );
        match body(&model, &k) {
            HistoryBody::Commits {
                detail: Some(DetailPane::Loaded(view)),
                ..
            } => {
                assert_eq!(view.heading, format!("{} subject {sha}", &sha[..7]));
                assert_eq!(view.author, "Author: Ada <ada@example.com>");
                assert_eq!(view.date, "Date: 2026-09-26 14:03");
                assert_eq!(view.body.as_deref(), Some("why"));
                assert_eq!(view.files[0].tooltip.as_deref(), Some("old.rs → new.rs"));
                assert_eq!(view.no_files, None);
            }
            other => panic!("expected a loaded detail, got {other:?}"),
        }
        model.select(&k, &sha);
        let other = page(1, 1)[0].sha.clone();
        assert!(model.select(&k, &other).is_some());
        assert!(
            model.select(&k, &sha).is_none(),
            "a cached detail is not read again"
        );

        detail.changes.clear();
        assert_eq!(detail_view(&detail).no_files, Some(NO_CHANGES));
        detail.parent_shas.push("p2".to_owned());
        assert_eq!(detail_view(&detail).no_files, Some(NO_MERGE_CHANGES));
    }

    #[test]
    fn refresh_rereads_from_the_start_and_keeps_a_surviving_selection() {
        let k = key("r1", None);
        let mut model = loaded(&k, 50);
        let first = page(0, 1)[0].sha.clone();
        model.select(&k, &first);
        model.request_missing(&[], &["r1"]);
        model.apply(&DaemonMessage::RemoteUrl(GitRemoteUrl {
            repo_id: "r1".to_owned(),
            raw_url: "git@x:o/r.git".to_owned(),
            web_url: None,
            forge: "unknown".to_owned(),
        }));

        let reads = model.refresh(std::slice::from_ref(&k));
        assert_eq!(reads.len(), 1);
        assert_eq!(list_read(&reads[0]).0, 0);
        assert_eq!(
            model.request_missing(&[], &["r1"]).len(),
            1,
            "the answered remote is looked up again"
        );
        assert_eq!(
            model.refresh(&[]).len(),
            0,
            "a lookup still out is not sent again"
        );
        assert!(
            model.request_missing(&[], &["r1"]).is_empty(),
            "and it is kept"
        );
        assert_eq!(
            rows_and_more(&model, &k),
            (50, None),
            "the rows stay meanwhile, with no load more while the first page is out"
        );
        assert!(model.load_more(&k).is_none());

        model.apply(&commits_message(&k, 0, page(0, 10)));
        assert_eq!(
            rows_and_more(&model, &k),
            (10, None),
            "replaced, not appended"
        );
        assert_eq!(model.selected(&k), Some(first.as_str()));

        model.refresh(std::slice::from_ref(&k));
        model.apply(&commits_message(&k, 0, page(20, 5)));
        assert_eq!(model.selected(&k), None, "the selected sha is gone");
    }

    #[test]
    fn refresh_keeps_a_first_page_read_still_out() {
        let k = key("r1", None);
        let mut model = HistoryModel::default();
        let first = model.request_missing(std::slice::from_ref(&k), &[]);
        let (_, first_id) = list_read(&first[0]);
        assert!(
            model.refresh(std::slice::from_ref(&k)).is_empty(),
            "the read out already answers the refresh"
        );
        assert_eq!(
            model.apply(&error("commit list failed: boom", &first_id)),
            Applied::Consumed
        );
        assert_eq!(
            body(&model, &k),
            HistoryBody::Failed("couldn't load history: boom".to_owned()),
            "its failure is shown, not swallowed as stale"
        );

        let mut paging = loaded(&k, 50);
        let more = paging.load_more(&k).expect("more");
        let reads = paging.refresh(std::slice::from_ref(&k));
        assert_eq!(list_read(&reads[0]).0, 0, "a page read is replaced");
        assert_eq!(
            paging.apply(&commits_message(&k, 50, page(50, 50))),
            Applied::Ignored,
            "the replaced page is not taken"
        );
        assert_eq!(rows_and_more(&paging, &k).0, 50);
        assert_eq!(
            paging.apply(&error("late", &request_id(&more))),
            Applied::Ignored,
            "its page retired it, so its id is forgotten"
        );
    }

    #[test]
    fn stale_ids_are_retired_by_their_late_page() {
        let k = key("r1", Some("D:/wt"));
        let mut model = loaded(&k, 50);
        model.load_more(&k).expect("more");
        model.collapse(&k);
        assert_eq!(model.stale.len(), 1);
        assert_eq!(
            model.apply(&commits_message(&key("r1", None), 50, page(50, 1))),
            Applied::Ignored
        );
        assert_eq!(model.stale.len(), 1, "another key's page retires nothing");
        model.apply(&commits_message(&k, 0, page(0, 1)));
        assert_eq!(
            model.stale.len(),
            1,
            "another offset's page retires nothing"
        );
        model.apply(&commits_message(&k, 50, page(50, 1)));
        assert!(model.stale.is_empty(), "the late page retired its read");
    }

    #[test]
    fn remote_replies_fill_the_forge_button() {
        let mut model = HistoryModel::default();
        let block =
            |model: &HistoryModel, branch| model.block(&section("r1", branch), false, false);
        assert_eq!(block(&model, None).forge.tooltip, LOOKING_UP_ORIGIN);
        model.request_missing(&[], &["r1"]);
        let remote = |forge: &str, web_url: Option<&str>| {
            DaemonMessage::RemoteUrl(GitRemoteUrl {
                repo_id: "r1".to_owned(),
                raw_url: "git@x:o/r.git".to_owned(),
                web_url: web_url.map(str::to_owned),
                forge: forge.to_owned(),
            })
        };
        assert_eq!(
            model.apply(&remote("github", Some("https://github.com/o/r"))),
            Applied::Changed
        );
        let forge = block(&model, Some("feat/x")).forge;
        assert_eq!(
            forge.url.as_deref(),
            Some("https://github.com/o/r/tree/feat/x")
        );
        assert_eq!(forge.tooltip, "Open feat/x on GitHub");
        let home = block(&model, None).forge;
        assert_eq!(home.url.as_deref(), Some("https://github.com/o/r"));
        assert_eq!(home.tooltip, "Open the repository on GitHub");

        let mut other = HistoryModel::default();
        other.request_missing(&[], &["r1"]);
        other.apply(&remote("unknown", Some("https://git.example/o/r")));
        assert_eq!(block(&other, Some("main")).forge.tooltip, NOT_A_FORGE);
        let mut no_web = HistoryModel::default();
        no_web.request_missing(&[], &["r1"]);
        no_web.apply(&remote("github", None));
        assert_eq!(block(&no_web, Some("main")).forge.url, None);
        assert_eq!(block(&no_web, Some("main")).forge.tooltip, NOT_A_FORGE);
    }

    #[test]
    fn branch_urls_per_forge() {
        assert_eq!(
            branch_url("https://github.com/o/r/", "github", Some("main")).as_deref(),
            Some("https://github.com/o/r/tree/main")
        );
        assert_eq!(
            branch_url("https://gitlab.com/o/r", "gitlab", Some("main")).as_deref(),
            Some("https://gitlab.com/o/r/-/tree/main")
        );
        assert_eq!(
            branch_url("https://bitbucket.org/o/r", "bitbucket", Some("main")).as_deref(),
            Some("https://bitbucket.org/o/r/src/main")
        );
        assert_eq!(branch_url("https://x/o/r", "unknown", Some("main")), None);
        assert_eq!(
            branch_url("https://github.com/o/r", "github", Some("feat/a#1 ü")).as_deref(),
            Some("https://github.com/o/r/tree/feat/a%231%20%C3%BC"),
            "`/` stays, `#`, space and non-ASCII are encoded"
        );
        assert_eq!(
            branch_url("https://github.com/o/r//", "github", None).as_deref(),
            Some("https://github.com/o/r"),
            "no branch is the repo home"
        );
    }

    #[test]
    fn hover_text_with_and_without_email() {
        let mut c = commit("0123456789abcdef");
        assert_eq!(
            commit_hover_text(&c),
            "subject 0123456789abcdef\nSHA: 0123456789abcdef\nAuthor: Ada <ada@example.com>\nDate: 2026-09-26 14:03"
        );
        c.author_email = "  ".to_owned();
        assert_eq!(
            commit_hover_text(&c),
            "subject 0123456789abcdef\nSHA: 0123456789abcdef\nAuthor: Ada\nDate: 2026-09-26 14:03"
        );
        assert_eq!(short_sha(&c.sha), "0123456");
    }

    #[test]
    fn dates_are_cut_without_conversion() {
        assert_eq!(
            display_date("2026-09-26T23:59:01-07:00"),
            "2026-09-26 23:59"
        );
        assert_eq!(display_date("2026-09-26 08:05:00Z"), "2026-09-26 08:05");
        assert_eq!(display_date("2026-09-26"), "2026-09-26", "too short");
        assert_eq!(display_date("yesterday at noon"), "yesterday at noon");
        assert_eq!(display_date("2026/09/26T10:00"), "2026/09/26T10:00");
        assert_eq!(display_date(""), "");
    }

    #[test]
    fn history_collapse_defaults_follow_the_focus() {
        let mut state = ScUiState::default();
        let k = key("r1", None);
        assert!(state.is_history_collapsed(&k, true), "a focused session's");
        assert!(!state.is_history_collapsed(&k, false), "a browsed repo's");
        assert!(
            !state.is_collapsed(&k, Part::History, None),
            "the part query answers for no focused session"
        );
        state.set_collapsed(&k, Part::History, false);
        assert!(!state.is_history_collapsed(&k, true), "a stored value wins");
        state.set_collapsed(&k, Part::History, true);
        assert!(state.is_history_collapsed(&k, false));
    }

    #[test]
    fn split_heights_clamp_to_both_sides() {
        let mut state = ScUiState::default();
        let k = key("r1", None);
        assert!((changes_height(&state, None) - 420.0).abs() < f32::EPSILON);
        assert!((changes_height(&state, Some(1000.0)) - 420.0).abs() < f32::EPSILON);
        assert!(
            (changes_height(&state, Some(500.0)) - 360.0).abs() < f32::EPSILON,
            "the History keeps 140"
        );
        state.changes_height = Some(20.0);
        assert!((changes_height(&state, Some(1000.0)) - 140.0).abs() < f32::EPSILON);
        assert!(
            (changes_height(&state, Some(200.0)) - 100.0).abs() < f32::EPSILON,
            "too short for both: half each"
        );

        assert!((list_height(&state, &k, None) - 220.0).abs() < f32::EPSILON);
        assert!(
            (list_height(&state, &k, Some(250.0)) - 160.0).abs() < f32::EPSILON,
            "the detail keeps 90"
        );
        state.history_list_height.insert(k.id(), 10.0);
        assert!((list_height(&state, &k, Some(600.0)) - 90.0).abs() < f32::EPSILON);
        assert!(
            (list_height(&state, &key("r2", None), Some(600.0)) - 220.0).abs() < f32::EPSILON,
            "per section"
        );
    }

    #[test]
    fn prune_drops_list_heights_of_unregistered_repos() {
        let mut state = ScUiState::default();
        state.history_list_height.insert("r1::".to_owned(), 300.0);
        state
            .history_list_height
            .insert("r2::D:/wt".to_owned(), 300.0);
        let r1: RepoEntry =
            serde_json::from_value(json!({ "id": "r1", "name": "r1", "path": "D:/r1" }))
                .expect("repo fixture");
        assert!(state.prune(&[r1]));
        assert_eq!(
            state.history_list_height.keys().collect::<Vec<_>>(),
            ["r1::"]
        );
    }
}
