//! A log file writer that caps its own size while the process runs.
//!
//! Both binaries rotate their log once, at start (`daemon.log.old`,
//! `native.log.old`). A process that runs for days would keep growing its log
//! with nothing to stop it, so the writer handed to tracing is this one: it
//! rolls the file it outgrew to `<path>.1` whenever a write would take it past
//! its cap, replacing the earlier `.1` generation. The start-of-run `.old` file
//! is left alone, so the previous run's log survives until the next start.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The cap both binaries open their log with: 5 MiB.
pub const DEFAULT_CAP: u64 = 5 * 1024 * 1024;

/// A log file opened for writing that rolls itself to `<path>.1` when a write
/// would take it past its cap.
///
/// A single write bigger than the cap is still written whole: it rolls first
/// when the file already holds something, and an empty file is never rolled.
///
/// A roll that cannot move the file (a reader without delete sharing, an
/// antivirus holding it) keeps logging to the same file, says so on stderr once
/// per attempt, and tries again at the next cap crossing. The writer turns
/// itself off — dropping that write and every later one, and still reporting
/// them as written so logging never fails its caller — only when it can no
/// longer reopen or write the file at all, and says that once on stderr.
pub struct CappedLog {
    path: PathBuf,
    cap: u64,
    file: Option<File>,
    written: u64,
}

impl CappedLog {
    /// Open `path` for writing, truncating it, with `cap` as its byte cap.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be created or opened for writing.
    pub fn open(path: impl Into<PathBuf>, cap: u64) -> io::Result<Self> {
        let path = path.into();
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        Ok(Self {
            path,
            cap,
            file: Some(file),
            written: 0,
        })
    }

    /// Roll when `len` more bytes would take a non-empty file past the cap,
    /// then write them. A failure to reopen or write turns the writer off
    /// rather than reaching the caller.
    fn accept(&mut self, buf: &[u8], len: u64) {
        if self.file.is_none() {
            return;
        }
        if self.written > 0 && self.written.saturating_add(len) > self.cap {
            self.roll();
        }
        let result = match self.file.as_mut() {
            Some(file) => file.write_all(buf),
            None => return,
        };
        match result {
            Ok(()) => self.written = self.written.saturating_add(len),
            Err(err) => self.turn_off(&err),
        }
    }

    /// Move the file to `<path>.1`, replacing the earlier generation, and
    /// reopen `path` empty.
    fn roll(&mut self) {
        // Closed before the rename: Windows moves a file this process has open
        // only when it was opened with delete sharing, which std's `File` does
        // not ask for.
        self.file = None;
        match self.roll_onto_generation() {
            Ok(()) => {}
            Err(err) => self.reopen_append(&err),
        }
    }

    fn roll_onto_generation(&mut self) -> io::Result<()> {
        // No pre-delete: `fs::rename` replaces an existing target ("replacing
        // the original file if `to` already exists"; on Windows it is
        // `MoveFileExW`), and a target that is a directory makes it fail, so a
        // failed rename leaves the earlier generation where it was.
        std::fs::rename(&self.path, roll_target(&self.path))?;
        // The bytes moved away with the file, whatever the reopen does.
        self.written = 0;
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        self.file = Some(file);
        Ok(())
    }

    /// A roll that could not move the file keeps logging: `path` is reopened
    /// for append, so the next bytes land in the same file, and the next cap
    /// crossing tries the roll again. The writer is off only when even that
    /// reopen fails.
    fn reopen_append(&mut self, roll_err: &io::Error) {
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => {
                self.file = Some(file);
                let _ = writeln!(
                    io::stderr(),
                    "capped log {} could not roll to {}: {roll_err}; continuing in the same file",
                    self.path.display(),
                    roll_target(&self.path).display()
                );
            }
            Err(err) => self.turn_off(&err),
        }
    }

    /// Drop the file handle and say once, on stderr, why nothing more is
    /// written: the handle is gone, so no later write reaches this again.
    fn turn_off(&mut self, err: &io::Error) {
        self.file = None;
        let _ = writeln!(
            io::stderr(),
            "capped log {} turned off after an IO error: {err}",
            self.path.display()
        );
    }
}

impl Write for CappedLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.accept(buf, buf.len() as u64);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let result = match self.file.as_mut() {
            Some(file) => file.flush(),
            None => return Ok(()),
        };
        if let Err(err) = result {
            self.turn_off(&err);
        }
        Ok(())
    }
}

