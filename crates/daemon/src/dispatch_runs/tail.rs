//! A run's stream tail: one task per run polls its stream file from the last
//! byte read, cuts the new bytes into complete lines and folds them, with the
//! events the run's agent parses from them, into the run's state.

use super::{Batch, DispatchRuns};
use protocol::AgentId;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

/// What one run's tailer reads and where it folds it.
pub(super) struct Tailer {
    pub(super) runs: Arc<DispatchRuns>,
    pub(super) run_id: String,
    /// The listing the tailer belongs to.
    pub(super) generation: u64,
    pub(super) stream: PathBuf,
    pub(super) agent: AgentId,
    pub(super) period: Duration,
}

/// Poll the run's stream every `period` until `cancel`: the first read is
/// from byte 0, a stream not there yet is waited for, and a stream that
/// shrank is read again from its start, its run's state cleared at the next
/// fold. Each tick that completes at least one line folds all of them and
/// sends one `Updated`.
pub(super) async fn run(tailer: Tailer, cancel: CancellationToken) {
    // `LiveRunFile::agent_id` falls back to the default agent, so the agent
    // is always a registered one.
    let Some(plugin) = agent_registry::plugin(&tailer.agent) else {
        return;
    };
    let mut buffer = TailBuffer::default();
    let mut offset = 0u64;
    let mut restart = false;
    let mut tick = tokio::time::interval(tailer.period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let Some(bytes) = read_new(&tailer.stream, &mut offset, &mut buffer, &mut restart) else {
            continue;
        };
        let lines: Vec<String> = buffer
            .feed(&bytes)
            .into_iter()
            .filter(|line| !line.trim().is_empty())
            .collect();
        if lines.is_empty() {
            continue;
        }
        let events = lines
            .iter()
            .flat_map(|line| plugin.parse_headless_line(line))
            .collect();
        let batch = Batch {
            events,
            lines: &lines,
            plugin,
        };
        tailer
            .runs
            .fold(&tailer.run_id, tailer.generation, restart, batch);
        restart = false;
    }
}

/// The bytes `path` gained since `offset`, advancing it; `None` when the file
/// is missing, unreadable or has nothing new. A file shorter than `offset`
/// was rewritten: the buffer and offset go back to its start, and `shrank`
/// is set for the caller to clear the run's state at its next fold.
fn read_new(
    path: &Path,
    offset: &mut u64,
    buffer: &mut TailBuffer,
    shrank: &mut bool,
) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len < *offset {
        buffer.reset();
        *offset = 0;
        *shrank = true;
    }
    if len == *offset {
        return None;
    }
    file.seek(SeekFrom::Start(*offset)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    *offset += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    Some(bytes)
}

/// The partial trailing line of a stream, kept until its newline arrives.
#[derive(Debug, Default)]
pub(crate) struct TailBuffer {
    /// The bytes after the last newline read so far.
    partial: Vec<u8>,
}

impl TailBuffer {
    /// Append `bytes` and return the lines they complete, oldest first, with
    /// their `\n` or `\r\n` endings stripped (UTF-8 lossy). The bytes after
    /// the last newline stay for the next call.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.partial.extend_from_slice(bytes);
        let Some(last_newline) = self.partial.iter().rposition(|&byte| byte == b'\n') else {
            return Vec::new();
        };
        let complete: Vec<u8> = self.partial.drain(..=last_newline).collect();
        complete[..last_newline]
            .split(|&byte| byte == b'\n')
            .map(|line| {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                String::from_utf8_lossy(line).into_owned()
            })
            .collect()
    }

    /// Drop the partial line, for a stream that shrank and is read again from
    /// its start.
    pub(crate) fn reset(&mut self) {
        self.partial.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_buffer_holds_a_partial_line_until_its_newline() {
        let mut buffer = TailBuffer::default();
        assert_eq!(buffer.feed(b"one\ntw"), ["one"]);
        assert!(buffer.feed(b"o").is_empty(), "no newline yet");
        assert_eq!(buffer.feed(b"\nthree\nfo"), ["two", "three"]);
        assert_eq!(buffer.feed(b"ur\n"), ["four"]);
    }

    #[test]
    fn tail_buffer_strips_crlf() {
        let mut buffer = TailBuffer::default();
        assert_eq!(buffer.feed(b"one\r\ntwo\r"), ["one"]);
        assert_eq!(buffer.feed(b"\n"), ["two"]);
    }

    #[test]
    fn tail_buffer_reset_drops_the_partial_line() {
        let mut buffer = TailBuffer::default();
        assert_eq!(buffer.feed(b"stale").len(), 0);
        buffer.reset();
        assert_eq!(buffer.feed(b"fresh\n"), ["fresh"]);
    }
}
