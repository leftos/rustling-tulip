//! Reading sessions back out of `rt-tracer` and daemon log files.
//!
//! The daemon sets `RUSTLING_TULIP_TRACER_LOG` so each tracer writes
//! `<config>/logs/tracer-<session id>.log`. Those logs name the session's
//! working directory, the program and arguments it ran, and how it ended,
//! which is enough to reconstruct a session that ended before the daemon kept
//! a history of its own. A tracer that was killed stops logging at its last
//! activity rather than at the kill, so [`session_end_times`] reads the end
//! time from the daemon's log instead.

use std::collections::HashMap;
use std::str::Chars;

use chrono::{DateTime, Utc};

const START_MARKER: &str = "rt-tracer starting ";
const SPAWN_MARKER: &str = "supervisor: about to spawn child ";
const STOP_MARKER: &str = "supervisor: Stop request received";
const EXIT_MARKER: &str = "supervisor: child exited; shutting down ";
/// Daemon log messages that mark a session's end.
const END_MARKERS: [&str; 2] = ["discard_session: begin ", "tracer_client: child exited "];

/// How a tracer-backed session ended, as its tracer log tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TracerLogEnd {
    /// The daemon asked the tracer to stop the session.
    StoppedByUser,
    /// The child exited on its own with this code.
    Exited { code: i32 },
    /// The log ends with neither: the tracer died or was killed.
    Lost,
}

/// A session reconstructed from its tracer log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TracerLogSummary {
    pub session_id: String,
    /// The child's working directory.
    pub cwd: String,
    pub program: String,
    pub args: Vec<String>,
    /// When the tracer started.
    pub started_at: DateTime<Utc>,
    /// The timestamp of the log's last line.
    pub last_line_at: DateTime<Utc>,
    pub end: TracerLogEnd,
}

/// Summarises a tracer log, or `None` when it has no `rt-tracer starting`
/// line. Lines without a leading timestamp are ignored. A log with no
/// readable spawn line keeps the starting line's cwd and has an empty program
/// and argument list.
#[must_use]
pub fn parse_tracer_log(text: &str) -> Option<TracerLogSummary> {
    let mut scan = Scan::default();
    for line in text.lines() {
        if let Some((at, rest)) = split_timestamp(line) {
            scan.absorb(at, rest);
        }
    }
    scan.finish()
}

/// Each session id's earliest end time in a daemon log: the first
/// `discard_session: begin` or `tracer_client: child exited` line naming it.
#[must_use]
pub fn session_end_times(daemon_log: &str) -> HashMap<String, DateTime<Utc>> {
    let mut ends: HashMap<String, DateTime<Utc>> = HashMap::new();
    for line in daemon_log.lines() {
        let Some((at, rest)) = split_timestamp(line) else {
            continue;
        };
        if !END_MARKERS.iter().any(|marker| rest.contains(marker)) {
            continue;
        }
        let Some(id) = token_after(rest, " session_id=") else {
            continue;
        };
        ends.entry(id.to_owned())
            .and_modify(|end| *end = (*end).min(at))
            .or_insert(at);
    }
    ends
}

struct Start {
    at: DateTime<Utc>,
    session_id: String,
    cwd: String,
}

struct Spawn {
    program: String,
    args: Vec<String>,
    cwd: String,
}

#[derive(Default)]
struct Scan {
    start: Option<Start>,
    spawn: Option<Spawn>,
    stopped: bool,
    exit_code: Option<i32>,
    last_line_at: Option<DateTime<Utc>>,
}

impl Scan {
    fn absorb(&mut self, at: DateTime<Utc>, rest: &str) {
        self.last_line_at = Some(at);
        if self.start.is_none() {
            self.start = parse_start(at, rest);
        }
        if self.spawn.is_none() {
            self.spawn = parse_spawn(rest);
        }
        if rest.contains(STOP_MARKER) {
            self.stopped = true;
        }
        if self.exit_code.is_none() {
            self.exit_code = parse_exit(rest);
        }
    }

