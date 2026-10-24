//! Files fetched from a remote host onto this PC: where each one lands, the
//! part file a transfer writes into before it takes the file's place, and the
//! transfers in flight.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::open::OpenJob;
use crate::target::Target;

/// A file under a repo root or one of its worktrees on the remote host, to
/// fetch onto this PC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
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
/// name, lower-cased; `None` for the local daemon, whose files are already
/// on this PC.
#[must_use]
pub fn host_folder(target: &Target) -> Option<String> {
    target
        .tailnet_host()
        .map(|host| sanitize_segment(&host.name().to_lowercase()))
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

/// A fetch in flight.
#[derive(Debug)]
pub struct FetchEntry {
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
    /// A fetch of `request` from `host`, saved at `target`.
    #[must_use]
    pub fn new(host: String, request: FetchRequest, target: PathBuf) -> Self {
        Self {
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
        }
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

    /// Ends every fetch that is not finishing; a finishing one ends by its
    /// writer's result.
    pub fn take_unfinished(&mut self) -> Vec<(String, FetchEntry)> {
        self.entries
            .extract_if(|_, entry| !entry.finishing)
            .collect()
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
        FetchEntry, FetchRegistry, FetchRequest, WriteOutcome, WriteStep, display_host_path,
        download_path, host_folder, is_under, part_path, run_writer, sanitize_segment,
    };
    use crate::target::Target;

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
            repo_id: "r1".to_owned(),
            repo_name: "repo".to_owned(),
            worktree_path: None,
            rel_path: rel.to_owned(),
            line: None,
            column: None,
            open_when_done: false,
        };
        let target = download_path(Path::new("root"), host, "repo", rel).expect("a valid path");
        FetchEntry::new(host.to_owned(), request, target)
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
            .take_unfinished()
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        assert_eq!(ended, [running]);
        assert!(registry.get(&finishing).is_some(), "the writer decides it");
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
                FetchEntry::new("host".to_owned(), next, PathBuf::from("b.txt")).in_group(group),
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