/// `path` with `.1` appended: the generation an in-run roll moves the file it
/// outgrew to.
fn roll_target(path: &Path) -> PathBuf {
    let mut target = path.as_os_str().to_owned();
    target.push(".1");
    PathBuf::from(target)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{CappedLog, roll_target};
    use crate::test_fakes::ScratchDir;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    fn log_path(scratch: &ScratchDir) -> PathBuf {
        scratch.0.join("capped.log")
    }

    fn contents(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read the log file")
    }

    #[test]
    fn a_write_past_the_cap_rolls_to_the_generation_and_starts_a_fresh_file() {
        let scratch = ScratchDir::new("capped-roll");
        let path = log_path(&scratch);
        let mut log = CappedLog::open(&path, 8).expect("open the log");

        log.write_all(b"12345").expect("a write under the cap");
        log.write_all(b"6789")
            .expect("a write past the cap is accepted");

        assert_eq!(contents(&path), "6789", "the live file restarts empty");
        assert_eq!(
            contents(&roll_target(&path)),
            "12345",
            "the previous contents move to the generation file"
        );
    }

    #[test]
    fn a_second_roll_replaces_the_old_generation() {
        let scratch = ScratchDir::new("capped-second-roll");
        let path = log_path(&scratch);
        let mut log = CappedLog::open(&path, 4).expect("open the log");

        log.write_all(b"aaaa")
            .expect("the first file fills the cap");
        log.write_all(b"bbbb").expect("the second write rolls");
        log.write_all(b"cccc").expect("the third write rolls again");

        assert_eq!(contents(&path), "cccc");
        assert_eq!(
            contents(&roll_target(&path)),
            "bbbb",
            "only the newest generation is kept"
        );
    }

    #[test]
    fn a_write_larger_than_the_cap_is_kept_whole_after_a_roll() {
        let scratch = ScratchDir::new("capped-oversized");
        let path = log_path(&scratch);
        let mut log = CappedLog::open(&path, 8).expect("open the log");
        let burst = "x".repeat(64);

        log.write_all(b"12345").expect("a write under the cap");
        log.write_all(burst.as_bytes())
            .expect("an oversized write is accepted");

        assert_eq!(contents(&path), burst, "the burst is not cut short");
        assert_eq!(contents(&roll_target(&path)), "12345");
    }

    #[test]
    fn a_write_larger_than_the_cap_into_an_empty_file_does_not_roll() {
        let scratch = ScratchDir::new("capped-empty-oversized");
        let path = log_path(&scratch);
        let mut log = CappedLog::open(&path, 8).expect("open the log");
        let burst = "x".repeat(64);

        log.write_all(burst.as_bytes())
            .expect("an oversized first write is accepted");

        assert_eq!(contents(&path), burst, "the burst is written whole");
        assert!(
            !roll_target(&path).exists(),
            "an empty file has nothing to roll"
        );
    }

    #[test]
    fn a_failed_roll_keeps_logging_to_the_same_file() {
        let scratch = ScratchDir::new("capped-failed-roll");
        let path = log_path(&scratch);
        std::fs::create_dir_all(roll_target(&path))
            .expect("a directory where the roll target goes, so the rename fails");
        let mut log = CappedLog::open(&path, 8).expect("open the log");

        log.write_all(b"12345").expect("a write under the cap");
        let rolled = log.write_all(b"6789");

        assert!(
            rolled.is_ok(),
            "a failed roll must not fail the write: {rolled:?}"
        );
        assert_eq!(
            contents(&path),
            "123456789",
            "logging continues in the same file"
        );
        assert!(
            log.write_all(b"0000").is_ok(),
            "every later write is accepted too"
        );
        assert_eq!(
            contents(&path),
            "1234567890000",
            "the next cap crossing retries the roll and keeps logging"
        );
        assert!(
            roll_target(&path).is_dir(),
            "a failed roll leaves the earlier generation alone"
        );
    }

    #[test]
    fn a_write_with_no_file_open_returns_ok() {
        let scratch = ScratchDir::new("capped-off");
        let path = log_path(&scratch);
        let mut off = CappedLog {
            path: path.clone(),
            cap: 8,
            file: None,
            written: 0,
        };

        assert!(
            off.write_all(b"anything").is_ok(),
            "a writer that turned itself off must not fail its caller"
        );
        assert!(off.flush().is_ok(), "flush is accepted too");
        assert!(!path.exists(), "nothing is written");
    }
}
