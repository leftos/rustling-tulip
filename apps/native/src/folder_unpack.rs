//! A fetched folder's stream: a tar archive inside a zstd frame, read from
//! the fetch's steps and unpacked into a staging folder beside the target,
//! which is swapped into place once the stream ends.

use std::cell::Cell;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc::Receiver;

use tar::{Archive, EntryType};

use crate::file_fetch::{WriteOutcome, WriteStep, is_under, part_path, rel_segments};

/// How much of the decompressed stream one read takes, and how many bytes of
/// a file pass between progress reports.
const BUFFER: usize = 64 * 1024;
const PROGRESS_EVERY: u64 = 1 << 20;

/// One tar block, the two blocks of an archive's end marker, and the most
/// zero padding an archive may carry past the marker.
const BLOCK: u64 = 512;
const END_MARKER: u64 = 2 * BLOCK;
const PADDING_LIMIT: u64 = 1 << 20;

/// What the host announced before the stream: the files and bytes it will send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Announced {
    pub files: u64,
    pub bytes: u64,
}

/// Where the step stream stands once its chunks run out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamStatus {
    /// The stream has not ended.
    Open,
    /// The host sent `Done`.
    Done,
    /// The fetch was aborted, or its channel closed.
    Aborted,
}

/// How much of a fetch has been unpacked so far.
#[derive(Debug, Default)]
struct Written {
    files: u64,
    bytes: u64,
}

/// How an unpacking ended early.
enum Ending {
    /// The fetch was aborted; the stage is deleted.
    Aborted,
    /// Why the folder was not unpacked; the stage is deleted.
    Failed(String),
}

/// Unpacks the fetch `id`'s stream into a staging folder beside `target`,
/// then swaps it over `target`. Blocks on the disk and on `steps`, so the
/// caller runs it on a thread of its own.
pub(crate) fn run_unpacker(
    target: &Path,
    id: &str,
    announced: Announced,
    steps: Receiver<WriteStep>,
    progress: &mut dyn FnMut(u64),
) -> WriteOutcome {
    let Some(parent) = target.parent() else {
        return WriteOutcome::Failed(format!("{} has no folder", target.display()));
    };
    if let Err(err) = fs::create_dir_all(parent) {
        return WriteOutcome::Failed(format!("creating {}: {err}", parent.display()));
    }
    let stage = part_path(target, id);
    if let Err(err) = fs::create_dir(&stage) {
        return WriteOutcome::Failed(format!("creating {}: {err}", stage.display()));
    }
    let aside = target.with_file_name(format!(".{id}.old"));
    let mut unpacker = Unpacker {
        target,
        stage: &stage,
        aside: &aside,
        announced,
        seen: Written::default(),
        written: Written::default(),
        progress,
        status: Rc::new(Cell::new(StreamStatus::Open)),
    };
    match unpacker.run(steps) {
        Ok(()) => WriteOutcome::Saved(target.to_path_buf()),
        Err(Ending::Aborted) => {
            remove_stage(&stage);
            WriteOutcome::Aborted
        }
        Err(Ending::Failed(reason)) => {
            remove_stage(&stage);
            WriteOutcome::Failed(reason)
        }
    }
}

/// One folder's unpacking, from its stage to its place.
struct Unpacker<'a> {
    target: &'a Path,
    stage: &'a Path,
    aside: &'a Path,
    announced: Announced,
    /// Every entry the stream carries, whatever its type.
    seen: Written,
    /// The files and bytes actually unpacked.
    written: Written,
    progress: &'a mut dyn FnMut(u64),
    status: Rc<Cell<StreamStatus>>,
}