    fn finish(self) -> Option<TracerLogSummary> {
        let start = self.start?;
        let end = match (self.stopped, self.exit_code) {
            (true, _) => TracerLogEnd::StoppedByUser,
            (false, Some(code)) => TracerLogEnd::Exited { code },
            (false, None) => TracerLogEnd::Lost,
        };
        let (program, args, cwd) = match self.spawn {
            Some(spawn) => (spawn.program, spawn.args, spawn.cwd),
            None => (String::new(), Vec::new(), start.cwd),
        };
        Some(TracerLogSummary {
            session_id: start.session_id,
            cwd,
            program,
            args,
            started_at: start.at,
            last_line_at: self.last_line_at.unwrap_or(start.at),
            end,
        })
    }
}

/// Splits a tracing line into its leading RFC 3339 timestamp and the rest.
fn split_timestamp(line: &str) -> Option<(DateTime<Utc>, &str)> {
    let (stamp, rest) = line.split_once(' ')?;
    let at = DateTime::parse_from_rfc3339(stamp)
        .ok()?
        .with_timezone(&Utc);
    Some((at, rest))
}

/// The space-free value of `key` (which includes its `=`), when non-empty.
fn token_after<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = &line[line.find(key)? + key.len()..];
    let token = rest.split_once(' ').map_or(rest, |(token, _)| token);
    (!token.is_empty()).then_some(token)
}

/// `rt-tracer starting session_id=<id> cwd=<path> cols=… rows=… argc=…`.
/// The cwd runs up to ` cols=`, so a path with spaces survives.
fn parse_start(at: DateTime<Utc>, rest: &str) -> Option<Start> {
    let fields = &rest[rest.find(START_MARKER)?..];
    let session_id = token_after(fields, " session_id=")?.to_owned();
    let cwd_start = fields.find(" cwd=")? + " cwd=".len();
    let cwd_field = &fields[cwd_start..];
    let cwd = cwd_field
        .find(" cols=")
        .map_or(cwd_field, |end| &cwd_field[..end]);
    Some(Start {
        at,
        session_id,
        cwd: cwd.to_owned(),
    })
}

/// `supervisor: about to spawn child program=<p> argc=N args=[…] cwd=<path>`.
/// The cwd is the last field, so it is everything after the final ` cwd=`.
fn parse_spawn(rest: &str) -> Option<Spawn> {
    let fields = &rest[rest.find(SPAWN_MARKER)? + SPAWN_MARKER.len()..];
    let program_field = fields.strip_prefix("program=")?;
    let argc_at = program_field.find(" argc=")?;
    let after_program = &program_field[argc_at..];
    let cwd_at = after_program.rfind(" cwd=")?;
    let args_at = after_program[..cwd_at].find(" args=")? + " args=".len();
    Some(Spawn {
        program: program_field[..argc_at].to_owned(),
        args: parse_debug_str_list(&after_program[args_at..cwd_at])?,
        cwd: after_program[cwd_at + " cwd=".len()..].to_owned(),
    })
}

/// `supervisor: child exited; shutting down exit_code=N`.
fn parse_exit(rest: &str) -> Option<i32> {
    let fields = &rest[rest.find(EXIT_MARKER)?..];
    token_after(fields, " exit_code=")?.parse().ok()
}

/// Decodes a `Debug`-formatted `Vec<String>`: `["a", "b\\c", "line\nnext"]`.
fn parse_debug_str_list(text: &str) -> Option<Vec<String>> {
    let inner = text.strip_prefix('[')?.strip_suffix(']')?;
    let mut items = Vec::new();
    if inner.is_empty() {
        return Some(items);
    }
    let mut chars = inner.chars();
    loop {
        if chars.next()? != '"' {
            return None;
        }
        items.push(parse_debug_str_body(&mut chars)?);
        match chars.next() {
            None => return Some(items),
            Some(',') if chars.next()? == ' ' => {}
            Some(_) => return None,
        }
    }
}

/// Reads a `Debug`-escaped string body up to and including its closing quote.
fn parse_debug_str_body(chars: &mut Chars<'_>) -> Option<String> {
    let mut out = String::new();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => out.push(parse_escape(chars)?),
            c => out.push(c),
        }
    }
}

