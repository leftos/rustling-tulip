//! Per-session scrollback ring buffer on disk.
//!
//! Each PTY chunk (or each headless stream-json line) is appended to
//! `<sessions_dir>/<id>/scrollback.bin` through the session's
//! [`ScrollbackWriter`], which keeps the file open and tracks its length.
//! When the file exceeds the cap, the oldest bytes are dropped up to the next
//! line start and a `<sessions_dir>/<id>/scrollback.truncated` flag is touched
//! so the UI can show "earlier output discarded" on replay.
//!
//! On attach, the frontend issues a one-shot `LoadScrollback` request before
//! subscribing to live PTY events; this lets users see their previous session
//! state across app/daemon restarts without reconnecting to a vanished PTY.
//!
//! All ops are best-effort: failures are logged but never abort session
//! handling. Synchronization: writes are sequential (one writer per session),
//! reads run in dispatch tasks; a brief partial read during a trim operation
//! is acceptable — terminal rendering tolerates it.

use crate::paths::Dirs;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use tracing::warn;

/// 2 MB hard cap on scrollback file size. Tunable; balances "users see
/// enough history" against "we don't waste disk when an agent prints a lot".
const MAX_SIZE: u64 = 2 * 1024 * 1024;
/// When trimming, keep this many bytes (the most recent ones). The 1.5 MB
/// floor + 2 MB ceiling means a trim happens at most once per ~512 KB of
/// fresh output, amortizing the cost of the rewrite.
const TRIM_TO: usize = 1_500_000;
/// How far past the raw trim offset a trim looks for a line start.
const LINE_SEARCH: usize = 4096;

fn scrollback_path(dirs: &Dirs, session_id: &str) -> PathBuf {
    dirs.sessions_dir.join(session_id).join("scrollback.bin")
}

fn truncated_flag_path(dirs: &Dirs, session_id: &str) -> PathBuf {
    dirs.sessions_dir
        .join(session_id)
        .join("scrollback.truncated")
}

/// Where a trim of `bytes` that would keep everything from `raw_offset` on
/// starts instead, so a replay starts at a line: `raw_offset` itself when it
/// already is a line start, else just past the first `\n` in the
/// [`LINE_SEARCH`] bytes from `raw_offset`, else `raw_offset`.
pub fn trim_start(bytes: &[u8], raw_offset: usize) -> usize {
    if raw_offset > 0 && bytes.get(raw_offset - 1) == Some(&b'\n') {
        return raw_offset;
    }
    let end = bytes.len().min(raw_offset.saturating_add(LINE_SEARCH));
    bytes
        .get(raw_offset..end)
        .and_then(|window| window.iter().position(|&b| b == b'\n'))
        .map_or(raw_offset, |at| raw_offset + at + 1)
}

/// A session's scrollback file, kept open for appending. Opened on the first
/// write, creating the session's dir and the file; dropping it closes the
/// file.
#[derive(Debug)]
pub struct ScrollbackWriter {
    path: PathBuf,
    flag_path: PathBuf,
    session_id: String,
    open: Option<OpenFile>,
}

#[derive(Debug)]
struct OpenFile {
    file: File,
    len: u64,
}

impl ScrollbackWriter {
    /// A writer for `session_id`'s scrollback; touches nothing on disk until
    /// the first write.
    pub fn new(dirs: &Dirs, session_id: &str) -> Self {
        Self {
            path: scrollback_path(dirs, session_id),
            flag_path: truncated_flag_path(dirs, session_id),
            session_id: session_id.to_owned(),
            open: None,
        }
    }

    /// Appends `data`. Returns whether the file is now past the cap, when
    /// the caller runs [`Self::trim`] (off the async worker) before the next
    /// write.
    pub fn write(&mut self, data: &[u8]) -> bool {
        if data.is_empty() {
            return false;
        }
        match self.write_inner(data) {
            Ok(len) => len > MAX_SIZE,
            Err(err) => {
                warn!(?err, session_id = %self.session_id, "scrollback append failed");
                // Reopen on the next write rather than append to a handle in
                // an unknown state.
                self.open = None;
                false
            }
        }
    }

    fn write_inner(&mut self, data: &[u8]) -> std::io::Result<u64> {
        let open = match &mut self.open {
            Some(open) => open,
            None => self.open.insert(open_append(&self.path)?),
        };
        open.file.write_all(data)?;
        open.len += data.len() as u64;
        Ok(open.len)
    }

    /// Drops the oldest bytes, keeping at most `TRIM_TO` from the next line
    /// start, through `scrollback.bin.tmp` and a rename, and touches the
    /// truncated flag. Blocking: callers on the async runtime run it in
    /// `spawn_blocking`.
    pub fn trim(&mut self) {
        // Closed before the rename: an open handle blocks it on Windows.
        self.open = None;
        if let Err(err) = self.trim_inner() {
            warn!(?err, session_id = %self.session_id, "scrollback trim failed");
        }
    }