impl Unpacker<'_> {
    /// Reads the stream into the stage and swaps the stage over the target.
    fn run(&mut self, steps: Receiver<WriteStep>) -> Result<(), Ending> {
        let eof = Rc::new(Cell::new(false));
        let reader = ChunkReader {
            steps,
            next_seq: 0,
            pending: Vec::new(),
            pos: 0,
            status: Rc::clone(&self.status),
            eof: Rc::clone(&eof),
        };
        let decoder = zstd::stream::read::Decoder::new(reader)
            .map_err(|err| Ending::Failed(format!("opening the folder stream: {err}")))?;
        let mut archive = Archive::new(Counting::new(decoder));
        {
            let entries = archive
                .entries()
                .map_err(|err| ending(&self.status, &eof, &err))?;
            for entry in entries {
                let mut entry = entry.map_err(|err| ending(&self.status, &eof, &err))?;
                self.entry(&mut entry, &eof)?;
            }
        }
        let mut rest = archive.into_inner();
        // The block the archive consumed to find no further header is the end
        // marker's first block; the rest of the marker, and no more than a
        // mebibyte of padding past it, must follow.
        let marker = rest.zeros.min(BLOCK);
        let mut padding = 0u64;
        let mut tail = vec![0u8; BUFFER];
        loop {
            let read = rest
                .read(&mut tail)
                .map_err(|err| ending(&self.status, &eof, &err))?;
            if read == 0 {
                break;
            }
            if tail[..read].iter().any(|&byte| byte != 0) {
                return Err(Ending::Failed(
                    "the folder stream has data after the archive's end".to_owned(),
                ));
            }
            padding += read as u64;
            if marker + padding > END_MARKER + PADDING_LIMIT {
                return Err(Ending::Failed(
                    "the folder stream has more than a mebibyte of padding after its end"
                        .to_owned(),
                ));
            }
        }
        if marker + padding < END_MARKER {
            return Err(Ending::Failed("the folder stream ended early".to_owned()));
        }
        if self.written.files < self.announced.files || self.written.bytes < self.announced.bytes {
            tracing::debug!(
                files = self.written.files,
                announced_files = self.announced.files,
                bytes = self.written.bytes,
                announced_bytes = self.announced.bytes,
                "the folder stream sent fewer files or bytes than it announced"
            );
        }
        swap(self.stage, self.target, self.aside)
    }

    /// Writes one entry of the archive into the stage, or skips it.
    fn entry<R: Read>(
        &mut self,
        entry: &mut tar::Entry<'_, R>,
        eof: &Cell<bool>,
    ) -> Result<(), Ending> {
        self.seen.files += 1;
        if self.seen.files > self.announced.files {
            return Err(Ending::Failed(format!(
                "the folder stream has more files than the {} announced",
                self.announced.files
            )));
        }
        self.seen.bytes += entry.size();
        if self.seen.bytes > self.announced.bytes {
            return Err(Ending::Failed(format!(
                "the folder stream has more bytes than the {} announced",
                self.announced.bytes
            )));
        }
        let kind = entry.header().entry_type();
        let is_directory = kind == EntryType::Directory;
        let is_file = kind == EntryType::Regular || kind == EntryType::Continuous;
        if !is_directory && !is_file {
            return Ok(());
        }
        let name = entry.path_bytes();
        let name = String::from_utf8(name.into_owned())
            .map_err(|_| Ending::Failed("an entry's name is not text".to_owned()))?;
        let name = if is_directory {
            name.strip_suffix('/').unwrap_or(&name)
        } else {
            name.as_str()
        };
        let segments =
            rel_segments(name).map_err(|err| Ending::Failed(format!("{name}: {err}")))?;
        let mut path = self.stage.to_path_buf();
        path.extend(segments);
        if !is_under(self.stage, &path) {
            return Err(Ending::Failed(format!(
                "{name}: the name is outside the folder"
            )));
        }
        if is_directory {
            fs::create_dir_all(&path)
                .map_err(|err| Ending::Failed(format!("creating {}: {err}", path.display())))?;
            return Ok(());
        }
        self.file(entry, &path, name, eof)
    }

    /// Writes one regular file entry into the stage, counting what it adds.
    fn file<R: Read>(
        &mut self,
        entry: &mut tar::Entry<'_, R>,
        path: &Path,
        name: &str,
        eof: &Cell<bool>,
    ) -> Result<(), Ending> {
        let Ok(mut left) = usize::try_from(entry.size()) else {
            return Err(Ending::Failed(format!("{name}: the entry is too large")));
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| Ending::Failed(format!("creating {}: {err}", parent.display())))?;
        }
        let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                return Err(Ending::Failed(format!(
                    "{name}: another entry unpacks to this name"
                )));
            }
            Err(err) => {
                return Err(Ending::Failed(format!(
                    "creating {}: {err}",
                    path.display()
                )));
            }
        };
        let mut since = 0u64;
        let mut buffer = vec![0u8; BUFFER];
        while left > 0 {
            let read = entry
                .read(&mut buffer[..left.min(BUFFER)])
                .map_err(|err| ending(&self.status, eof, &err))?;
            if read == 0 {
                return Err(Ending::Failed("the folder stream ended early".to_owned()));
            }
            file.write_all(&buffer[..read])
                .map_err(|err| Ending::Failed(format!("writing {}: {err}", path.display())))?;
            self.written.bytes += read as u64;
            left -= read;
            since += read as u64;
            if since >= PROGRESS_EVERY {
                (self.progress)(self.written.bytes);
                since = 0;
            }
        }
        drop(file);
        self.written.files += 1;
        (self.progress)(self.written.bytes);
        Ok(())
    }
}