/// The character a `Debug` escape (after its backslash) stands for.
fn parse_escape(chars: &mut Chars<'_>) -> Option<char> {
    let decoded = match chars.next()? {
        '\\' => '\\',
        '"' => '"',
        '\'' => '\'',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        '0' => '\0',
        'u' => parse_unicode_escape(chars)?,
        _ => return None,
    };
    Some(decoded)
}

/// `{XXXX}` after `\u`: one to six hex digits naming a scalar value.
fn parse_unicode_escape(chars: &mut Chars<'_>) -> Option<char> {
    if chars.next()? != '{' {
        return None;
    }
    let mut hex = String::new();
    loop {
        match chars.next()? {
            '}' => break,
            c if c.is_ascii_hexdigit() && hex.len() < 6 => hex.push(c),
            _ => return None,
        }
    }
    char::from_u32(u32::from_str_radix(&hex, 16).ok()?)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests unwrap parsed fixtures with expect for clear failure messages"
)]
mod tests {
    use super::{
        TracerLogEnd, TracerLogSummary, parse_debug_str_list, parse_tracer_log, session_end_times,
    };
    use chrono::{DateTime, Utc};

    fn ts(stamp: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(stamp)
            .expect("valid timestamp")
            .with_timezone(&Utc)
    }

    fn parse(lines: &[&str]) -> TracerLogSummary {
        parse_tracer_log(&lines.join("\n")).expect("log has a starting line")
    }

    const LOST_CLAUDE_LOG: [&str; 5] = [
        r"2026-09-27T11:09:41.935397Z  INFO rt_tracer: rt-tracer starting session_id=f6216fc2-54b1-497c-a257-7c297f7d9859 cwd=D:\yaat cols=120 rows=32 argc=6",
        r"2026-09-27T11:09:41.935449Z  INFO rt_tracer::supervisor: supervisor: starting session_id=f6216fc2-54b1-497c-a257-7c297f7d9859 pipe=rt-tracer-f6216fc2-54b1-497c-a257-7c297f7d9859 program=C:\Users\lefto\.local\bin\claude.exe cols=120 rows=32",
        r#"2026-09-27T11:09:41.940609Z  INFO rt_tracer::supervisor: supervisor: about to spawn child program=C:\Users\lefto\.local\bin\claude.exe argc=5 args=["--add-dir", "D:\\yaat-server", "--append-system-prompt", "Workspace member paths for this session (use these for cross-repo file access — they override any absolute paths referenced in CLAUDE.md / AGENTS.md):\n  yaat         ->  D:\\yaat\n  yaat-server  ->  D:\\yaat-server\n", "--dangerously-skip-permissions"] cwd=D:\yaat"#,
        r"2026-09-27T11:09:41.948055Z  INFO rt_tracer::supervisor: supervisor: child spawned child_pid=Some(54124)",
        r"2026-09-27T11:09:41.982317Z  INFO rt_tracer::supervisor: supervisor: client connected iteration=1 session_id=f6216fc2-54b1-497c-a257-7c297f7d9859",
    ];

    const STOP_START: [&str; 2] = [
        r"2026-09-26T03:53:21.116126Z  INFO rt_tracer: rt-tracer starting session_id=22b5a96d-8d81-40e3-a594-498fc639c7cc cwd=D:\rustling-tulip cols=120 rows=32 argc=2",
        r#"2026-09-26T03:53:21.120000Z  INFO rt_tracer::supervisor: supervisor: about to spawn child program=C:\Users\lefto\.local\bin\claude.exe argc=1 args=["--dangerously-skip-permissions"] cwd=D:\rustling-tulip"#,
    ];

