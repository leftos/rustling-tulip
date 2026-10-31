//! Files fetched from a remote host onto this PC: where each one lands, the
//! part file a transfer writes into before it takes the file's place, and the
//! transfers in flight.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::html_media::{
    MAX_MEDIA_BATCH_BYTES, MAX_MEDIA_FILE_BYTES, MAX_MEDIA_PER_PAGE, MEDIA_FETCHES_IN_FLIGHT,
};
use crate::open::OpenJob;
use crate::target::{HostKey, Target};

/// A file under a repo root or one of its worktrees on the remote host, to
/// fetch onto this PC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    /// The host the path lives on: the host of the pane, dialog or page
    /// the fetch came from, never one found by the repo id.
    pub host: HostKey,
    /// The registered repo the path is under.
    pub repo_id: String,
    /// The repo's name, the folder its files land in.
    pub repo_name: String,
    /// The worktree the path is under; `None` for the repo root.
    pub worktree_path: Option<String>,
    /// The path relative to the repo root or worktree, `/` or `\` separated.
    pub rel_path: String,
    /// The line to open the saved copy at; `None` opens it in its default
    /// app.
    pub line: Option<u32>,
    /// The column on `line`; defaults to 1.
    pub column: Option<u32>,
    /// Whether the saved copy opens once the fetch is done.
    pub open_when_done: bool,
}

/// Characters Windows forbids in a file name.
const FORBIDDEN: [char; 7] = ['<', '>', ':', '"', '|', '?', '*'];

/// Windows' reserved device names, which no file may be named, with or
/// without an extension; the superscript digits are U+00B9, U+00B2 and
/// U+00B3.
const RESERVED: [&str; 30] = [
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "CONIN$",
    "CONOUT$",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "COM\u{b9}",
    "COM\u{b2}",
    "COM\u{b3}",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "LPT\u{b9}",
    "LPT\u{b2}",
    "LPT\u{b3}",
];

/// Whether Windows reads `segment` as a device: its stem, before the first
/// dot and without trailing spaces, is a reserved name.
fn is_reserved(segment: &str) -> bool {
    let stem = segment
        .split('.')
        .next()
        .unwrap_or(segment)
        .trim_end_matches(' ');
    RESERVED.iter().any(|name| name.eq_ignore_ascii_case(stem))
}

/// `segment` as a name Windows accepts: a forbidden or control character
/// becomes `_`, a trailing dot or space becomes `_`, and a reserved device
/// name gets a leading `_`.
fn make_safe(segment: &str) -> String {
    let mut safe: String = segment
        .chars()
        .map(|c| {
            if FORBIDDEN.contains(&c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    if safe.ends_with(['.', ' ']) {
        safe.pop();
        safe.push('_');
    }
    if is_reserved(&safe) {
        safe.insert(0, '_');
    }
    safe
}

/// A host or repo name as one folder name: separators become `_` too, and an
/// empty, `.` or `..` name becomes `_`.
#[must_use]
pub fn sanitize_segment(segment: &str) -> String {
    if matches!(segment, "" | "." | "..") {
        return "_".to_owned();
    }
    make_safe(&segment.replace(['/', '\\'], "_"))
}

/// The folder a target's fetched files land in: the tailnet host's `MagicDNS`
/// name, or `ssh-` and the SSH host's string, lower-cased and made safe, so
/// the two kinds never share a folder; `None` for the local daemon, whose
/// files are already on this PC.
#[must_use]
pub fn host_folder(target: &Target) -> Option<String> {
    let folder = match target {
        Target::Local => return None,
        Target::Tailnet(host) => host.name().to_lowercase(),
        Target::Ssh(host) => format!("ssh-{}", host.name().to_lowercase()),
    };
    Some(sanitize_segment(&folder))
}

/// The last segment of the relative path `rel`.
#[must_use]
pub fn file_name(rel: &str) -> &str {
    rel.rsplit(['/', '\\']).next().unwrap_or(rel)
}

/// `rel`'s segments made safe; refuses an empty path, a leading separator,
/// a drive prefix, and an empty, `.` or `..` segment.
pub(crate) fn rel_segments(rel: &str) -> Result<Vec<String>, String> {
    if rel.is_empty() {
        return Err("the path is empty".to_owned());
    }
    if rel.starts_with(['/', '\\']) {
        return Err(format!(
            "{rel} starts with a separator; give a path relative to the repo"
        ));
    }
    let mut chars = rel.chars();
    if chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.next() == Some(':') {
        return Err(format!(
            "{rel} has a drive prefix; give a path relative to the repo"
        ));
    }
    rel.split(['/', '\\'])
        .map(|segment| match segment {
            "" => Err(format!("{rel} has an empty segment")),
            "." => Err(format!("{rel} has a \".\" segment")),
            ".." => Err(format!("{rel} has a \"..\" segment")),
            _ => Ok(make_safe(segment)),
        })
        .collect()
}

/// Where the file `rel` of the repo `repo_name` on `host` lands:
/// `<root>/<host>/<repo-name>/<rel>`.
///
/// # Errors
/// Why `rel` is refused: empty, absolute, drive-prefixed, or with an empty,
/// `.` or `..` segment.
pub fn download_path(
    root: &Path,
    host: &str,
    repo_name: &str,
    rel: &str,
) -> Result<PathBuf, String> {
    let segments = rel_segments(rel)?;
    let mut path = root
        .join(sanitize_segment(host))
        .join(sanitize_segment(repo_name));
    path.extend(segments);
    Ok(path)
}

/// The part file the fetch `id` writes `target` into: hidden, beside it,
/// and as short whatever the target's name.
#[must_use]
pub fn part_path(target: &Path, id: &str) -> PathBuf {
    target.with_file_name(format!(".{id}.part"))
}

/// Whether `path` lies below `root`, compared component by component; a
/// path with a `..` anywhere is never under it.
#[must_use]
pub fn is_under(root: &Path, path: &Path) -> bool {
    !path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
        && path != root
        && path.starts_with(root)
}

/// A host path as display text, without Windows' verbatim prefix:
/// `\\?\C:\x` shows as `C:\x` and `\\?\UNC\server\share` as
/// `\\server\share`.
#[must_use]
pub fn display_host_path(resolved: &str) -> String {
    if let Some(rest) = resolved.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = resolved.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        resolved.to_owned()
    }
}

/// How the saved copy at `path` opens: in VS Code at `line` (column 1 unless
/// given), else in its default app, which asks first for a file that runs
/// code.
#[must_use]
pub fn open_job(path: PathBuf, line: Option<u32>, column: Option<u32>) -> OpenJob {
    match line {
        Some(line) => OpenJob::VsCode {
            path,
            line,
            column: column.unwrap_or(1),
        },
        None => OpenJob::DefaultApp {
            path,
            confirmed: false,
        },
    }
}

/// One step for a fetch's writer, in the order the daemon sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteStep {
    Chunk {
        seq: u64,
        data: Vec<u8>,
    },
    /// Every chunk was sent: check the size and put the file in place.
    Done,
    /// Stop and delete the part file.
    Abort,
}

/// How a fetch's writer ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The file is saved at this path.
    Saved(PathBuf),
    /// Told to stop, or its channel closed; the part file is deleted.
    Aborted,
    /// Why the file was not saved; a part file this writer made is deleted.
    Failed(String),
}