/// The fetch's steps as one `Read`: chunks in sequence, `Done` as the end.
struct ChunkReader {
    steps: Receiver<WriteStep>,
    next_seq: u64,
    pending: Vec<u8>,
    pos: usize,
    status: Rc<Cell<StreamStatus>>,
    /// Whether the reader last told its consumer the stream had ended.
    eof: Rc<Cell<bool>>,
}

impl Read for ChunkReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.pos < self.pending.len() {
                let take = out.len().min(self.pending.len() - self.pos);
                out[..take].copy_from_slice(&self.pending[self.pos..self.pos + take]);
                self.pos += take;
                self.eof.set(false);
                return Ok(take);
            }
            match self.status.get() {
                StreamStatus::Done => {
                    self.eof.set(true);
                    return Ok(0);
                }
                StreamStatus::Aborted => {
                    return Err(io::Error::other("the folder fetch was aborted"));
                }
                StreamStatus::Open => {}
            }
            match self.steps.recv() {
                Ok(WriteStep::Chunk { seq, data }) => {
                    if seq != self.next_seq {
                        return Err(io::Error::other(format!(
                            "chunk {seq} arrived where chunk {} was expected",
                            self.next_seq
                        )));
                    }
                    self.next_seq += 1;
                    self.pending = data;
                    self.pos = 0;
                }
                Ok(WriteStep::Done) => self.status.set(StreamStatus::Done),
                Ok(WriteStep::Abort) | Err(_) => self.status.set(StreamStatus::Aborted),
            }
        }
    }
}

/// Counts the decompressed bytes the archive reads, and how many zero bytes
/// the stream ends with, so the end marker can be checked after the last
/// entry.
struct Counting<R> {
    inner: R,
    zeros: u64,
}

impl<R: Read> Counting<R> {
    fn new(inner: R) -> Self {
        Self { inner, zeros: 0 }
    }
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(out)?;
        self.zeros = match out[..read].iter().rev().position(|&byte| byte != 0) {
            Some(back) => back as u64,
            None => self.zeros + read as u64,
        };
        Ok(read)
    }
}

/// The failure `err` stands for, judged by where the step stream stood: an
/// abort, the stream ending while the archive still wanted bytes, or the
/// archive's own error.
fn ending(status: &Cell<StreamStatus>, eof: &Cell<bool>, err: &io::Error) -> Ending {
    match status.get() {
        StreamStatus::Aborted => Ending::Aborted,
        _ if eof.get() => Ending::Failed("the folder stream ended early".to_owned()),
        _ => Ending::Failed(format!("reading the folder stream: {err}")),
    }
}

/// Replaces `target` with `stage`, refusing a target that is not a folder of
/// its own. An earlier copy is moved aside rather than deleted, so a swap
/// that fails leaves it in place.
fn swap(stage: &Path, target: &Path, aside: &Path) -> Result<(), Ending> {
    let replacing = match fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(Ending::Failed(format!(
                "{} is a link; refusing to replace it",
                target.display()
            )));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(Ending::Failed(format!(
                "{} is not a folder; refusing to replace it",
                target.display()
            )));
        }
        Ok(_) => true,
        Err(err) if err.kind() == io::ErrorKind::NotFound => false,
        Err(err) => {
            return Err(Ending::Failed(format!(
                "reading {}: {err}",
                target.display()
            )));
        }
    };
    if replacing {
        fs::rename(target, aside)
            .map_err(|err| Ending::Failed(format!("moving {} aside: {err}", target.display())))?;
    }
    if let Err(err) = fs::rename(stage, target) {
        if replacing {
            fs::rename(aside, target).map_err(|back| {
                Ending::Failed(format!(
                    "saving {}: {err}; putting the earlier copy back also failed: {back}",
                    target.display()
                ))
            })?;
        }
        return Err(Ending::Failed(format!(
            "saving {}: {err}",
            target.display()
        )));
    }
    if replacing && let Err(err) = fs::remove_dir_all(aside) {
        tracing::warn!(
            aside = %aside.display(),
            "could not delete a replaced folder: {err}"
        );
    }
    Ok(())
}