    const STOP_END: [&str; 5] = [
        r"2026-09-26T05:09:44.845855Z  INFO rt_tracer::supervisor: supervisor: Stop request received",
        r"2026-09-26T05:09:44.845904Z  INFO rt_tracer::supervisor: supervisor: stop request received, killing child",
        r"2026-09-26T05:09:44.858923Z  INFO rt_tracer::supervisor: supervisor: forwarding child exit to client code=1",
        r"2026-09-26T05:09:44.858945Z  INFO rt_tracer::supervisor: supervisor: child exited; shutting down exit_code=1",
        r"2026-09-26T05:09:44.858955Z  INFO rt_tracer::supervisor: supervisor: client disconnected cleanly iteration=1 session_elapsed_ms=4543729",
    ];

    #[test]
    fn lost_claude_log_yields_its_fields_and_decoded_args() {
        let summary = parse(&LOST_CLAUDE_LOG);

        assert_eq!(summary.session_id, "f6216fc2-54b1-497c-a257-7c297f7d9859");
        assert_eq!(summary.cwd, r"D:\yaat");
        assert_eq!(summary.program, r"C:\Users\lefto\.local\bin\claude.exe");
        assert_eq!(
            summary.args,
            [
                "--add-dir",
                r"D:\yaat-server",
                "--append-system-prompt",
                "Workspace member paths for this session (use these for cross-repo file access — they override any \
                 absolute paths referenced in CLAUDE.md / AGENTS.md):\n  yaat         ->  D:\\yaat\n  \
                 yaat-server  ->  D:\\yaat-server\n",
                "--dangerously-skip-permissions",
            ]
        );
        assert_eq!(summary.started_at, ts("2026-09-27T11:09:41.935397Z"));
        assert_eq!(summary.last_line_at, ts("2026-09-27T11:09:41.982317Z"));
        assert_eq!(summary.end, TracerLogEnd::Lost);
    }

    #[test]
    fn stop_request_marks_the_session_stopped_by_user() {
        let lines: Vec<&str> = STOP_START.iter().chain(STOP_END.iter()).copied().collect();

        let summary = parse(&lines);

        assert_eq!(summary.session_id, "22b5a96d-8d81-40e3-a594-498fc639c7cc");
        assert_eq!(summary.args, ["--dangerously-skip-permissions"]);
        assert_eq!(summary.end, TracerLogEnd::StoppedByUser);
        assert_eq!(summary.last_line_at, ts("2026-09-26T05:09:44.858955Z"));
    }

    #[test]
    fn child_exit_without_stop_is_exited_with_its_code() {
        let clean = [
            STOP_START[0],
            STOP_START[1],
            r"2026-09-26T04:00:00.000001Z  INFO rt_tracer::supervisor: supervisor: child exited; shutting down exit_code=0",
        ];
        assert_eq!(parse(&clean).end, TracerLogEnd::Exited { code: 0 });

        let failed = [
            STOP_START[0],
            r"2026-09-26T04:00:00.000001Z  INFO rt_tracer::supervisor: supervisor: child exited; shutting down exit_code=-1",
        ];
        assert_eq!(parse(&failed).end, TracerLogEnd::Exited { code: -1 });
    }

