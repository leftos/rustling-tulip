//! Shell integration marks: the `OSC 133` prompt, output and end marks and
//! the `OSC 633;E` command line a plain shell writes around each command,
//! found by a scanner that reads the same bytes as the terminal, and the
//! command records they build.
//!
//! A record's rows are absolute: the count of rows the terminal has ever
//! pushed into its history plus the row's own line, so a record keeps its
//! row however far the output scrolls (see `term.rs`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use alacritty_terminal::vte::{Params, Parser, Perform};

/// How many finished commands a pane remembers, oldest dropped first.
pub const MAX_RECORDS: usize = 1000;

/// How a finished command ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellStatus {
    /// Exit code 0.
    Ok,
    /// Any other exit code.
    Fail,
    /// The shell reported no exit code.
    Unknown,
}

impl ShellStatus {
    #[must_use]
    pub fn of(exit: Option<i32>) -> Self {
        match exit {
            Some(0) => Self::Ok,
            Some(_) => Self::Fail,
            None => Self::Unknown,
        }
    }
}

/// A finished command whose prompt row is on screen, as the pane draws its
/// dot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellDot {
    /// The viewport row of the command's prompt.
    pub row: usize,
    pub status: ShellStatus,
    pub exit: Option<i32>,
    /// `exit N · 1.23s`; without the duration for a command replayed from
    /// history.
    pub tooltip: String,
}

/// One finished command. Rows are absolute (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The row the prompt was drawn on (`OSC 133;A`).
    pub prompt: u64,
    /// The row the output started on (`OSC 133;C`), when the shell said and
    /// the row is still known.
    pub output: Option<u64>,
    /// The row the cursor was on when the command ended (`OSC 133;D`), while
    /// the row is still known: a reflow can lose it and keep the prompt.
    pub end: Option<u64>,
    pub exit: Option<i32>,
    /// The command line (`OSC 633;E`), when the shell sent it.
    pub command: Option<String>,
    /// When each mark arrived; `None` for marks replayed from history.
    pub prompt_at: Option<Instant>,
    pub output_at: Option<Instant>,
    pub end_at: Option<Instant>,
}

impl Record {
    #[must_use]
    pub fn status(&self) -> ShellStatus {
        ShellStatus::of(self.exit)
    }

    /// From the output start (else the prompt) to the end; `None` for a
    /// command replayed from history.
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        let end = self.end_at?;
        let start = self.output_at.or(self.prompt_at)?;
        Some(end.saturating_duration_since(start))
    }

    #[must_use]
    pub fn tooltip(&self) -> String {
        tooltip(self.exit, self.duration())
    }
}

/// `exit N · dur`, `?` standing in for a missing code; just `exit N`
/// without a duration.
#[must_use]
pub fn tooltip(exit: Option<i32>, duration: Option<Duration>) -> String {
    let code = exit.map_or_else(|| "?".to_owned(), |code| code.to_string());
    match duration {
        Some(duration) => format!("exit {code} · {}", format_duration(duration)),
        None => format!("exit {code}"),
    }
}