/// Deletes a stage this call made, warning if the folder will not go.
fn remove_stage(stage: &Path) {
    if let Err(err) = fs::remove_dir_all(stage) {
        tracing::warn!(stage = %stage.display(), "could not delete an unpacked folder's stage: {err}");
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
    use std::io::Write as _;
    use std::path::Path;
    use std::sync::mpsc::channel;

    use tar::{Builder, EntryType, Header};

    use super::{Announced, run_unpacker};
    use crate::file_fetch::{WriteOutcome, WriteStep, part_path};
    use crate::temp_dir::TempDir;

    /// How much of the compressed stream one step carries.
    const CHUNK: usize = 64 * 1024;

    fn chunk(seq: u64, data: &[u8]) -> WriteStep {
        WriteStep::Chunk {
            seq,
            data: data.to_vec(),
        }
    }

    /// `tar` compressed with zstd, as the host would send it.
    fn compress(tar: &[u8]) -> Vec<u8> {
        let mut encoder =
            zstd::stream::write::Encoder::new(Vec::new(), 1).expect("open the encoder");
        encoder.write_all(tar).expect("compress the archive");
        encoder.finish().expect("finish the stream")
    }

    /// [`compress`], cut into 64 KiB chunks and ended with `Done`.
    fn stream(tar: &[u8]) -> Vec<WriteStep> {
        let compressed = compress(tar);
        let mut steps: Vec<WriteStep> = compressed
            .chunks(CHUNK)
            .enumerate()
            .map(|(seq, data)| chunk(seq as u64, data))
            .collect();
        steps.push(WriteStep::Done);
        steps
    }

    /// Runs the unpacker over `steps`, which the channel buffers before it
    /// starts.
    fn unpack(
        target: &Path,
        id: &str,
        announced: Announced,
        steps: Vec<WriteStep>,
    ) -> WriteOutcome {
        unpack_with(target, id, announced, steps, &mut |_| {})
    }

    /// [`unpack`] reporting every unpacked byte count through `progress`.
    fn unpack_with(
        target: &Path,
        id: &str,
        announced: Announced,
        steps: Vec<WriteStep>,
        progress: &mut dyn FnMut(u64),
    ) -> WriteOutcome {
        let (tx, rx) = channel();
        for step in steps {
            tx.send(step).expect("the unpacker's channel is open");
        }
        drop(tx);
        run_unpacker(target, id, announced, rx, progress)
    }

    /// The reason an outcome failed.
    fn failed(outcome: WriteOutcome) -> String {
        match outcome {
            WriteOutcome::Failed(reason) => reason,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    /// The archive `build` writes.
    fn archive(build: impl FnOnce(&mut Builder<Vec<u8>>)) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        build(&mut builder);
        builder.into_inner().expect("finish the archive")
    }

    /// A regular file entry as the crate writes one.
    fn file(builder: &mut Builder<Vec<u8>>, name: &str, body: &[u8]) {
        let mut header = Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, name, body)
            .expect("append a file");
    }

    /// An entry whose name is written into the header as-is, for the names
    /// the builder refuses and the ones Windows would.
    fn raw(
        builder: &mut Builder<Vec<u8>>,
        name: &str,
        kind: EntryType,
        body: &[u8],
        link: Option<&str>,
    ) {
        let mut header = Header::new_gnu();
        let bytes = name.as_bytes();
        header.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
        if let Some(link) = link {
            let bytes = link.as_bytes();
            header.as_old_mut().linkname[..bytes.len()].copy_from_slice(bytes);
        }
        header.set_entry_type(kind);
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, body).expect("append the entry");
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
    fn an_unpacked_folder_lands_byte_exact() {
        let dir = TempDir::new("byte-exact");
        let target = dir.0.join("host").join("repo").join("checkout");
        let big: Vec<u8> = (0..300 * 1024)
            .map(|index| u8::try_from(index % 251).expect("a byte"))
            .collect();
        let tar = archive(|builder| {
            file(builder, "checkout/a.txt", b"hello");
            file(builder, "checkout/deep/b.bin", &big);
            file(builder, "checkout/deep/empty.txt", b"");
        });
        let announced = Announced {
            files: 3,
            bytes: 5 + big.len() as u64,
        };

        let outcome = unpack(&target, "fetch-1", announced, stream(&tar));

        assert_eq!(outcome, WriteOutcome::Saved(target.clone()));
        assert_eq!(
            fs::read(target.join("checkout/a.txt")).expect("read a.txt"),
            b"hello"
        );
        assert_eq!(
            fs::read(target.join("checkout/deep/b.bin")).expect("read b.bin"),
            big
        );
        assert_eq!(
            fs::read(target.join("checkout/deep/empty.txt")).expect("read the empty file"),
            b""
        );
        assert!(
            !part_path(&target, "fetch-1").exists(),
            "no stage is left beside the saved folder"
        );
    }

    #[test]
    fn an_empty_folder_entry_lands() {
        let dir = TempDir::new("empty-folder");
        let target = dir.0.join("checkout");
        let tar = archive(|builder| {
            raw(builder, "empty/", EntryType::Directory, b"", None);
        });

        let outcome = unpack(
            &target,
            "fetch-1",
            Announced { files: 1, bytes: 0 },
            stream(&tar),
        );

        assert_eq!(outcome, WriteOutcome::Saved(target.clone()));
        assert!(target.join("empty").is_dir());
    }

    #[test]
    fn an_entry_with_dotdot_is_refused_and_the_stage_is_deleted() {
        let dir = TempDir::new("dotdot");
        let target = dir.0.join("repo").join("checkout");
        let tar = archive(|builder| {
            file(builder, "ok.txt", b"fine");
            raw(builder, "../evil.txt", EntryType::Regular, b"no", None);
        });

        let reason = failed(unpack(
            &target,
            "fetch-1",
            Announced { files: 2, bytes: 6 },
            stream(&tar),
        ));

        assert!(reason.contains("../evil.txt"), "{reason}");
        assert!(
            !part_path(&target, "fetch-1").exists(),
            "the stage is deleted"
        );
        assert!(!dir.0.join("repo").join("evil.txt").exists());
    }

    #[test]
    fn an_absolute_or_drive_entry_is_refused() {
        for name in ["/etc/x", "C:/x", "C:x", "a:b"] {
            let dir = TempDir::new("absolute");
            let target = dir.0.join("repo").join("checkout");
            let tar = archive(|builder| {
                raw(builder, name, EntryType::Regular, b"no", None);
            });

            let reason = failed(unpack(
                &target,
                "fetch-1",
                Announced { files: 1, bytes: 2 },
                stream(&tar),
            ));

            assert!(reason.contains(name), "{name}: {reason}");
            assert!(!part_path(&target, "fetch-1").exists());
        }
    }

    #[test]
    fn a_symlink_or_hard_link_entry_is_skipped() {
        let dir = TempDir::new("links");
        let target = dir.0.join("checkout");
        let tar = archive(|builder| {
            file(builder, "a.txt", b"content");
            raw(builder, "link", EntryType::Symlink, b"", Some("a.txt"));
            raw(builder, "hard", EntryType::Link, b"", Some("a.txt"));
        });

        let outcome = unpack(
            &target,
            "fetch-1",
            Announced { files: 3, bytes: 7 },
            stream(&tar),
        );

        assert_eq!(outcome, WriteOutcome::Saved(target.clone()));
        assert_eq!(files_in(&target), ["a.txt"]);
    }

    #[test]
    fn a_windows_unsafe_name_is_made_safe() {
        let dir = TempDir::new("unsafe");
        let target = dir.0.join("checkout");
        let tar = archive(|builder| {
            raw(builder, "con.txt", EntryType::Regular, b"1", None);
            raw(builder, "name:stream", EntryType::Regular, b"2", None);
            raw(builder, "trail.", EntryType::Regular, b"3", None);
        });

        let outcome = unpack(
            &target,
            "fetch-1",
            Announced { files: 3, bytes: 3 },
            stream(&tar),
        );

        assert_eq!(outcome, WriteOutcome::Saved(target.clone()));
        assert_eq!(files_in(&target), ["_con.txt", "name_stream", "trail_"]);
    }

    #[test]
    fn two_entries_with_the_same_safe_name_fail() {
        let dir = TempDir::new("same-name");

        let target = dir.0.join("made-safe");
        let tar = archive(|builder| {
            raw(builder, "x?y", EntryType::Regular, b"one", None);
            raw(builder, "x_y", EntryType::Regular, b"two", None);
        });
        let reason = failed(unpack(
            &target,
            "fetch-1",
            Announced { files: 2, bytes: 6 },
            stream(&tar),
        ));
        assert!(reason.contains("x_y"), "{reason}");

        // Two names that differ only in case are one file on NTFS.
        let target = dir.0.join("case");
        let tar = archive(|builder| {
            raw(builder, "A.txt", EntryType::Regular, b"one", None);
            raw(builder, "a.txt", EntryType::Regular, b"two", None);
        });
        let reason = failed(unpack(
            &target,
            "fetch-1",
            Announced { files: 2, bytes: 6 },
            stream(&tar),
        ));
        assert!(reason.contains("a.txt"), "{reason}");
    }

    #[test]
    fn unpacking_past_the_announced_bytes_fails() {
        let dir = TempDir::new("past-bytes");
        let target = dir.0.join("checkout");
        let tar = archive(|builder| file(builder, "big.bin", &[7u8; 100]));

        let reason = failed(unpack(
            &target,
            "fetch-1",
            Announced {
                files: 1,
                bytes: 50,
            },
            stream(&tar),
        ));

        assert!(reason.contains("50"), "{reason}");
        assert!(!part_path(&target, "fetch-1").exists());
    }

    #[test]
    fn unpacking_past_the_announced_files_fails() {
        let dir = TempDir::new("past-files");
        let target = dir.0.join("checkout");
        let tar = archive(|builder| {
            file(builder, "one.txt", b"a");
            file(builder, "two.txt", b"b");
        });

        let reason = failed(unpack(
            &target,
            "fetch-1",
            Announced { files: 1, bytes: 2 },
            stream(&tar),
        ));

        assert!(reason.contains("more files than"), "{reason}");
        assert!(!part_path(&target, "fetch-1").exists());
    }

    #[test]
    fn fewer_files_than_announced_is_saved() {
        let dir = TempDir::new("fewer");
        let target = dir.0.join("checkout");
        let tar = archive(|builder| file(builder, "only.txt", b"text"));

        let outcome = unpack(
            &target,
            "fetch-1",
            Announced { files: 3, bytes: 4 },
            stream(&tar),
        );

        assert_eq!(outcome, WriteOutcome::Saved(target.clone()));
        assert_eq!(fs::read(target.join("only.txt")).expect("read"), b"text");
    }

    #[test]
    fn a_seq_gap_fails_and_deletes_the_stage() {
        let dir = TempDir::new("seq-gap");
        let target = dir.0.join("checkout");
        let tar = archive(|builder| file(builder, "a.txt", &[1u8; 4096]));
        let compressed = compress(&tar);
        let (first, second) = compressed.split_at(compressed.len() / 2);
        let steps = vec![chunk(0, first), chunk(2, second), WriteStep::Done];

        let reason = failed(unpack(
            &target,
            "fetch-1",
            Announced {
                files: 1,
                bytes: 4096,
            },
            steps,
        ));

        assert!(reason.contains("chunk 2"), "{reason}");
        assert!(!part_path(&target, "fetch-1").exists());
    }

    #[test]
    fn an_abort_deletes_the_stage() {
        let dir = TempDir::new("abort");
        let target = dir.0.join("checkout");
        let tar = archive(|builder| file(builder, "a.txt", b"content"));
        let compressed = compress(&tar);
        let steps = vec![
            chunk(0, &compressed[..compressed.len() / 2]),
            WriteStep::Abort,
        ];

        let outcome = unpack(&target, "fetch-1", Announced { files: 1, bytes: 7 }, steps);

        assert_eq!(outcome, WriteOutcome::Aborted);
        assert!(!part_path(&target, "fetch-1").exists());
        assert!(!target.exists());
    }

    #[test]
    fn a_closed_channel_aborts() {
        let dir = TempDir::new("closed");
        let target = dir.0.join("checkout");
        let (tx, rx) = channel::<WriteStep>();
        drop(tx);

        let outcome = run_unpacker(
            &target,
            "fetch-1",
            Announced { files: 0, bytes: 0 },
            rx,
            &mut |_| {},
        );

        assert_eq!(outcome, WriteOutcome::Aborted);
        assert!(!part_path(&target, "fetch-1").exists());
        assert!(!target.exists());
    }

    #[test]
    fn a_stream_that_ends_early_fails() {
        let dir = TempDir::new("early-end");
        let target = dir.0.join("checkout");
        let tar = archive(|builder| file(builder, "a.txt", &[9u8; 4096]));
        // The 512-byte header, then ten of the entry's bytes.
        let cut = &tar[..512 + 10];

        let outcome = unpack(
            &target,
            "fetch-1",
            Announced {
                files: 1,
                bytes: 4096,
            },
            stream(cut),
        );

        let reason = failed(outcome);
        assert!(reason.contains("ended early"), "{reason}");
        assert!(!part_path(&target, "fetch-1").exists());
    }

    #[test]
    fn done_replaces_an_earlier_copy_of_the_folder() {
        let dir = TempDir::new("replace");
        let target = dir.0.join("checkout");
        fs::create_dir_all(&target).expect("make the old copy");
        fs::write(target.join("stale.txt"), b"old").expect("write the stale file");
        let tar = archive(|builder| file(builder, "fresh.txt", b"new"));

        let outcome = unpack(
            &target,
            "fetch-1",
            Announced { files: 1, bytes: 3 },
            stream(&tar),
        );

        assert_eq!(outcome, WriteOutcome::Saved(target.clone()));
        assert!(!target.join("stale.txt").exists(), "the old copy is gone");
        assert_eq!(fs::read(target.join("fresh.txt")).expect("read"), b"new");
    }

    #[test]
    fn an_existing_stage_fails_and_is_left_alone() {
        let dir = TempDir::new("stage-exists");
        let target = dir.0.join("repo").join("checkout");
        let stage = part_path(&target, "fetch-1");
        fs::create_dir_all(&stage).expect("make the stage");
        fs::write(stage.join("keep.txt"), b"keep").expect("write a file in the stage");
        let tar = archive(|builder| file(builder, "a.txt", b"x"));

        let outcome = unpack(
            &target,
            "fetch-1",
            Announced { files: 1, bytes: 1 },
            stream(&tar),
        );

        let reason = failed(outcome);
        assert!(reason.contains("creating"), "{reason}");
        assert_eq!(
            fs::read(stage.join("keep.txt")).expect("read the stage's file"),
            b"keep"
        );
        assert!(!target.exists());
    }

    #[test]
    fn a_file_at_the_target_is_left_alone_and_fails() {
        let dir = TempDir::new("target-file");
        let target = dir.0.join("repo").join("checkout");
        fs::create_dir_all(dir.0.join("repo")).expect("make the target's folder");
        fs::write(&target, b"old").expect("write the file at the target");
        let tar = archive(|builder| file(builder, "a.txt", b"x"));

        let outcome = unpack(
            &target,
            "fetch-1",
            Announced { files: 1, bytes: 1 },
            stream(&tar),
        );

        let reason = failed(outcome);
        assert!(reason.contains("not a folder"), "{reason}");
        assert_eq!(fs::read(&target).expect("read the file"), b"old");
        assert!(!part_path(&target, "fetch-1").exists());
    }

    #[test]
    fn progress_reports_unpacked_bytes() {
        let dir = TempDir::new("progress");
        let target = dir.0.join("checkout");
        let big = vec![3u8; 2 * 1024 * 1024 + 512 * 1024];
        let tar = archive(|builder| {
            file(builder, "big.bin", &big);
            file(builder, "small.txt", b"tail!");
        });
        let total = big.len() as u64 + 5;
        let mut seen: Vec<u64> = Vec::new();

        let outcome = unpack_with(
            &target,
            "fetch-1",
            Announced {
                files: 2,
                bytes: total,
            },
            stream(&tar),
            &mut |bytes| seen.push(bytes),
        );

        assert_eq!(outcome, WriteOutcome::Saved(target.clone()));
        assert_eq!(seen.last().copied(), Some(total), "{seen:?}");
        assert!(
            seen.windows(2).all(|pair| pair[0] <= pair[1]),
            "progress never goes backwards: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|&bytes| (1 << 20..big.len() as u64).contains(&bytes)),
            "progress is reported within the large file: {seen:?}"
        );
    }

    #[test]
    fn a_failed_swap_keeps_the_earlier_copy() {
        let dir = TempDir::new("failed-swap");
        let target = dir.0.join("checkout");
        fs::create_dir_all(&target).expect("make the earlier copy");
        fs::write(target.join("stale.txt"), b"old").expect("write the earlier copy's file");
        // The earlier copy cannot be moved aside, so the swap fails before it
        // is touched.
        let busy = dir.0.join(".fetch-1.old");
        fs::create_dir(&busy).expect("block the aside name");
        fs::write(busy.join("busy.txt"), b"busy").expect("fill the aside name");
        let tar = archive(|builder| file(builder, "fresh.txt", b"new"));

        let outcome = unpack(
            &target,
            "fetch-1",
            Announced { files: 1, bytes: 3 },
            stream(&tar),
        );

        let reason = failed(outcome);
        assert!(reason.contains("aside"), "{reason}");
        assert_eq!(
            fs::read(target.join("stale.txt")).expect("read the earlier copy"),
            b"old"
        );
        assert!(
            !part_path(&target, "fetch-1").exists(),
            "the stage is deleted"
        );
    }

    #[test]
    fn a_stream_without_the_end_marker_fails_and_keeps_the_earlier_copy() {
        let dir = TempDir::new("no-marker");
        let target = dir.0.join("checkout");
        fs::create_dir_all(&target).expect("make the earlier copy");
        fs::write(target.join("stale.txt"), b"old").expect("write the earlier copy's file");
        let tar = archive(|builder| {
            file(builder, "a.txt", b"one");
            file(builder, "b.txt", b"two");
        });
        // The builder ends the archive with the two zero blocks; cut them off.
        let cut = &tar[..tar.len() - 1024];

        let outcome = unpack(
            &target,
            "fetch-1",
            Announced { files: 2, bytes: 6 },
            stream(cut),
        );

        let reason = failed(outcome);
        assert!(reason.contains("ended early"), "{reason}");
        assert_eq!(
            fs::read(target.join("stale.txt")).expect("read the earlier copy"),
            b"old"
        );
    }

    #[test]
    fn an_empty_zstd_frame_fails() {
        let dir = TempDir::new("empty-frame");
        let target = dir.0.join("checkout");

        let outcome = unpack(
            &target,
            "fetch-1",
            Announced { files: 0, bytes: 0 },
            stream(&[]),
        );

        let reason = failed(outcome);
        assert!(reason.contains("ended early"), "{reason}");
        assert!(!target.exists());
    }

    #[test]
    fn many_directory_entries_past_the_announced_files_fail() {
        let dir = TempDir::new("many-dirs");
        let target = dir.0.join("checkout");
        let tar = archive(|builder| {
            raw(builder, "one/", EntryType::Directory, b"", None);
            raw(builder, "two/", EntryType::Directory, b"", None);
            raw(builder, "three/", EntryType::Directory, b"", None);
        });

        let reason = failed(unpack(
            &target,
            "fetch-1",
            Announced { files: 2, bytes: 0 },
            stream(&tar),
        ));

        assert!(reason.contains("more files than"), "{reason}");
    }

    #[test]
    fn a_skipped_entry_claiming_a_huge_size_fails() {
        let dir = TempDir::new("huge-size");
        let target = dir.0.join("checkout");
        let body = vec![0u8; 2 * 1024 * 1024];
        let tar = archive(|builder| {
            raw(builder, "link", EntryType::Symlink, &body, Some("a.txt"));
        });

        let reason = failed(unpack(
            &target,
            "fetch-1",
            Announced {
                files: 1,
                bytes: 1024,
            },
            stream(&tar),
        ));

        assert!(reason.contains("more bytes than"), "{reason}");
    }

    #[test]
    fn padding_past_a_mebibyte_after_the_end_marker_fails() {
        let dir = TempDir::new("padding");
        let target = dir.0.join("checkout");
        let mut tar = archive(|builder| file(builder, "a.txt", b"x"));
        tar.resize(tar.len() + 1024 * 1024 + 1, 0);

        let reason = failed(unpack(
            &target,
            "fetch-1",
            Announced { files: 1, bytes: 1 },
            stream(&tar),
        ));

        assert!(reason.contains("padding"), "{reason}");
    }

    #[test]
    fn a_corrupt_header_reports_the_tar_error() {
        let dir = TempDir::new("corrupt");
        let target = dir.0.join("checkout");
        let mut tar = archive(|builder| file(builder, "a.txt", b"one"));
        // The first byte of the first header's name, which the checksum then
        // no longer matches.
        tar[0] ^= 0xFF;

        let reason = failed(unpack(
            &target,
            "fetch-1",
            Announced { files: 1, bytes: 3 },
            stream(&tar),
        ));

        assert!(reason.contains("checksum"), "{reason}");
        assert!(!reason.contains("ended early"), "{reason}");
    }
}