/// A part file this fetch made itself, so deleting it never removes a file
/// it did not create.
struct PartFile {
    part: PathBuf,
    target: PathBuf,
    file: File,
    next_seq: u64,
    written: u64,
}

impl PartFile {
    /// Makes the target's folders and a new part file; a file already at
    /// the part's path fails it and is left alone.
    fn create(target: &Path, id: &str) -> Result<Self, String> {
        let parent = target
            .parent()
            .ok_or_else(|| format!("{} has no folder", target.display()))?;
        fs::create_dir_all(parent)
            .map_err(|err| format!("creating {}: {err}", parent.display()))?;
        let part = part_path(target, id);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .map_err(|err| format!("creating {}: {err}", part.display()))?;
        Ok(Self {
            part,
            target: target.to_path_buf(),
            file,
            next_seq: 0,
            written: 0,
        })
    }

    /// Appends chunk `seq`, which must be the next one.
    fn write(&mut self, seq: u64, data: &[u8]) -> Result<(), String> {
        if seq != self.next_seq {
            return Err(format!(
                "chunk {seq} arrived where chunk {} was expected",
                self.next_seq
            ));
        }
        self.file
            .write_all(data)
            .map_err(|err| format!("writing {}: {err}", self.part.display()))?;
        self.next_seq += 1;
        self.written += data.len() as u64;
        Ok(())
    }

    /// Puts the part in the target's place once it holds at least `size`
    /// bytes (a file that grew during the transfer is streamed to its end);
    /// any failure deletes the part.
    fn finish(self, size: u64) -> Result<PathBuf, String> {
        let Self {
            part,
            target,
            file,
            written,
            ..
        } = self;
        drop(file);
        if written < size {
            remove_part(&part);
            return Err(format!(
                "received {written} of {size} bytes before the transfer ended"
            ));
        }
        match fs::rename(&part, &target) {
            Ok(()) => Ok(target),
            Err(err) => {
                remove_part(&part);
                Err(format!("saving {}: {err}", target.display()))
            }
        }
    }

    fn discard(self) {
        let Self { part, file, .. } = self;
        drop(file);
        remove_part(&part);
    }
}

fn remove_part(part: &Path) {
    if let Err(err) = fs::remove_file(part) {
        tracing::warn!(part = %part.display(), "could not delete a fetch's part file: {err}");
    }
}

/// Writes the fetch `id` of a `size`-byte file into its part file beside
/// `target` as `steps` arrive, and renames it over `target` on
/// [`WriteStep::Done`]. Blocks on the disk, so it runs off the UI thread.
pub async fn run_writer(
    target: PathBuf,
    id: String,
    size: u64,
    mut steps: UnboundedReceiver<WriteStep>,
) -> WriteOutcome {
    let mut part = match PartFile::create(&target, &id) {
        Ok(part) => part,
        Err(reason) => return WriteOutcome::Failed(reason),
    };
    while let Some(step) = steps.next().await {
        match step {
            WriteStep::Chunk { seq, data } => {
                if let Err(reason) = part.write(seq, &data) {
                    part.discard();
                    return WriteOutcome::Failed(reason);
                }
            }
            WriteStep::Done => {
                return match part.finish(size) {
                    Ok(saved) => WriteOutcome::Saved(saved),
                    Err(reason) => WriteOutcome::Failed(reason),
                };
            }
            WriteStep::Abort => break,
        }
    }
    part.discard();
    WriteOutcome::Aborted
}

/// The readings of one terminal link, tried one at a time: the ones still to
/// try after the reading in flight, and the key of the one toast that says
/// the link is fetching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeGroup {
    key: String,
    rest: VecDeque<FetchRequest>,
}

impl ProbeGroup {
    /// The key of the group's "Fetching" toast.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The readings still to try, in order.
    #[must_use]
    pub fn remaining(&self) -> &VecDeque<FetchRequest> {
        &self.rest
    }
}

/// How one media fetch of a page's batch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaEnd {
    /// Saved, or already being fetched by another batch.
    Landed,
    /// The host or the writer failed it, or it was cancelled.
    Failed,
    /// Over a size cap.
    Skipped,
}

/// The media a saved HTML page names, fetched before the page opens: the
/// paths still to start, the fetches in flight, and how the ended ones went.
#[derive(Debug)]
pub struct MediaBatch {
    /// Where the page is saved.
    saved: PathBuf,
    /// Whether the page's own media are known; until then it cannot finish.
    scanned: bool,
    queue: VecDeque<String>,
    /// Every path the batch has queued, the page's own among them.
    seen: HashSet<String>,
    /// The paths the page itself names; only a stylesheet among them is
    /// read for media of its own.
    from_page: HashSet<String>,
    /// How many paths entered the queue, for the count cap.
    entered: usize,
    /// Every reference found, skipped ones included.
    found: usize,
    in_flight: usize,
    landed: usize,
    failed: usize,
    skipped: usize,
    bytes_started: u64,
    /// The batch's byte cap was hit: every path added from then on is
    /// skipped.
    capped: bool,
}

impl MediaBatch {
    fn new(page_rel: &str, saved: PathBuf) -> Self {
        Self {
            saved,
            scanned: false,
            queue: VecDeque::new(),
            seen: HashSet::from([page_rel.replace('\\', "/")]),
            from_page: HashSet::new(),
            entered: 0,
            found: 0,
            in_flight: 0,
            landed: 0,
            failed: 0,
            skipped: 0,
            bytes_started: 0,
            capped: false,
        }
    }

    /// Queues the paths in `paths` the batch has not seen, up to the count
    /// cap and while the byte cap is not hit (the rest are skipped), and
    /// counts `skipped` references more.
    fn add(&mut self, paths: Vec<String>, skipped: usize) {
        self.found += skipped;
        self.skipped += skipped;
        for path in paths {
            if !self.seen.insert(path.clone()) {
                continue;
            }
            self.found += 1;
            if !self.capped && self.entered < MAX_MEDIA_PER_PAGE {
                self.entered += 1;
                self.queue.push_back(path);
            } else {
                self.skipped += 1;
            }
        }
    }

    /// The queued paths to start now, keeping at most
    /// [`MEDIA_FETCHES_IN_FLIGHT`] in flight.
    fn next(&mut self) -> Vec<String> {
        let room = MEDIA_FETCHES_IN_FLIGHT.saturating_sub(self.in_flight);
        let started: Vec<String> = self.queue.drain(..room.min(self.queue.len())).collect();
        self.in_flight += started.len();
        started
    }

    /// Whether a started child of `size` bytes fits the caps; one that
    /// would pass the batch's byte cap also skips every path not started.
    fn admit(&mut self, size: u64) -> bool {
        if size > MAX_MEDIA_FILE_BYTES {
            return false;
        }
        if self.bytes_started.saturating_add(size) > MAX_MEDIA_BATCH_BYTES {
            self.capped = true;
            self.skipped += self.queue.len();
            self.queue.clear();
            return false;
        }
        self.bytes_started += size;
        true
    }

    fn end(&mut self, how: MediaEnd) {
        self.in_flight = self.in_flight.saturating_sub(1);
        match how {
            MediaEnd::Landed => self.landed += 1,
            MediaEnd::Failed => self.failed += 1,
            MediaEnd::Skipped => self.skipped += 1,
        }
    }