/// Under a second `Nms`; under ten `N.NNs`; under a minute `N.Ns`; else
/// `Mm Ss`. Each rounds half up at its own precision.
#[must_use]
pub fn format_duration(duration: Duration) -> String {
    let micros = duration.as_micros();
    let rounded = |unit: u128| (micros + unit / 2) / unit;
    if duration < Duration::from_secs(1) {
        format!("{}ms", rounded(1_000))
    } else if duration < Duration::from_secs(10) {
        let centis = rounded(10_000);
        format!("{}.{:02}s", centis / 100, centis % 100)
    } else if duration < Duration::from_secs(60) {
        let tenths = rounded(100_000);
        format!("{}.{}s", tenths / 10, tenths % 10)
    } else {
        let secs = rounded(1_000_000);
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

/// An `OSC 633;E` command line decoded: `\\` is a backslash and `\xHH` the
/// character with that code. Anything else stays as written.
#[must_use]
pub fn decode_command(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        if let Some(after) = tail.strip_prefix("\\\\") {
            out.push('\\');
            rest = after;
        } else if let Some(ch) = tail
            .strip_prefix("\\x")
            .and_then(|hex| hex.get(..2))
            .filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(char::from(ch));
            rest = &tail[4..];
        } else {
            out.push('\\');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// A shell mark, applied at the cursor once the bytes before it are written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mark {
    /// `133;A`: the prompt is being drawn.
    Prompt,
    /// `133;C`: the command's output starts.
    Output,
    /// `133;D[;code]`: the command ended.
    End(Option<i32>),
    /// `633;E;cmd`: the command line; any later argument is ignored.
    Command(String),
}

/// Where the scanner splits the output. A mark takes effect after its own
/// bytes; the others before their final byte, so the terminal is read as it
/// stood before the sequence ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Split {
    Mark(Mark),
    /// `CSI 3 J`: the history is about to be erased.
    ClearHistory,
    /// `ESC c`: the terminal is about to be reset.
    Reset,
    /// `CSI ? 1049/1047/47 h`: the alternate screen may be entered.
    AltEnter,
    /// `CSI ? 1049/1047/47 l`: the alternate screen may have been left
    /// (taken after the sequence's own bytes).
    AltExit,
}

impl Split {
    /// Whether the split falls before the sequence's final byte.
    #[must_use]
    pub fn before_final(&self) -> bool {
        matches!(self, Self::ClearHistory | Self::Reset | Self::AltEnter)
    }
}

/// A parser of its own that reads every byte the terminal reads and stops
/// at each sequence the marks care about. It never changes the bytes.
#[derive(Default)]
pub struct Scanner {
    parser: Parser,
    found: Found,
}

impl Scanner {
    /// Reads `bytes` up to and including the final byte of the next split.
    /// Returns how many bytes it read and the split, or all of them and
    /// `None` when there is none.
    pub fn next(&mut self, bytes: &[u8]) -> (usize, Option<Split>) {
        let read = self.parser.advance_until_terminated(&mut self.found, bytes);
        (read, self.found.0.take())
    }
}

#[derive(Default)]
struct Found(Option<Split>);

/// The alternate-screen private modes.
const ALT_MODES: [u16; 3] = [1049, 1047, 47];

impl Perform for Found {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.0 = parse_osc(params).map(Split::Mark);
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let mut values = params
            .iter()
            .map(|param| param.first().copied().unwrap_or(0));
        self.0 = match (intermediates, action) {
            ([], 'J') if values.next() == Some(3) => Some(Split::ClearHistory),
            ([b'?'], 'h') if values.any(|mode| ALT_MODES.contains(&mode)) => Some(Split::AltEnter),
            ([b'?'], 'l') if values.any(|mode| ALT_MODES.contains(&mode)) => Some(Split::AltExit),
            _ => None,
        };
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if intermediates.is_empty() && byte == b'c' {
            self.0 = Some(Split::Reset);
        }
    }

    fn terminated(&self) -> bool {
        self.0.is_some()
    }
}

/// The mark an OSC carries. `OSC 633;E`'s command line is its first
/// argument alone: the shell escapes a `;` inside it as `\x3b`, so a later
/// argument (a nonce) is not part of it.
fn parse_osc(params: &[&[u8]]) -> Option<Mark> {
    let (&code, rest) = params.split_first()?;
    let (&kind, args) = rest.split_first()?;
    match (code, kind) {
        (b"133", b"A") => Some(Mark::Prompt),
        (b"133", b"C") => Some(Mark::Output),
        (b"133", b"D") => {
            let exit = args
                .first()
                .and_then(|arg| std::str::from_utf8(arg).ok())
                .and_then(|arg| arg.trim().parse().ok());
            Some(Mark::End(exit))
        }
        (b"633", b"E") => {
            let raw = args.first().copied().unwrap_or_default();
            Some(Mark::Command(decode_command(&String::from_utf8_lossy(raw))))
        }
        _ => None,
    }
}

/// The command a prompt has started and no end has closed yet.
#[derive(Clone, Debug)]
struct Open {
    prompt: u64,
    output: Option<u64>,
    command: Option<String>,
    prompt_at: Option<Instant>,
    output_at: Option<Instant>,
    /// Whether a command ran: an output start or a command line arrived
    /// since the prompt.
    ran: bool,
}

/// The finished commands, oldest first, and the one in flight.
#[derive(Default)]
pub struct Records {
    done: VecDeque<Record>,
    open: Option<Open>,
}

impl Records {
    /// A prompt on row `abs`. The same row again is a redraw of the same
    /// prompt; a new row drops a command that never ended (an empty Enter
    /// or Ctrl+C), which gets no dot.
    pub fn prompt(&mut self, abs: u64, at: Option<Instant>) {
        if self.open.as_ref().is_some_and(|open| open.prompt == abs) {
            return;
        }
        self.open = Some(Open {
            prompt: abs,
            output: None,
            command: None,
            prompt_at: at,
            output_at: None,
            ran: false,
        });
    }

    pub fn output(&mut self, abs: u64, at: Option<Instant>) {
        if let Some(open) = self.open.as_mut() {
            open.output = Some(abs);
            open.output_at = at;
            open.ran = true;
        }
    }