    #[test]
    fn pwsh_log_decodes_escaped_quotes_and_keeps_root_cwd() {
        let lines = [
            r"2026-09-27T08:00:00.000000Z  INFO rt_tracer: rt-tracer starting session_id=0b8f4c1e-1111-4222-8333-944455556666 cwd=D:\ cols=120 rows=32 argc=4",
            r#"2026-09-27T08:00:00.010000Z  INFO rt_tracer::supervisor: supervisor: about to spawn child program=pwsh.exe argc=3 args=["-NoExit", "-Command", "$global:__rt_original_prompt = $function:prompt; function global:prompt { \"PS $($PWD.Path)> \" }"] cwd=D:\"#,
        ];

        let summary = parse(&lines);

        assert_eq!(summary.program, "pwsh.exe");
        assert_eq!(
            summary.args,
            [
                "-NoExit",
                "-Command",
                r#"$global:__rt_original_prompt = $function:prompt; function global:prompt { "PS $($PWD.Path)> " }"#,
            ]
        );
        assert_eq!(summary.cwd, r"D:\");
    }

    #[test]
    fn log_without_starting_line_is_none() {
        let text = LOST_CLAUDE_LOG[1..].join("\n");

        assert_eq!(parse_tracer_log(&text), None);
        assert_eq!(parse_tracer_log(""), None);
    }

    #[test]
    fn garbage_lines_are_ignored() {
        let lines = [
            "not a log line",
            "",
            "garbage supervisor: Stop request received",
            LOST_CLAUDE_LOG[0],
            "\u{fffd}\u{fffd} torn write",
            LOST_CLAUDE_LOG[2],
            "2026-13-45T99:00:00Z  INFO bad timestamp",
        ];

        let summary = parse(&lines);

        assert_eq!(summary.end, TracerLogEnd::Lost);
        assert_eq!(summary.args.len(), 5);
        assert_eq!(summary.last_line_at, ts("2026-09-27T11:09:41.940609Z"));
    }

    #[test]
    fn missing_spawn_line_keeps_the_starting_cwd() {
        let summary = parse(&[LOST_CLAUDE_LOG[0]]);

        assert_eq!(summary.cwd, r"D:\yaat");
        assert_eq!(summary.program, "");
        assert_eq!(summary.args, [] as [std::string::String; 0]);
        assert_eq!(summary.last_line_at, summary.started_at);
    }

    #[test]
    fn debug_list_parser_decodes_every_escape_and_rejects_malformed_lists() {
        assert_eq!(
            parse_debug_str_list(r#"["a\tb\r", "\u{1b}[0m", "it\'s", "", "nul\0"]"#),
            Some(vec![
                "a\tb\r".to_owned(),
                "\u{1b}[0m".to_owned(),
                "it's".to_owned(),
                String::new(),
                "nul\0".to_owned(),
            ])
        );
        assert_eq!(parse_debug_str_list("[]"), Some(Vec::new()));
        assert_eq!(parse_debug_str_list(r#"["open"#), None);
        assert_eq!(parse_debug_str_list(r#"["a","b"]"#), None);
        assert_eq!(parse_debug_str_list(r#"["bad \q escape"]"#), None);
        assert_eq!(parse_debug_str_list(r#"["\u{110000}"]"#), None);
    }

    #[test]
    fn session_end_times_keeps_the_earliest_end_marker_per_session() {
        let log = [
            r"2026-09-27T18:29:05.139435Z  INFO rustling_tulipd::server: discard_session: begin session_id=a39b8bf3-48f6-4d8d-8b6c-51394f5ab4d1 cleanup_targets=0 remove_worktrees=0",
            r"2026-09-27T18:29:04.000000Z  INFO rustling_tulipd::tracer_client: tracer_client: child exited session_id=a39b8bf3-48f6-4d8d-8b6c-51394f5ab4d1 code=0",
            r"2026-09-26T05:09:44.858969Z  INFO rustling_tulipd::tracer_client: tracer_client: child exited session_id=22b5a96d-8d81-40e3-a594-498fc639c7cc code=1",
            r"2026-09-26T05:10:00.000000Z  INFO rustling_tulipd::server: discard_session: begin session_id=22b5a96d-8d81-40e3-a594-498fc639c7cc cleanup_targets=1 remove_worktrees=1",
            r"2026-09-25T00:00:00.000000Z  INFO rt_tracer::supervisor: supervisor: client connected iteration=1 session_id=f6216fc2-54b1-497c-a257-7c297f7d9859",
            r"2026-09-25T00:00:00.000000Z  INFO rustling_tulipd::server: attach session_id=22b5a96d-8d81-40e3-a594-498fc639c7cc",
            "discard_session: begin session_id=no-timestamp",
        ]
        .join("\n");

        let ends = session_end_times(&log);

        assert_eq!(ends.len(), 2);
        assert_eq!(
            ends["a39b8bf3-48f6-4d8d-8b6c-51394f5ab4d1"],
            ts("2026-09-27T18:29:04.000000Z")
        );
        assert_eq!(
            ends["22b5a96d-8d81-40e3-a594-498fc639c7cc"],
            ts("2026-09-26T05:09:44.858969Z")
        );
    }
}