    fn trim_inner(&mut self) -> std::io::Result<()> {
        let bytes = std::fs::read(&self.path)?;
        let keep_from = trim_start(&bytes, bytes.len().saturating_sub(TRIM_TO));
        let tmp = self.path.with_extension("bin.tmp");
        std::fs::write(&tmp, &bytes[keep_from..])?;
        std::fs::rename(&tmp, &self.path)?;
        // Touch the truncated flag (idempotent — already-existing is fine).
        if let Err(err) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.flag_path)
        {
            warn!(?err, session_id = %self.session_id, "scrollback truncated flag not set");
        }
        self.open = Some(open_append(&self.path)?);
        Ok(())
    }

    /// Flushes the open file, so a read of it sees every byte written.
    pub fn flush(&mut self) {
        if let Some(open) = &mut self.open
            && let Err(err) = open.file.flush()
        {
            warn!(?err, session_id = %self.session_id, "scrollback flush failed");
        }
    }
}

/// Opens `path` for appending, creating it and its dir, with its current
/// length (a reattached session appends to the file it left).
fn open_append(path: &Path) -> std::io::Result<OpenFile> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let len = file.metadata()?.len();
    Ok(OpenFile { file, len })
}

/// Load the full scrollback for a session. Returns `(bytes, truncated)` —
/// `truncated == true` means the buffer overflowed at some point and the
/// caller should surface a "earlier output discarded" hint to the user.
/// Missing file is not an error; returns empty + false.
pub fn load(dirs: &Dirs, session_id: &str) -> (Vec<u8>, bool) {
    let path = scrollback_path(dirs, session_id);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => {
            warn!(?err, %session_id, "scrollback load failed");
            return (Vec::new(), false);
        }
    };
    let truncated = truncated_flag_path(dirs, session_id).exists();
    (bytes, truncated)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test setup")]
mod tests {
    use super::*;
    use crate::history::test_support::scratch_dirs;

    #[test]
    fn trim_starts_at_the_next_newline_within_four_kib() {
        let mut bytes = vec![b'a'; 10_000];
        bytes[1_100] = b'\n';
        assert_eq!(trim_start(&bytes, 1_000), 1_101);
    }

    #[test]
    fn trim_falls_back_to_the_raw_offset_without_a_newline_in_four_kib() {
        let bytes = vec![b'a'; 10_000];
        assert_eq!(trim_start(&bytes, 1_000), 1_000, "no newline at all");

        let mut outside = vec![b'a'; 10_000];
        outside[1_000 + LINE_SEARCH] = b'\n';
        assert_eq!(
            trim_start(&outside, 1_000),
            1_000,
            "a newline just past the window"
        );

        let mut last_inside = vec![b'a'; 10_000];
        last_inside[1_000 + LINE_SEARCH - 1] = b'\n';
        assert_eq!(
            trim_start(&last_inside, 1_000),
            1_000 + LINE_SEARCH,
            "a newline on the window's last byte"
        );

        let short = vec![b'a'; 1_010];
        assert_eq!(
            trim_start(&short, 1_000),
            1_000,
            "a window cut short by the end"
        );
    }

    #[test]
    fn trim_keeps_a_raw_offset_that_is_already_a_line_start() {
        let mut bytes = vec![b'a'; 10_000];
        bytes[999] = b'\n';
        bytes[1_100] = b'\n';
        assert_eq!(trim_start(&bytes, 1_000), 1_000);
    }

    #[test]
    fn trim_does_not_start_inside_an_escape_sequence() {
        let line = b"\x1b[38;5;196mred text\x1b[0m\n";
        let mut bytes = Vec::new();
        for _ in 0..100 {
            bytes.extend_from_slice(line);
        }
        // Lands on the `5` inside the 60th line's colour sequence.
        let raw_offset = 59 * line.len() + 5;
        let start = trim_start(&bytes, raw_offset);
        assert_eq!(start, 60 * line.len(), "the trim starts at the next line");
        assert_eq!(bytes[start - 1], b'\n');
        assert!(bytes[start..].starts_with(line));
    }

    #[test]
    fn writer_trims_the_file_at_a_line_start_past_the_cap() {
        let dirs = scratch_dirs("scrollback-writer-trim");
        let mut writer = ScrollbackWriter::new(&dirs, "s1");
        let mut written = Vec::new();
        let mut trimmed = false;
        let mut seq = 0_u32;
        // Lines of 37 bytes, so a raw offset lands mid-line.
        while !trimmed || written.len() < usize::try_from(MAX_SIZE).expect("cap fits") + 1_000 {
            let line = format!("{seq:08} {}\n", "x".repeat(27));
            seq += 1;
            written.extend_from_slice(line.as_bytes());
            if writer.write(line.as_bytes()) {
                writer.trim();
                trimmed = true;
            }
        }
        drop(writer);
        let (persisted, truncated) = load(&dirs, "s1");
        let _ = std::fs::remove_dir_all(&dirs.config);

        assert!(truncated, "the truncated flag is set");
        assert!(persisted.len() as u64 <= MAX_SIZE, "at or under the cap");
        assert!(
            written.ends_with(&persisted),
            "the file holds the last bytes written"
        );
        let cut = written.len() - persisted.len();
        assert!(cut > 0, "the trim dropped bytes");
        assert_eq!(
            written[cut - 1],
            b'\n',
            "the file starts right after a line"
        );
    }
}