    pub fn command(&mut self, text: String) {
        if let Some(open) = self.open.as_mut() {
            open.command = Some(text);
            open.ran = true;
        }
    }

    /// Ends the command in flight on row `abs`, if there is one. A prompt
    /// that ran no command (an empty Enter or Ctrl+C, which zsh's and bash's
    /// hooks still end) is dropped with no record.
    pub fn end(&mut self, abs: u64, exit: Option<i32>, at: Option<Instant>) {
        let Some(open) = self.open.take().filter(|open| open.ran) else {
            return;
        };
        self.done.push_back(Record {
            prompt: open.prompt,
            output: open.output,
            end: Some(abs),
            exit,
            command: open.command,
            prompt_at: open.prompt_at,
            output_at: open.output_at,
            end_at: at,
        });
        while self.done.len() > MAX_RECORDS {
            self.done.pop_front();
        }
    }

    pub fn apply(&mut self, mark: Mark, abs: u64, at: Option<Instant>) {
        match mark {
            Mark::Prompt => self.prompt(abs, at),
            Mark::Output => self.output(abs, at),
            Mark::End(exit) => self.end(abs, exit, at),
            Mark::Command(text) => self.command(text),
        }
    }

    /// Drops every command whose prompt row is below `base`: it left the
    /// history.
    pub fn evict_below(&mut self, base: u64) {
        self.done.retain(|record| record.prompt >= base);
        if self.open.as_ref().is_some_and(|open| open.prompt < base) {
            self.open = None;
        }
    }

    pub fn clear(&mut self) {
        self.done.clear();
        self.open = None;
    }