    /// Whether every reference has ended and the page can open.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.scanned && self.queue.is_empty() && self.in_flight == 0
    }

    /// Where the page is saved.
    #[must_use]
    pub fn saved(&self) -> &Path {
        &self.saved
    }

    /// How many references have ended: landed, failed or skipped.
    #[must_use]
    pub fn ended(&self) -> usize {
        self.landed + self.failed + self.skipped
    }

    /// How many references the page and its stylesheets name, skipped ones
    /// included.
    #[must_use]
    pub fn total(&self) -> usize {
        self.found
    }

    /// How many references failed or were skipped.
    #[must_use]
    pub fn missed(&self) -> usize {
        self.failed + self.skipped
    }
}

/// A fetch in flight.
#[derive(Debug)]
pub struct FetchEntry {
    /// The host whose daemon sends the file.
    host_key: HostKey,
    host: String,
    request: FetchRequest,
    target: PathBuf,
    size: Option<u64>,
    bytes_received: u64,
    resolved_path: Option<String>,
    writer: Option<UnboundedSender<WriteStep>>,
    /// `Done` went to the writer, whose result now decides how it ends.
    finishing: bool,
    /// The link's other readings, when this fetch is one of them.
    group: Option<ProbeGroup>,
    /// Started from the Fetch file… dialog, which shows its progress and
    /// errors in place of toasts.
    popup: bool,
    /// The id of the page whose media this fetch is one of.
    parent: Option<String>,
    /// The page's media, once the page is saved and waits on them.
    batch: Option<MediaBatch>,
    /// The connection closed while this fetch was finishing: it still
    /// saves, but nothing more is asked of the host for it.
    connection_lost: bool,
}

/// `target` as the key two fetches collide on: case-insensitive where the
/// file system is.
fn target_key(target: &Path) -> String {
    let text = target.to_string_lossy();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text.into_owned()
    }
}

impl FetchEntry {
    /// A fetch of `request` from `host_key`'s daemon, whose download folder
    /// is `host`, saved at `target`.
    #[must_use]
    pub fn new(host_key: HostKey, host: String, request: FetchRequest, target: PathBuf) -> Self {
        Self {
            host_key,
            host,
            request,
            target,
            size: None,
            bytes_received: 0,
            resolved_path: None,
            writer: None,
            finishing: false,
            group: None,
            popup: false,
            parent: None,
            batch: None,
            connection_lost: false,
        }
    }

    /// Whether the connection closed while this fetch was finishing.
    #[must_use]
    pub fn connection_lost(&self) -> bool {
        self.connection_lost
    }

    /// The id of the page whose media this fetch is one of.
    #[must_use]
    pub fn parent(&self) -> Option<&str> {
        self.parent.as_deref()
    }

    /// The page's media batch, while the saved page waits on them.
    #[must_use]
    pub fn batch(&self) -> Option<&MediaBatch> {
        self.batch.as_ref()
    }

    /// The page saved at `saved` waits on its media, which are not yet
    /// known.
    pub fn await_media(&mut self, saved: PathBuf) {
        self.batch = Some(MediaBatch::new(&self.request.rel_path, saved));
    }

    /// This fetch as the Fetch file… dialog's, which reports how it goes.
    #[must_use]
    pub fn for_popup(mut self) -> Self {
        self.popup = true;
        self
    }

    /// Whether the Fetch file… dialog reports how this fetch goes.
    #[must_use]
    pub fn reports_to_popup(&self) -> bool {
        self.popup
    }

    /// Whether `Done` went to the writer, whose result now decides how the
    /// fetch ends.
    #[must_use]
    pub fn is_finishing(&self) -> bool {
        self.finishing
    }

    /// This fetch as one reading of `group`'s link.
    #[must_use]
    pub fn in_group(mut self, group: ProbeGroup) -> Self {
        self.group = Some(group);
        self
    }

    /// The link's other readings, when this fetch is one of them.
    #[must_use]
    pub fn group(&self) -> Option<&ProbeGroup> {
        self.group.as_ref()
    }

    /// Notes that `Done` went to the writer: the fetch now ends by the
    /// writer's result, even when the connection goes first.
    pub fn mark_finishing(&mut self) {
        self.finishing = true;
    }

    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The host whose daemon sends the file.
    #[must_use]
    pub fn host_key(&self) -> &HostKey {
        &self.host_key
    }

    #[must_use]
    pub fn request(&self) -> &FetchRequest {
        &self.request
    }

    /// Where the file is saved.
    #[must_use]
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// The file's size the daemon reported; `None` until it started.
    #[must_use]
    pub fn size(&self) -> Option<u64> {
        self.size
    }

    #[must_use]
    pub fn bytes_received(&self) -> u64 {
        self.bytes_received
    }

    /// The host's canonical path of the file, as the daemon sent it.
    #[must_use]
    pub fn resolved_path(&self) -> Option<&str> {
        self.resolved_path.as_deref()
    }

    /// The fetched file's name.
    #[must_use]
    pub fn file_name(&self) -> &str {
        file_name(&self.request.rel_path)
    }

    pub fn on_started(&mut self, size: u64, resolved_path: String) {
        self.size = Some(size);
        self.resolved_path = Some(resolved_path);
    }

    pub fn on_chunk(&mut self, len: u64) {
        self.bytes_received += len;
    }

    /// The channel to the fetch's writer, once the transfer started.
    #[must_use]
    pub fn writer(&self) -> Option<&UnboundedSender<WriteStep>> {
        self.writer.as_ref()
    }

    pub fn set_writer(&mut self, writer: UnboundedSender<WriteStep>) {
        self.writer = Some(writer);
    }

    fn same_target(&self, other: &Self) -> bool {
        target_key(&self.target) == target_key(&other.target)
    }
}

/// A fetch was refused: the same file is already being fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadyFetching;

/// The fetches in flight, by id.
#[derive(Debug)]
pub struct FetchRegistry {
    /// Fixed for the client's run, so no id repeats one a crashed run left
    /// on a part file.
    launch: String,
    next_id: u64,
    next_group: u64,
    entries: HashMap<String, FetchEntry>,
}

impl FetchRegistry {
    /// An empty registry whose ids carry `launch`, a stamp unique to this
    /// run of the client.
    #[must_use]
    pub fn new(launch: &str) -> Self {
        Self {
            launch: launch.to_owned(),
            next_id: 0,
            next_group: 0,
            entries: HashMap::new(),
        }
    }

    /// A new probe group whose readings after the first are `rest`.
    pub fn new_group(&mut self, rest: Vec<FetchRequest>) -> ProbeGroup {
        self.next_group += 1;
        ProbeGroup {
            key: format!("fetch-group-{}-{}", self.launch, self.next_group),
            rest: rest.into(),
        }
    }

    /// The fetch `id` failed: when it is a reading of a probe group that
    /// has not started and readings remain, ends it and returns the next
    /// reading with the group that follows it. Otherwise leaves it in place
    /// for its failure to be reported.
    pub fn next_reading(&mut self, id: &str) -> Option<(FetchRequest, ProbeGroup)> {
        let entry = self.entries.get_mut(id)?;
        if entry.size.is_some() {
            return None;
        }
        let group = entry.group.as_mut()?;
        let next = group.rest.pop_front()?;
        let group = group.clone();
        self.entries.remove(id);
        Some((next, group))
    }