    /// The finished commands, oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Record> {
        self.done.iter()
    }

    /// Every row a command is anchored to.
    #[must_use]
    pub fn anchors(&self) -> Vec<u64> {
        let mut rows: Vec<u64> = self
            .done
            .iter()
            .flat_map(|record| [Some(record.prompt), record.output, record.end])
            .chain(
                self.open
                    .iter()
                    .flat_map(|open| [Some(open.prompt), open.output]),
            )
            .flatten()
            .collect();
        rows.sort_unstable();
        rows.dedup();
        rows
    }

    /// Moves every anchor through `to`. A command whose prompt has no new
    /// row is dropped; an output start or end without one is forgotten, and
    /// the command kept.
    pub fn remap(&mut self, to: impl Fn(u64) -> Option<u64>) {
        self.done.retain_mut(|record| {
            let Some(prompt) = to(record.prompt) else {
                return false;
            };
            record.prompt = prompt;
            record.output = record.output.and_then(&to);
            record.end = record.end.and_then(&to);
            true
        });
        self.open = self.open.take().and_then(|mut open| {
            open.prompt = to(open.prompt)?;
            open.output = open.output.and_then(&to);
            Some(open)
        });
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        MAX_RECORDS, Mark, Records, Scanner, ShellStatus, Split, decode_command, format_duration,
        tooltip,
    };

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Every split the scanner finds in `bytes`, with the offset just past
    /// each one's final byte.
    fn splits(bytes: &[u8]) -> Vec<(usize, Split)> {
        let mut scanner = Scanner::default();
        let mut found = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let (read, split) = scanner.next(&bytes[at..]);
            at += read;
            if let Some(split) = split {
                found.push((at, split));
            }
        }
        found
    }

    #[test]
    fn the_command_line_decodes_escaped_backslashes_and_hex_codes() {
        assert_eq!(decode_command("ls -la"), "ls -la");
        assert_eq!(decode_command(r"echo a\x3bb"), "echo a;b");
        assert_eq!(decode_command(r"a\x0ab"), "a\nb");
        assert_eq!(decode_command(r"C:\\dir"), r"C:\dir");
        assert_eq!(
            decode_command(r"\\x41"),
            r"\x41",
            "an escaped backslash ends first"
        );
        assert_eq!(decode_command(r"end\x4"), r"end\x4", "a short code stays");
        assert_eq!(decode_command(r"a\xzz"), r"a\xzz", "a bad code stays");
        assert_eq!(decode_command("trail\\"), "trail\\");
        assert_eq!(decode_command(r"\xe9t\xE9"), "été");
    }

    #[test]
    fn durations_format_at_each_boundary() {
        assert_eq!(format_duration(ms(0)), "0ms");
        assert_eq!(format_duration(ms(999)), "999ms");
        assert_eq!(format_duration(ms(1000)), "1.00s");
        assert_eq!(format_duration(ms(1234)), "1.23s");
        assert_eq!(format_duration(ms(9994)), "9.99s");
        assert_eq!(format_duration(ms(10_000)), "10.0s");
        assert_eq!(format_duration(ms(59_900)), "59.9s");
        assert_eq!(format_duration(ms(60_000)), "1m 0s");
        assert_eq!(format_duration(ms(61_500)), "1m 2s");
        assert_eq!(format_duration(ms(119_600)), "2m 0s", "never `1m 60s`");
        assert_eq!(format_duration(ms(3_723_000)), "62m 3s");
    }

    #[test]
    fn the_tooltip_names_the_exit_and_the_duration() {
        assert_eq!(tooltip(Some(0), Some(ms(1500))), "exit 0 · 1.50s");
        assert_eq!(tooltip(None, Some(ms(20))), "exit ? · 20ms");
        assert_eq!(tooltip(Some(2), None), "exit 2");
        assert_eq!(tooltip(None, None), "exit ?");
    }

    #[test]
    fn the_scanner_finds_marks_and_the_splits_the_anchors_need() {
        let bytes = b"a\x1b]133;A\x07$ \x1b]133;B\x07\x1b]633;E;ls;x\x07\x1b]133;C\x07out\x1b]133;D;2\x1b\\";
        let found: Vec<Split> = splits(bytes).into_iter().map(|(_, split)| split).collect();
        assert_eq!(
            found,
            [
                Split::Mark(Mark::Prompt),
                Split::Mark(Mark::Command("ls".to_owned())),
                Split::Mark(Mark::Output),
                Split::Mark(Mark::End(Some(2))),
            ],
            "B is ignored"
        );
        let bare = splits(b"\x1b]133;D\x07\x1b]133;D;x\x07");
        assert_eq!(bare[0].1, Split::Mark(Mark::End(None)));
        assert_eq!(bare[1].1, Split::Mark(Mark::End(None)));

        let others = splits(b"\x1b[2J\x1b[3J\x1bc\x1b[?1049h\x1b[?25;47l\x1b]633;P;Cwd=x\x07");
        let ends: Vec<(usize, Split)> = others;
        assert_eq!(
            ends,
            [
                (8, Split::ClearHistory),
                (10, Split::Reset),
                (18, Split::AltEnter),
                (27, Split::AltExit),
            ]
        );
    }

    #[test]
    fn the_command_line_is_the_first_argument_only() {
        let found =
            splits(b"\x1b]633;E;ls -la;abc123\x07\x1b]633;E;echo a\\x3bb\x07\x1b]633;E\x07");
        let commands: Vec<Split> = found.into_iter().map(|(_, split)| split).collect();
        assert_eq!(
            commands,
            [
                Split::Mark(Mark::Command("ls -la".to_owned())),
                Split::Mark(Mark::Command("echo a;b".to_owned())),
                Split::Mark(Mark::Command(String::new())),
            ],
            "a nonce after the command is ignored, an escaped `;` decoded"
        );
    }

    #[test]
    fn a_mark_split_across_feeds_is_found_once_whole() {
        let mut scanner = Scanner::default();
        assert_eq!(scanner.next(b"x\x1b]13"), (5, None));
        assert_eq!(scanner.next(b"3;A"), (3, None));
        assert_eq!(
            scanner.next(b"\x07tail"),
            (1, Some(Split::Mark(Mark::Prompt)))
        );
    }

    #[test]
    fn a_finished_command_keeps_its_rows_times_and_command() {
        let t = Instant::now();
        let mut records = Records::default();
        records.prompt(3, Some(t));
        records.command("make".to_owned());
        records.output(4, Some(t + ms(100)));
        records.end(9, Some(1), Some(t + ms(1600)));
        let done: Vec<_> = records.iter().collect();
        assert_eq!(done.len(), 1);
        assert_eq!(
            (done[0].prompt, done[0].output, done[0].end),
            (3, Some(4), Some(9))
        );
        assert_eq!(done[0].status(), ShellStatus::Fail);
        assert_eq!(done[0].command.as_deref(), Some("make"));
        assert_eq!(
            done[0].duration(),
            Some(ms(1500)),
            "timed from the output start"
        );
        assert_eq!(done[0].tooltip(), "exit 1 · 1.50s");
    }

    #[test]
    fn without_an_output_start_the_duration_counts_from_the_prompt() {
        let t = Instant::now();
        let mut records = Records::default();
        records.prompt(0, Some(t));
        records.command("true".to_owned());
        records.end(1, None, Some(t + ms(40)));
        let record = records.iter().next().expect("a record");
        assert_eq!(record.status(), ShellStatus::Unknown);
        assert_eq!(record.tooltip(), "exit ? · 40ms");
    }

    #[test]
    fn replayed_marks_make_records_without_a_duration() {
        let mut records = Records::default();
        records.prompt(0, None);
        records.output(1, None);
        records.end(2, Some(0), None);
        let record = records.iter().next().expect("a record");
        assert_eq!(record.duration(), None);
        assert_eq!(record.tooltip(), "exit 0");
    }

    #[test]
    fn a_prompt_on_a_new_row_before_the_end_drops_the_command() {
        let mut records = Records::default();
        records.prompt(0, None);
        records.output(0, None);
        records.prompt(1, None);
        records.output(2, None);
        records.end(3, Some(0), None);
        let prompts: Vec<u64> = records.iter().map(|r| r.prompt).collect();
        assert_eq!(prompts, [1], "the first command never ended");
        records.end(4, Some(0), None);
        assert_eq!(
            records.iter().count(),
            1,
            "an end with none in flight is ignored"
        );
    }

    #[test]
    fn an_end_with_no_command_since_the_prompt_makes_no_record() {
        // zsh: its precmd hook ends every prompt, an empty Enter's too.
        let mut records = Records::default();
        records.end(0, Some(0), None);
        records.prompt(0, None);
        records.end(1, Some(0), None);
        records.prompt(1, None);
        assert_eq!(records.iter().count(), 0, "an empty Enter makes no dot");
        records.command("ls".to_owned());
        records.end(2, Some(0), None);
        let prompts: Vec<u64> = records.iter().map(|r| r.prompt).collect();
        assert_eq!(prompts, [1], "a command line alone makes one");

        // bash: the PROMPT_COMMAND trap ends a prompt left by Ctrl+C.
        let mut records = Records::default();
        records.prompt(5, None);
        records.end(6, Some(130), None);
        records.output(6, None);
        records.end(7, Some(0), None);
        assert_eq!(
            records.iter().count(),
            0,
            "the end dropped the prompt, so nothing is in flight"
        );
        records.prompt(7, None);
        records.output(8, None);
        records.end(9, Some(130), None);
        let exits: Vec<Option<i32>> = records.iter().map(|r| r.exit).collect();
        assert_eq!(exits, [Some(130)], "an output start alone makes one");
    }

    #[test]
    fn a_remap_keeps_a_command_while_its_prompt_maps() {
        let mut records = Records::default();
        records.prompt(1, None);
        records.command("make".to_owned());
        records.output(2, None);
        records.end(3, Some(0), None);
        records.remap(|row| (row == 1).then_some(10));
        let record = records.iter().next().expect("kept by its prompt");
        assert_eq!((record.prompt, record.output, record.end), (10, None, None));
        assert_eq!(record.command.as_deref(), Some("make"));
        assert_eq!(records.anchors(), [10]);
    }

    #[test]
    fn a_redrawn_prompt_on_the_same_row_keeps_the_command() {
        let t = Instant::now();
        let mut records = Records::default();
        records.prompt(5, Some(t));
        records.output(6, Some(t + ms(10)));
        records.prompt(5, Some(t + ms(20)));
        records.end(7, Some(0), Some(t + ms(30)));
        let record = records.iter().next().expect("a record");
        assert_eq!(record.output, Some(6));
        assert_eq!(record.prompt_at, Some(t));
    }

    #[test]
    fn records_are_capped_oldest_first() {
        let mut records = Records::default();
        for n in 0..=u64::try_from(MAX_RECORDS).expect("small") {
            records.prompt(n, None);
            records.output(n, None);
            records.end(n, Some(0), None);
        }
        assert_eq!(records.iter().count(), MAX_RECORDS);
        assert_eq!(records.iter().next().map(|r| r.prompt), Some(1));
    }

    #[test]
    fn eviction_and_remap_move_or_drop_whole_commands() {
        let mut records = Records::default();
        for n in [2, 5, 8] {
            records.prompt(n, None);
            records.output(n + 1, None);
            records.end(n + 2, Some(0), None);
        }
        records.prompt(11, None);
        assert_eq!(records.anchors(), [2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
        records.evict_below(5);
        assert_eq!(records.anchors(), [5, 6, 7, 8, 9, 10, 11]);
        records.remap(|row| (row != 8).then_some(row + 100));
        let prompts: Vec<u64> = records.iter().map(|r| r.prompt).collect();
        assert_eq!(prompts, [105]);
        assert_eq!(records.anchors(), [105, 106, 107, 111]);
        records.evict_below(200);
        assert!(records.anchors().is_empty());
    }
}