    /// Records `entry` under a new id, unless a fetch to the same target
    /// path is in flight.
    ///
    /// # Errors
    /// [`AlreadyFetching`] when it is.
    pub fn insert(&mut self, entry: FetchEntry) -> Result<String, AlreadyFetching> {
        if self.entries.values().any(|other| other.same_target(&entry)) {
            return Err(AlreadyFetching);
        }
        self.next_id += 1;
        let id = format!("fetch-{}-{}", self.launch, self.next_id);
        self.entries.insert(id.clone(), entry);
        Ok(id)
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&FetchEntry> {
        self.entries.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut FetchEntry> {
        self.entries.get_mut(id)
    }

    /// Ends the fetch `id`, freeing its file for another fetch.
    pub fn remove(&mut self, id: &str) -> Option<FetchEntry> {
        self.entries.remove(id)
    }

    /// Ends every fetch from `host` that is not finishing, and every saved
    /// page from it waiting on its media; a finishing one ends by its
    /// writer's result, marked as having lost its connection. Other hosts'
    /// fetches go on.
    pub fn take_unfinished(&mut self, host: &HostKey) -> Vec<(String, FetchEntry)> {
        let ended = self
            .entries
            .extract_if(|_, entry| {
                entry.host_key == *host && (!entry.finishing || entry.batch.is_some())
            })
            .collect();
        for entry in self.entries.values_mut() {
            if entry.host_key == *host {
                entry.connection_lost = true;
            }
        }
        ended
    }

    fn batch_mut(&mut self, page_id: &str) -> Option<&mut MediaBatch> {
        self.entries.get_mut(page_id)?.batch.as_mut()
    }

    /// The page `page_id`'s own scan found `media` and skipped `skipped`
    /// references: queues them, and lets the batch finish.
    pub fn page_scanned(&mut self, page_id: &str, media: Vec<String>, skipped: usize) {
        if let Some(batch) = self.batch_mut(page_id) {
            batch.from_page.extend(media.iter().cloned());
            batch.add(media, skipped);
            batch.scanned = true;
        }
    }

    /// Whether the page `page_id` itself names `rel`, rather than one of
    /// its stylesheets.
    #[must_use]
    pub fn named_by_page(&self, page_id: &str, rel: &str) -> bool {
        self.entries
            .get(page_id)
            .and_then(FetchEntry::batch)
            .is_some_and(|batch| batch.from_page.contains(rel))
    }

    /// Queues a stylesheet's `media` for the page `page_id`, each path once
    /// per batch, and counts `skipped` references more.
    pub fn add_media(&mut self, page_id: &str, media: Vec<String>, skipped: usize) {
        if let Some(batch) = self.batch_mut(page_id) {
            batch.add(media, skipped);
        }
    }

    /// The requests of the page `page_id`'s media to start now: the page's
    /// repo and worktree, never opened.
    pub fn next_children(&mut self, page_id: &str) -> Vec<FetchRequest> {
        let Some(entry) = self.entries.get_mut(page_id) else {
            return Vec::new();
        };
        let Some(batch) = entry.batch.as_mut() else {
            return Vec::new();
        };
        let page = &entry.request;
        batch
            .next()
            .into_iter()
            .map(|rel_path| FetchRequest {
                host: page.host.clone(),
                repo_id: page.repo_id.clone(),
                repo_name: page.repo_name.clone(),
                worktree_path: page.worktree_path.clone(),
                rel_path,
                line: None,
                column: None,
                open_when_done: false,
            })
            .collect()
    }

    /// Records a fetch of `request` from `host` into `target` as a child of
    /// the page `page_id`. A file another fetch is already writing counts
    /// as landed for this batch, and gives `None`.
    pub fn insert_child(
        &mut self,
        page_id: &str,
        host_key: HostKey,
        host: String,
        request: FetchRequest,
        target: PathBuf,
    ) -> Option<String> {
        let mut entry = FetchEntry::new(host_key, host, request, target);
        entry.parent = Some(page_id.to_owned());
        let inserted = self.insert(entry).ok();
        if inserted.is_none() {
            self.child_ended(page_id, MediaEnd::Landed);
        }
        inserted
    }

    /// Whether a child of the page `page_id` that started at `size` bytes
    /// fits the batch's caps; a page no longer waiting admits it.
    pub fn admit_child(&mut self, page_id: &str, size: u64) -> bool {
        self.batch_mut(page_id)
            .is_none_or(|batch| batch.admit(size))
    }

    /// A child of the page `page_id` ended `how`.
    pub fn child_ended(&mut self, page_id: &str, how: MediaEnd) {
        if let Some(batch) = self.batch_mut(page_id) {
            batch.end(how);
        }
    }

    /// Ends the page `page_id` once every one of its media has ended.
    pub fn take_finished_batch(&mut self, page_id: &str) -> Option<FetchEntry> {
        let finished = self
            .entries
            .get(page_id)?
            .batch
            .as_ref()
            .is_some_and(MediaBatch::is_finished);
        if finished {
            self.entries.remove(page_id)
        } else {
            None
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::channel::mpsc::unbounded;

    use super::{
        FetchEntry, FetchRegistry, FetchRequest, MediaBatch, MediaEnd, WriteOutcome, WriteStep,
        display_host_path, download_path, host_folder, is_under, part_path, run_writer,
        sanitize_segment,
    };
    use crate::html_media::{MAX_MEDIA_FILE_BYTES, MAX_MEDIA_PER_PAGE};
    use crate::target::{HostKey, Target};

    /// A fresh folder under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "rt-native-fetch-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            fs::create_dir_all(&path).expect("create the temp folder");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn chunk(seq: u64, data: &[u8]) -> WriteStep {
        WriteStep::Chunk {
            seq,
            data: data.to_vec(),
        }
    }

    /// Feeds `steps` to a writer for a `size`-byte file at `target` under the
    /// fetch id `id`, then closes the channel, and returns how the writer
    /// ended.
    fn write_sized(target: &Path, id: &str, size: u64, steps: Vec<WriteStep>) -> WriteOutcome {
        let (tx, rx) = unbounded();
        for step in steps {
            tx.unbounded_send(step).expect("the writer listens");
        }
        drop(tx);
        futures::executor::block_on(run_writer(target.to_path_buf(), id.to_owned(), size, rx))
    }

    /// [`write_sized`] with the size the chunks add up to.
    fn write(target: &Path, id: &str, steps: Vec<WriteStep>) -> WriteOutcome {
        let size = steps
            .iter()
            .map(|step| match step {
                WriteStep::Chunk { data, .. } => data.len() as u64,
                WriteStep::Done | WriteStep::Abort => 0,
            })
            .sum();
        write_sized(target, id, size, steps)
    }

    /// The files left in `dir`, by name, sorted.
    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("read the folder")
            .map(|entry| {
                entry
                    .expect("read a folder entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn chunks_reassemble_byte_exact() {
        let dir = TempDir::new("chunks");
        let target = dir.0.join("nested").join("deep").join("data.bin");
        let first: Vec<u8> = (0..=255).collect();
        let second = vec![0u8, 13, 10, 255];
        let third = b"tail".to_vec();

        let outcome = write(
            &target,
            "fetch-1",
            vec![
                chunk(0, &first),
                chunk(1, &second),
                chunk(2, &third),
                WriteStep::Done,
            ],
        );

        assert_eq!(outcome, WriteOutcome::Saved(target.clone()));
        let expected = [first, second, third].concat();
        assert_eq!(fs::read(&target).expect("read the saved file"), expected);
        assert_eq!(
            files_in(target.parent().expect("a parent")),
            ["data.bin"],
            "no part file is left beside the saved file"
        );
    }

    #[test]
    fn an_empty_file_is_saved_empty() {
        let dir = TempDir::new("empty");
        let target = dir.0.join("empty.txt");

        let outcome = write(&target, "fetch-1", vec![WriteStep::Done]);

        assert_eq!(outcome, WriteOutcome::Saved(target.clone()));
        assert_eq!(fs::read(&target).expect("read the saved file"), b"");
    }

    #[test]
    fn the_first_chunk_is_seq_zero() {
        let dir = TempDir::new("first-seq");
        let target = dir.0.join("a.txt");

        let outcome = write(&target, "fetch-1", vec![chunk(1, b"x"), WriteStep::Done]);

        assert!(
            matches!(outcome, WriteOutcome::Failed(_)),
            "the daemon's first chunk is seq 0, so seq 1 first is a gap: {outcome:?}"
        );
    }

    #[test]
    fn a_seq_gap_fails_and_deletes_the_part() {
        let dir = TempDir::new("gap");
        let target = dir.0.join("a.txt");

        let outcome = write(
            &target,
            "fetch-1",
            vec![chunk(0, b"one"), chunk(2, b"three"), WriteStep::Done],
        );

        let WriteOutcome::Failed(reason) = outcome else {
            panic!("a gap is an error, got {outcome:?}");
        };
        assert!(reason.contains("chunk 2"), "names the chunk: {reason}");
        assert!(files_in(&dir.0).is_empty(), "the part file is gone");
    }

    #[test]
    fn a_short_transfer_fails_and_deletes_the_part() {
        let dir = TempDir::new("short");
        let target = dir.0.join("a.txt");

        let outcome = write_sized(
            &target,
            "fetch-1",
            10,
            vec![chunk(0, b"four"), WriteStep::Done],
        );

        let WriteOutcome::Failed(reason) = outcome else {
            panic!("fewer bytes than the size is an error, got {outcome:?}");
        };
        assert!(reason.contains("10"), "names the size: {reason}");
        assert!(
            files_in(&dir.0).is_empty(),
            "nothing is saved and the part is gone"
        );
    }

    #[test]
    fn a_file_that_grew_saves_every_byte() {
        let dir = TempDir::new("grew");
        let target = dir.0.join("a.txt");

        let outcome = write_sized(
            &target,
            "fetch-1",
            2,
            vec![chunk(0, b"ab"), chunk(1, b"cd"), WriteStep::Done],
        );

        assert_eq!(outcome, WriteOutcome::Saved(target.clone()));
        assert_eq!(fs::read(&target).expect("read the saved file"), b"abcd");
    }

    #[test]
    fn an_abort_deletes_the_part() {
        let dir = TempDir::new("abort");
        let target = dir.0.join("a.txt");

        let outcome = write(&target, "fetch-1", vec![chunk(0, b"one"), WriteStep::Abort]);

        assert_eq!(outcome, WriteOutcome::Aborted);
        assert!(files_in(&dir.0).is_empty(), "the part file is gone");
    }

    #[test]
    fn a_closed_channel_deletes_the_part() {
        let dir = TempDir::new("closed");
        let target = dir.0.join("a.txt");

        let outcome = write(&target, "fetch-1", vec![chunk(0, b"one")]);

        assert_eq!(outcome, WriteOutcome::Aborted);
        assert!(files_in(&dir.0).is_empty(), "the part file is gone");
    }

    #[test]
    fn done_replaces_an_existing_file() {
        let dir = TempDir::new("replace");
        let target = dir.0.join("a.txt");
        fs::write(&target, "old contents").expect("write the old file");

        let outcome = write(&target, "fetch-1", vec![chunk(0, b"new"), WriteStep::Done]);

        assert_eq!(outcome, WriteOutcome::Saved(target.clone()));
        assert_eq!(fs::read(&target).expect("read the saved file"), b"new");
    }

    #[test]
    fn a_failed_rename_fails_and_deletes_the_part() {
        let dir = TempDir::new("rename");
        let target = dir.0.join("taken");
        fs::create_dir_all(target.join("inside")).expect("a folder holds the target's name");

        let outcome = write(&target, "fetch-1", vec![chunk(0, b"x"), WriteStep::Done]);

        assert!(
            matches!(outcome, WriteOutcome::Failed(_)),
            "a folder in the way fails the rename: {outcome:?}"
        );
        assert_eq!(files_in(&dir.0), ["taken"], "the part file is gone");
    }

    #[test]
    fn a_real_file_at_the_part_path_survives() {
        let dir = TempDir::new("taken-part");
        let target = dir.0.join("a.txt");
        let part = part_path(&target, "fetch-1");
        fs::write(&part, "a real file").expect("write the file in the way");

        let outcome = write(&target, "fetch-1", vec![chunk(0, b"x"), WriteStep::Done]);

        assert!(
            matches!(outcome, WriteOutcome::Failed(_)),
            "a file in the part's place fails the fetch: {outcome:?}"
        );
        assert_eq!(
            fs::read_to_string(&part).expect("the file in the way is still there"),
            "a real file"
        );
        assert!(!target.exists(), "nothing is saved");
    }

    #[test]
    fn a_long_target_path_is_written_and_renamed() {
        let dir = TempDir::new("long");
        let segment = "d".repeat(60);
        let target = dir
            .0
            .join(&segment)
            .join(&segment)
            .join(&segment)
            .join(&segment)
            .join(format!("{segment}.txt"));
        assert!(target.as_os_str().len() > 260, "the path is long");

        let outcome = write(&target, "fetch-1", vec![chunk(0, b"long"), WriteStep::Done]);

        assert_eq!(outcome, WriteOutcome::Saved(target.clone()));
        assert_eq!(fs::read(&target).expect("read the saved file"), b"long");
    }

    #[test]
    fn two_fetches_write_different_parts() {
        let target = Path::new("root").join("a.txt");
        let first = part_path(&target, "fetch-ab12-1");
        let second = part_path(&target, "fetch-ab12-2");

        assert_ne!(first, second);
        assert_eq!(first, Path::new("root").join(".fetch-ab12-1.part"));
    }

    fn entry(host: &str, rel: &str) -> FetchEntry {
        let request = FetchRequest {
            host: HostKey::Local,
            repo_id: "r1".to_owned(),
            repo_name: "repo".to_owned(),
            worktree_path: None,
            rel_path: rel.to_owned(),
            line: None,
            column: None,
            open_when_done: false,
        };
        let target = download_path(Path::new("root"), host, "repo", rel).expect("a valid path");
        FetchEntry::new(HostKey::Local, host.to_owned(), request, target)
    }

    #[test]
    fn a_popup_entry_reports_to_the_popup() {
        assert!(!entry("host", "a.txt").reports_to_popup());
        let mut registry = FetchRegistry::new("t");
        let id = registry
            .insert(entry("host", "a.txt").for_popup())
            .expect("starts");
        assert!(
            registry.get(&id).expect("recorded").reports_to_popup(),
            "the flag survives the registry"
        );
    }

    #[test]
    fn fetch_ids_differ_between_launches() {
        let first = FetchRegistry::new("aaaa")
            .insert(entry("host", "a.txt"))
            .expect("starts");
        let second = FetchRegistry::new("bbbb")
            .insert(entry("host", "a.txt"))
            .expect("starts");

        assert_ne!(
            first, second,
            "a part file a crashed run left never blocks a new fetch"
        );
    }

    #[test]
    fn one_fetch_per_target_whatever_the_separator() {
        let mut registry = FetchRegistry::new("t");
        registry.insert(entry("host", "src/a.txt")).expect("starts");
        assert!(registry.insert(entry("host", "src\\a.txt")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn one_fetch_per_target_whatever_the_case_on_windows() {
        let mut registry = FetchRegistry::new("t");
        registry.insert(entry("host", "src/a.txt")).expect("starts");
        assert!(registry.insert(entry("host", "SRC/A.TXT")).is_err());
    }

    #[test]
    fn one_fetch_per_target_after_sanitising() {
        let mut registry = FetchRegistry::new("t");
        registry.insert(entry("host", "a?b")).expect("starts");
        assert!(
            registry.insert(entry("host", "a*b")).is_err(),
            "both land at a_b"
        );
    }

    #[test]
    fn a_finishing_fetch_survives_a_disconnect() {
        let mut registry = FetchRegistry::new("t");
        let finishing = registry.insert(entry("host", "a.txt")).expect("starts");
        let running = registry.insert(entry("host", "b.txt")).expect("starts");
        registry
            .get_mut(&finishing)
            .expect("recorded")
            .mark_finishing();

        let ended: Vec<String> = registry
            .take_unfinished(&HostKey::Local)
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        assert_eq!(ended, [running]);
        assert!(registry.get(&finishing).is_some(), "the writer decides it");
    }

    #[test]
    fn a_hosts_disconnect_ends_only_its_own_fetches() {
        let remote = HostKey::Remote("box".to_owned());
        let mut registry = FetchRegistry::new("t");
        let local = registry.insert(entry("host", "a.txt")).expect("starts");
        let theirs = registry
            .insert(FetchEntry::new(
                remote.clone(),
                "box".to_owned(),
                entry("box", "b.txt").request().clone(),
                PathBuf::from("box/b.txt"),
            ))
            .expect("starts");

        let ended: Vec<String> = registry
            .take_unfinished(&remote)
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        assert_eq!(ended, [theirs]);
        let kept = registry.get(&local).expect("This PC's fetch goes on");
        assert!(!kept.connection_lost(), "its connection still stands");
    }

    #[test]
    fn one_fetch_per_file_at_a_time() {
        let mut registry = FetchRegistry::new("t");
        let first = registry
            .insert(entry("host", "src/a.txt"))
            .expect("the first fetch starts");

        assert!(
            registry.insert(entry("host", "src/a.txt")).is_err(),
            "a second fetch of the same file is refused"
        );
        let other = registry
            .insert(entry("host", "src/b.txt"))
            .expect("another file fetches alongside");
        assert_ne!(first, other, "every fetch has its own id");
        assert!(
            registry.insert(entry("other-host", "src/a.txt")).is_ok(),
            "the same path on another host is another file"
        );

        registry.remove(&first);
        assert!(
            registry.insert(entry("host", "src/a.txt")).is_ok(),
            "the file fetches again once the first fetch ended"
        );
    }

    fn requests(rels: &[&str]) -> Vec<FetchRequest> {
        rels.iter().map(|rel| entry("host", rel).request).collect()
    }

    #[test]
    fn a_probe_group_advances_on_a_pre_start_error() {
        let mut registry = FetchRegistry::new("t");
        let group = registry.new_group(requests(&["b.txt", "c.txt"]));
        let key = group.key().to_owned();
        let id = registry
            .insert(entry("host", "a.txt").in_group(group))
            .expect("starts");

        let (next, group) = registry
            .next_reading(&id)
            .expect("a reading that never started moves on to the next");

        assert_eq!(next.rel_path, "b.txt");
        assert_eq!(group.key(), key, "the group keeps its toast");
        assert_eq!(
            group.remaining().iter().cloned().collect::<Vec<_>>(),
            requests(&["c.txt"])
        );
        assert!(registry.get(&id).is_none(), "the missed reading ends");
    }

    #[test]
    fn a_started_fetch_ends_the_group() {
        let mut registry = FetchRegistry::new("t");
        let group = registry.new_group(requests(&["b.txt"]));
        let id = registry
            .insert(entry("host", "a.txt").in_group(group))
            .expect("starts");
        registry
            .get_mut(&id)
            .expect("recorded")
            .on_started(4, r"C:\repo\a.txt".to_owned());

        assert_eq!(
            registry.next_reading(&id),
            None,
            "a reading the host started is the link's file"
        );
        assert!(registry.get(&id).is_some(), "its failure is reported");
    }

    #[test]
    fn the_last_failed_reading_ends_the_group_with_one_error() {
        let mut registry = FetchRegistry::new("t");
        let group = registry.new_group(requests(&["b.txt"]));
        let id = registry
            .insert(entry("host", "a.txt").in_group(group))
            .expect("starts");
        let (next, group) = registry.next_reading(&id).expect("moves on");
        let last = registry
            .insert(
                FetchEntry::new(
                    HostKey::Local,
                    "host".to_owned(),
                    next,
                    PathBuf::from("b.txt"),
                )
                .in_group(group),
            )
            .expect("the next reading starts");

        assert_eq!(registry.next_reading(&last), None, "no reading is left");
        assert!(
            registry.get(&last).is_some(),
            "the last reading stays for its one failure to be reported"
        );
        assert!(registry.next_reading("fetch-t-unknown").is_none());
    }

    #[test]
    fn the_registry_counts_bytes_against_the_size() {
        let mut registry = FetchRegistry::new("t");
        let id = registry.insert(entry("host", "a.txt")).expect("starts");
        let fetch = registry.get_mut(&id).expect("recorded");
        assert_eq!((fetch.bytes_received(), fetch.size()), (0, None));

        fetch.on_started(10, r"\\?\C:\repo\a.txt".to_owned());
        fetch.on_chunk(4);
        fetch.on_chunk(3);

        assert_eq!((fetch.bytes_received(), fetch.size()), (7, Some(10)));
    }

    const PAGE: &str = "docs/page.html";

    /// A saved page `PAGE` whose scan found `media` and skipped `skipped`;
    /// returns its id.
    fn scanned_page(registry: &mut FetchRegistry, media: &[&str], skipped: usize) -> String {
        let id = registry.insert(entry("host", PAGE)).expect("starts");
        let page = registry.get_mut(&id).expect("recorded");
        page.mark_finishing();
        page.await_media(PathBuf::from("root").join(PAGE));
        let media = media.iter().map(|&rel| rel.to_owned()).collect();
        registry.page_scanned(&id, media, skipped);
        id
    }

    fn rels(requests: &[FetchRequest]) -> Vec<&str> {
        requests
            .iter()
            .map(|request| request.rel_path.as_str())
            .collect()
    }

    fn batch<'a>(registry: &'a FetchRegistry, id: &str) -> &'a MediaBatch {
        registry
            .get(id)
            .and_then(FetchEntry::batch)
            .expect("the page waits on its media")
    }

    /// Starts every child the batch lets start, admitting each at `size`
    /// and ending it as admitted or skipped, until none is left; returns
    /// how many started.
    fn run_children(registry: &mut FetchRegistry, id: &str, size: u64) -> usize {
        let mut started = 0;
        loop {
            let children = registry.next_children(id);
            if children.is_empty() {
                return started;
            }
            for _ in &children {
                started += 1;
                let end = if registry.admit_child(id, size) {
                    MediaEnd::Landed
                } else {
                    MediaEnd::Skipped
                };
                registry.child_ended(id, end);
            }
        }
    }

    #[test]
    fn a_batch_starts_at_most_four_children() {
        let mut registry = FetchRegistry::new("t");
        let id = scanned_page(
            &mut registry,
            &["a.png", "b.png", "c.png", "d.png", "e.png", "f.png"],
            0,
        );

        let first = registry.next_children(&id);
        assert_eq!(rels(&first), ["a.png", "b.png", "c.png", "d.png"]);
        assert!(
            first.iter().all(|child| !child.open_when_done
                && child.repo_id == "r1"
                && child.line.is_none()),
            "a child is the page's repo and never opens: {first:?}"
        );
        assert!(registry.next_children(&id).is_empty(), "four are in flight");

        let child = first[0].clone();
        let target = download_path(Path::new("root"), "host", "repo", &child.rel_path)
            .expect("a valid path");
        let child_id = registry
            .insert_child(&id, HostKey::Local, "host".to_owned(), child, target)
            .expect("the child starts");
        assert_eq!(
            registry.get(&child_id).and_then(FetchEntry::parent),
            Some(id.as_str())
        );

        registry.child_ended(&id, MediaEnd::Landed);
        assert_eq!(rels(&registry.next_children(&id)), ["e.png"]);
    }

    #[test]
    fn a_batch_finishes_when_every_child_ends() {
        let mut registry = FetchRegistry::new("t");
        let waiting = registry.insert(entry("host", PAGE)).expect("starts");
        registry
            .get_mut(&waiting)
            .expect("recorded")
            .await_media(PathBuf::from("page.html"));
        assert!(
            registry.take_finished_batch(&waiting).is_none(),
            "a page not yet scanned waits"
        );

        let mut registry = FetchRegistry::new("t");
        let id = scanned_page(&mut registry, &["a.png", "b.png"], 0);
        assert_eq!(registry.next_children(&id).len(), 2);
        assert!(registry.take_finished_batch(&id).is_none());
        registry.child_ended(&id, MediaEnd::Landed);
        assert!(
            registry.take_finished_batch(&id).is_none(),
            "one is in flight"
        );
        registry.child_ended(&id, MediaEnd::Failed);

        let page = registry
            .take_finished_batch(&id)
            .expect("every child ended");
        let done = page.batch().expect("the page's batch");
        assert_eq!((done.ended(), done.total(), done.missed()), (2, 2, 1));
        assert_eq!(done.saved(), Path::new("root").join(PAGE));
        assert!(registry.get(&id).is_none(), "the page's fetch ended");
    }

    #[test]
    fn a_shared_child_counts_as_done() {
        let mut registry = FetchRegistry::new("t");
        registry
            .insert(entry("host", "docs/x.png"))
            .expect("another page's batch fetches it");
        let id = scanned_page(&mut registry, &["docs/x.png"], 0);

        let [child] = registry.next_children(&id).try_into().expect("one child");
        let target = download_path(Path::new("root"), "host", "repo", &child.rel_path)
            .expect("a valid path");
        assert_eq!(
            registry.insert_child(&id, HostKey::Local, "host".to_owned(), child, target),
            None,
            "the file is already fetching"
        );

        let page = registry
            .take_finished_batch(&id)
            .expect("the shared child counts as done");
        let done = page.batch().expect("the page's batch");
        assert_eq!((done.ended(), done.missed()), (1, 0));
    }

    #[test]
    fn references_past_the_count_cap_are_skipped() {
        let names: Vec<String> = (0..MAX_MEDIA_PER_PAGE + 5)
            .map(|n| format!("img/{n}.png"))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut registry = FetchRegistry::new("t");
        let id = scanned_page(&mut registry, &refs, 2);

        let counts = batch(&registry, &id);
        assert_eq!(counts.total(), MAX_MEDIA_PER_PAGE + 7);
        assert_eq!((counts.ended(), counts.missed()), (7, 7));

        assert_eq!(run_children(&mut registry, &id, 1), MAX_MEDIA_PER_PAGE);
        let page = registry.take_finished_batch(&id).expect("finished");
        assert_eq!(page.batch().expect("the batch").missed(), 7);
    }

    #[test]
    fn a_child_over_the_size_cap_is_skipped() {
        let mut registry = FetchRegistry::new("t");
        let id = scanned_page(&mut registry, &["big.mp4", "fits.mp4"], 0);
        assert_eq!(registry.next_children(&id).len(), 2);

        assert!(!registry.admit_child(&id, MAX_MEDIA_FILE_BYTES + 1));
        registry.child_ended(&id, MediaEnd::Skipped);
        assert!(registry.admit_child(&id, MAX_MEDIA_FILE_BYTES));
        registry.child_ended(&id, MediaEnd::Landed);

        let page = registry.take_finished_batch(&id).expect("finished");
        let done = page.batch().expect("the batch");
        assert_eq!((done.ended(), done.missed()), (2, 1));
    }

    #[test]
    fn the_batch_byte_cap_skips_the_rest() {
        let names: Vec<String> = (0..16).map(|n| format!("{n}.mp4")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut registry = FetchRegistry::new("t");
        let id = scanned_page(&mut registry, &refs, 0);

        let started = run_children(&mut registry, &id, MAX_MEDIA_FILE_BYTES);

        assert_eq!(
            started, 12,
            "ten fit in the batch, the two then in flight are cancelled and the rest never start"
        );
        let page = registry.take_finished_batch(&id).expect("finished");
        let done = page.batch().expect("the batch");
        assert_eq!((done.ended(), done.total(), done.missed()), (16, 16, 6));
    }

    #[test]
    fn a_capped_batch_skips_media_a_later_stylesheet_names() {
        let names: Vec<String> = (0..11).map(|n| format!("{n}.mp4")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut registry = FetchRegistry::new("t");
        let id = scanned_page(&mut registry, &refs, 0);
        assert_eq!(
            run_children(&mut registry, &id, MAX_MEDIA_FILE_BYTES),
            11,
            "the eleventh passes the batch's byte cap"
        );

        registry.add_media(&id, vec!["late.png".to_owned()], 0);

        assert!(
            registry.next_children(&id).is_empty(),
            "a full batch starts nothing more"
        );
        let page = registry.take_finished_batch(&id).expect("finished");
        let done = page.batch().expect("the batch");
        assert_eq!((done.total(), done.missed()), (12, 2));
    }

    #[test]
    fn a_stylesheets_media_are_queued_once() {
        let mut registry = FetchRegistry::new("t");
        let id = scanned_page(&mut registry, &["docs/site.css", "docs/bg.png"], 0);
        assert_eq!(registry.next_children(&id).len(), 2);

        registry.add_media(
            &id,
            vec![
                "docs/bg.png".to_owned(),
                "docs/font.woff2".to_owned(),
                PAGE.to_owned(),
            ],
            1,
        );
        assert_eq!(rels(&registry.next_children(&id)), ["docs/font.woff2"]);
        registry.add_media(&id, vec!["docs/font.woff2".to_owned()], 0);
        assert!(
            registry.next_children(&id).is_empty(),
            "a file is fetched once per batch"
        );
        assert_eq!(batch(&registry, &id).total(), 4);
    }

    #[test]
    fn a_nested_path_lands_under_host_and_repo() {
        let root = Path::new("root");
        let path = download_path(root, "box.ts.net", "repo", "src/sub\\main.rs")
            .expect("a nested relative path is valid");
        assert_eq!(
            path,
            root.join("box.ts.net")
                .join("repo")
                .join("src")
                .join("sub")
                .join("main.rs")
        );
    }

    /// `rel` is refused, with a reason that mentions `why`.
    fn refused(rel: &str, why: &str) {
        let err = download_path(Path::new("root"), "host", "repo", rel)
            .expect_err(&format!("{rel:?} must be refused"));
        assert!(err.contains(why), "{rel:?}: {err}");
    }

    /// Where `rel` lands under `root/host/repo`.
    fn lands_at(rel: &str) -> PathBuf {
        let base = Path::new("root").join("host").join("repo");
        download_path(Path::new("root"), "host", "repo", rel)
            .expect("the path is made safe, not refused")
            .strip_prefix(&base)
            .expect("lands under the repo's folder")
            .to_path_buf()
    }

    #[test]
    fn an_empty_path_is_refused() {
        refused("", "empty");
        refused("a//b", "empty");
    }

    #[test]
    fn a_parent_segment_is_refused() {
        refused("a/../b", "..");
    }

    #[test]
    fn a_current_segment_is_refused() {
        refused("./a", "\".\"");
    }

    #[test]
    fn a_drive_prefix_is_refused() {
        refused("C:/a.txt", "drive");
        refused("c:a.txt", "drive");
    }

    #[test]
    fn a_leading_separator_is_refused() {
        refused("/etc/passwd", "separator");
        refused("\\a.txt", "separator");
    }

    #[test]
    fn a_forbidden_character_becomes_an_underscore() {
        assert_eq!(
            lands_at("dir/a<b>c:d\"e|f?g*h"),
            Path::new("dir").join("a_b_c_d_e_f_g_h")
        );
    }

    #[test]
    fn a_control_character_becomes_an_underscore() {
        assert_eq!(lands_at("a\u{7}b\u{1f}.txt"), Path::new("a_b_.txt"));
    }

    #[test]
    fn a_trailing_dot_or_space_becomes_an_underscore() {
        assert_eq!(lands_at("dir./name "), Path::new("dir_").join("name_"));
    }

    #[test]
    fn a_reserved_name_gets_a_leading_underscore() {
        assert_eq!(lands_at("CON"), Path::new("_CON"));
        assert_eq!(lands_at("src/aux.c"), Path::new("src").join("_aux.c"));
        assert_eq!(lands_at("com1.txt"), Path::new("_com1.txt"));
        assert_eq!(lands_at("Lpt9.tar.gz"), Path::new("_Lpt9.tar.gz"));
        assert_eq!(
            lands_at("console/com10.txt"),
            Path::new("console").join("com10.txt"),
            "only the exact reserved names change"
        );
    }

    #[test]
    fn console_and_superscript_device_names_are_reserved() {
        assert_eq!(lands_at("CONOUT$"), Path::new("_CONOUT$"));
        assert_eq!(lands_at("conin$.log"), Path::new("_conin$.log"));
        assert_eq!(lands_at("COM\u{b9}.txt"), Path::new("_COM\u{b9}.txt"));
        assert_eq!(lands_at("lpt\u{b3}"), Path::new("_lpt\u{b3}"));
    }

    #[test]
    fn a_reserved_stem_with_trailing_spaces_is_reserved() {
        assert_eq!(lands_at("NUL .txt"), Path::new("_NUL .txt"));
    }

    #[test]
    fn host_and_repo_segments_are_sanitised() {
        assert_eq!(sanitize_segment("my:repo<1>"), "my_repo_1_");
        assert_eq!(sanitize_segment("a\"b|c?d*e\u{1}"), "a_b_c_d_e_");
        assert_eq!(sanitize_segment("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_segment("name."), "name_");
        assert_eq!(sanitize_segment(".."), "_");
        assert_eq!(sanitize_segment(""), "_");
        assert_eq!(sanitize_segment("PRN"), "_PRN");
        let path = download_path(Path::new("root"), "h:1", "my|repo", "a.txt").expect("valid");
        assert_eq!(
            path,
            Path::new("root").join("h_1").join("my_repo").join("a.txt")
        );
    }

    #[test]
    fn a_tailnet_host_folder_is_its_lowercased_name() {
        assert_eq!(
            host_folder(&Target::tailnet("Box.Tail1234.TS.net.", None)).as_deref(),
            Some("box.tail1234.ts.net")
        );
    }

    #[test]
    fn an_ssh_host_folder_is_its_sanitised_name() {
        let ssh = |name: &str| Target::parse_ssh(name, None).expect("an SSH host");
        assert_eq!(host_folder(&ssh("devbox")).as_deref(), Some("ssh-devbox"));
        assert_eq!(
            host_folder(&ssh("Me@Box:2222")).as_deref(),
            Some("ssh-me@box_2222")
        );
        let name = "box.tail1234.ts.net";
        assert_ne!(
            host_folder(&ssh(name)),
            host_folder(&Target::tailnet(name, None)),
            "an SSH host never shares a tailnet host's folder"
        );
    }

    #[test]
    fn the_local_daemon_has_no_download_folder() {
        assert_eq!(host_folder(&Target::Local), None);
    }

    #[test]
    fn is_under_is_component_wise() {
        let root = Path::new("data").join("remote-files");
        assert!(is_under(&root, &root.join("host").join("a.txt")));
        assert!(
            !is_under(&root, &root),
            "the root itself is not a fetched file"
        );
        assert!(
            !is_under(
                &root,
                &Path::new("data").join("remote-files2").join("a.txt")
            ),
            "a sibling sharing the root's prefix is not under it"
        );
        assert!(
            !is_under(
                &root,
                &root.join("host").join("..").join("..").join("x.txt")
            ),
            "a .. escape is not under the root"
        );
        assert!(!is_under(
            &root,
            Path::new("elsewhere").join("a.txt").as_path()
        ));
    }

    #[test]
    fn verbatim_prefixes_are_stripped_for_display() {
        assert_eq!(display_host_path(r"\\?\C:\repo\a.txt"), r"C:\repo\a.txt");
        assert_eq!(
            display_host_path(r"\\?\UNC\server\share\a.txt"),
            r"\\server\share\a.txt"
        );
        assert_eq!(display_host_path("/home/me/a.txt"), "/home/me/a.txt");
        assert_eq!(display_host_path(r"C:\a.txt"), r"C:\a.txt");
    }
}
