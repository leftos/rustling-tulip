//! Claude Code conversation transcripts on disk.
//!
//! Claude Code writes each conversation to
//! `<claude home>/projects/<encoded cwd>/<conversation id>.jsonl`. This module
//! finds the transcripts a session in a given working directory may have
//! written during a time window, so a session that ended unexpectedly can be
//! offered back to the user as `claude --resume <conversation id>`.

use std::ffi::{OsStr, OsString};
use std::fs::{self, DirEntry, File};
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use directories::UserDirs;
use serde_json::Value;
use tracing::debug;

/// How many lines of a transcript are read looking for its cwd and title.
const HEAD_SCAN_LINES: usize = 200;
/// Longest title, in characters, including the trailing ellipsis.
const TITLE_MAX_CHARS: usize = 80;

/// One conversation Claude Code recorded for a working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptCandidate {
    /// The conversation id: the transcript's file stem, as `claude --resume` takes it.
    pub id: String,
    /// The transcript file's modified time.
    pub last_active: DateTime<Utc>,
    /// The conversation's summary, else its first user message, tidied for display.
    pub title: Option<String>,
}

/// Claude Code's home: `$CLAUDE_CONFIG_DIR` when set and non-empty, else `<home>/.claude`.
#[must_use]
pub fn claude_home() -> Option<PathBuf> {
    let home = UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    claude_home_from(std::env::var_os("CLAUDE_CONFIG_DIR"), home)
}

fn claude_home_from(config_dir: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    match config_dir {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => home.map(|home| home.join(".claude")),
    }
}

/// The folder name Claude Code files a working directory's transcripts under:
/// every character outside `[A-Za-z0-9-]` becomes `-`, with no collapsing of
/// runs and no case change (`D:\rustling-tulip` → `D--rustling-tulip`).
#[must_use]
pub fn encode_project_dir(cwd: &str) -> String {
    cwd.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The transcripts recorded for `cwd` whose files were modified within
/// `[window_start, window_end]`, newest first, at most `limit` of them.
///
/// Only top-level `*.jsonl` files count (subfolders hold subagent
/// transcripts), and only those whose first recorded `cwd` is `cwd` itself:
/// distinct directories can encode to the same folder name. A missing folder
/// yields no candidates; unreadable files and malformed lines are skipped.
#[must_use]
pub fn candidates(
    claude_home: &Path,
    cwd: &str,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    limit: usize,
) -> Vec<TranscriptCandidate> {
    let dir = project_dir(claude_home, cwd);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) => {
            if err.kind() != ErrorKind::NotFound {
                debug!(dir = %dir.display(), error = %err, "transcripts: cannot list project folder");
            }
            return Vec::new();
        }
    };
    let mut found: Vec<TranscriptCandidate> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| candidate_from_entry(&entry, cwd, window_start, window_end))
        .collect();
    found.sort_by(|a, b| {
        b.last_active
            .cmp(&a.last_active)
            .then_with(|| a.id.cmp(&b.id))
    });
    found.truncate(limit);
    found
}

/// Whether Claude Code holds a transcript `id` for `cwd`. An `id` that is not
/// a plain file-name stem (a path separator, `..`) is never found.
#[must_use]
pub fn transcript_exists(claude_home: &Path, cwd: &str, id: &str) -> bool {
    is_plain_id(id)
        && project_dir(claude_home, cwd)
            .join(format!("{id}.jsonl"))
            .is_file()
}

/// The transcript `id` recorded for `cwd`, with its modified time and title,
/// when [`transcript_exists`] finds it.
#[must_use]
pub fn known_conversation(claude_home: &Path, cwd: &str, id: &str) -> Option<TranscriptCandidate> {
    if !transcript_exists(claude_home, cwd, id) {
        return None;
    }
    let path = project_dir(claude_home, cwd).join(format!("{id}.jsonl"));
    let last_active: DateTime<Utc> = fs::metadata(&path).ok()?.modified().ok()?.into();
    let title = scan_head(&path).and_then(|head| head.summary.or(head.first_user));
    Some(TranscriptCandidate {
        id: id.to_owned(),
        last_active,
        title,
    })
}

fn project_dir(claude_home: &Path, cwd: &str) -> PathBuf {
    claude_home.join("projects").join(encode_project_dir(cwd))
}

fn is_plain_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn candidate_from_entry(
    entry: &DirEntry,
    cwd: &str,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Option<TranscriptCandidate> {
    let path = entry.path();
    if path.extension().and_then(OsStr::to_str) != Some("jsonl") {
        return None;
    }
    let meta = entry.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let last_active: DateTime<Utc> = meta.modified().ok()?.into();
    if last_active < window_start || last_active > window_end {
        return None;
    }
    let id = path.file_stem()?.to_str()?.to_owned();
    let head = scan_head(&path)?;
    if cwd_key(head.cwd.as_deref()?) != cwd_key(cwd) {
        return None;
    }
    Some(TranscriptCandidate {
        id,
        last_active,
        title: head.summary.or(head.first_user),
    })
}

/// A path compared the way Windows treats it: case-insensitive, `/` and `\`
/// alike, a trailing separator ignored.
fn cwd_key(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

/// What the first lines of a transcript say about it.
#[derive(Default)]
struct Head {
    cwd: Option<String>,
    summary: Option<String>,
    first_user: Option<String>,
}

impl Head {
    fn absorb(&mut self, value: &Value) {
        if self.cwd.is_none() {
            self.cwd = value.get("cwd").and_then(Value::as_str).map(str::to_owned);
        }
        match value.get("type").and_then(Value::as_str) {
            Some("summary") if self.summary.is_none() => {
                self.summary = value
                    .get("summary")
                    .and_then(Value::as_str)
                    .and_then(tidy_title);
            }
            Some("user") if self.first_user.is_none() && !is_meta(value) => {
                self.first_user = user_text(value)
                    .filter(|text| !is_command_noise(text))
                    .and_then(tidy_title);
            }
            _ => {}
        }
    }

    fn is_complete(&self) -> bool {
        self.cwd.is_some() && self.summary.is_some()
    }
}

fn scan_head(path: &Path) -> Option<Head> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) => {
            debug!(path = %path.display(), error = %err, "transcripts: cannot open transcript");
            return None;
        }
    };
    let mut reader = BufReader::new(file);
    let mut head = Head::default();
    let mut line = Vec::new();
    for _ in 0..HEAD_SCAN_LINES {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) => {
                debug!(path = %path.display(), error = %err, "transcripts: read failed mid-transcript");
                break;
            }
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&line) {
            head.absorb(&value);
        }
        if head.is_complete() {
            break;
        }
    }
    Some(head)
}

/// Prefixes of the user messages Claude Code writes for slash commands and
/// their local output, which say nothing about the conversation.
const COMMAND_NOISE_PREFIXES: [&str; 5] = [
    "<command-name>",
    "<command-message>",
    "<local-command-stdout>",
    "<local-command-caveat>",
    "Caveat:",
];

/// A transcript line Claude Code marks `"isMeta": true`: context it injected,
/// not something the user typed.
fn is_meta(value: &Value) -> bool {
    value.get("isMeta").and_then(Value::as_bool) == Some(true)
}

fn is_command_noise(text: &str) -> bool {
    let text = text.trim_start();
    COMMAND_NOISE_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

/// A user message's text: its `content` when that is a string, else the first
/// `text` part of a content array.
fn user_text(value: &Value) -> Option<&str> {
    let content = value.get("message")?.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text);
    }
    content
        .as_array()?
        .iter()
        .find(|part| part.get("type").and_then(Value::as_str) == Some("text"))?
        .get("text")?
        .as_str()
}

/// Whitespace collapsed to single spaces, cut to [`TITLE_MAX_CHARS`] with `…`.
/// Blank text is no title.
fn tidy_title(raw: &str) -> Option<String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= TITLE_MAX_CHARS {
        return Some(collapsed);
    }
    let mut cut: String = collapsed.chars().take(TITLE_MAX_CHARS - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    Some(cut)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert scratch setup preconditions with expect for clear failure messages"
)]
mod tests {
    use super::{
        TranscriptCandidate, candidates, claude_home_from, encode_project_dir, known_conversation,
        transcript_exists,
    };
    use chrono::{DateTime, Duration, TimeZone, Utc};
    use serde_json::{Value, json};
    use std::ffi::OsString;
    use std::fs::{self, File};
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;
    use uuid::Uuid;

    /// RAII scratch Claude home under the OS temp root. Drop removes the tree.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rt-transcripts-{label}-{}",
                Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).expect("create scratch dir");
            Self { path }
        }

        fn home(&self) -> &Path {
            &self.path
        }

        /// Writes `projects/<encoded cwd>/<name>` with one JSON value a line,
        /// modified at `modified`.
        fn transcript(&self, cwd: &str, name: &str, lines: &[Value], modified: DateTime<Utc>) {
            let mut body = String::new();
            for line in lines {
                body.push_str(&line.to_string());
                body.push('\n');
            }
            self.raw_transcript(cwd, name, body.as_bytes(), modified);
        }

        fn raw_transcript(&self, cwd: &str, name: &str, body: &[u8], modified: DateTime<Utc>) {
            let path = self
                .path
                .join("projects")
                .join(encode_project_dir(cwd))
                .join(name);
            fs::create_dir_all(path.parent().expect("transcript has a parent"))
                .expect("create project folder");
            fs::write(&path, body).expect("write transcript");
            File::options()
                .write(true)
                .open(&path)
                .expect("reopen transcript")
                .set_modified(SystemTime::from(modified))
                .expect("set transcript mtime");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    const CWD: &str = r"D:\proj";

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 27, hour, 0, 0)
            .single()
            .expect("valid time")
    }

    fn user(cwd: &str, content: &str) -> Value {
        json!({"type": "user", "cwd": cwd, "message": {"role": "user", "content": content}})
    }

    fn all(home: &Path, cwd: &str) -> Vec<TranscriptCandidate> {
        candidates(home, cwd, at(0), at(23), usize::MAX)
    }

    fn ids(found: &[TranscriptCandidate]) -> Vec<&str> {
        found.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn encode_project_dir_matches_claude_folder_names() {
        assert_eq!(encode_project_dir(r"D:\"), "D--");
        assert_eq!(encode_project_dir(r"D:\in-the-sky"), "D--in-the-sky");
        assert_eq!(
            encode_project_dir(r"D:\rustling-tulip"),
            "D--rustling-tulip"
        );
        assert_eq!(
            encode_project_dir(r"C:\Users\lefto\My Proj.v2"),
            "C--Users-lefto-My-Proj-v2"
        );
        assert_eq!(encode_project_dir(r"\\host\share\x"), "--host-share-x");
    }

    #[test]
    fn claude_home_prefers_config_dir_then_home() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            claude_home_from(Some(OsString::from("/cfg")), home.clone()),
            Some(PathBuf::from("/cfg"))
        );
        assert_eq!(
            claude_home_from(None, home.clone()),
            Some(Path::new("/home/u").join(".claude"))
        );
        assert_eq!(
            claude_home_from(Some(OsString::new()), home),
            Some(Path::new("/home/u").join(".claude")),
            "an empty CLAUDE_CONFIG_DIR counts as unset"
        );
        assert_eq!(claude_home_from(None, None), None);
    }

    #[test]
    fn window_keeps_only_transcripts_modified_inside_it() {
        let scratch = Scratch::new("window");
        let line = [user(CWD, "hi")];
        scratch.transcript(CWD, "before.jsonl", &line, at(9) - Duration::seconds(1));
        scratch.transcript(CWD, "start.jsonl", &line, at(9));
        scratch.transcript(CWD, "inside.jsonl", &line, at(10));
        scratch.transcript(CWD, "end.jsonl", &line, at(11));
        scratch.transcript(CWD, "after.jsonl", &line, at(11) + Duration::seconds(1));

        let found = candidates(scratch.home(), CWD, at(9), at(11), usize::MAX);

        assert_eq!(ids(&found), ["end", "inside", "start"]);
        assert_eq!(found[1].last_active, at(10));
    }

    #[test]
    fn transcript_recorded_for_another_cwd_is_rejected() {
        let scratch = Scratch::new("mismatch");
        // `D:\a b` and `D:\a-b` both encode to `D--a-b`.
        scratch.transcript(r"D:\a-b", "other.jsonl", &[user(r"D:\a b", "hi")], at(10));
        scratch.transcript(r"D:\a-b", "mine.jsonl", &[user(r"D:\a-b", "hi")], at(10));
        scratch.transcript(
            r"D:\a-b",
            "no-cwd.jsonl",
            &[json!({"type": "summary", "summary": "s"})],
            at(10),
        );

        assert_eq!(ids(&all(scratch.home(), r"D:\a-b")), ["mine"]);
    }

    #[test]
    fn cwd_compare_ignores_case_separators_and_trailing_separator() {
        let scratch = Scratch::new("root");
        scratch.transcript(r"D:\", "root.jsonl", &[user("d:/", "hi")], at(10));
        scratch.transcript(r"D:\Proj", "proj.jsonl", &[user("d:/proj/", "hi")], at(10));

        assert_eq!(ids(&all(scratch.home(), r"D:\")), ["root"]);
        assert_eq!(ids(&all(scratch.home(), r"D:\Proj")), ["proj"]);
    }

    #[test]
    fn subfolder_transcripts_are_ignored() {
        let scratch = Scratch::new("subfolder");
        scratch.transcript(CWD, "top.jsonl", &[user(CWD, "hi")], at(10));
        scratch.transcript(
            CWD,
            "top/subagents/agent-1.jsonl",
            &[user(CWD, "hi")],
            at(10),
        );
        scratch.transcript(CWD, "notes.txt", &[user(CWD, "hi")], at(10));

        assert_eq!(ids(&all(scratch.home(), CWD)), ["top"]);
    }

    #[test]
    fn summary_is_preferred_over_first_user_message() {
        let scratch = Scratch::new("summary");
        let lines = [
            user(CWD, "first question"),
            json!({"type": "summary", "summary": "  Fix the   login bug "}),
        ];
        scratch.transcript(CWD, "s.jsonl", &lines, at(10));
        scratch.transcript(
            CWD,
            "u.jsonl",
            &[user(CWD, "  first\n\tquestion  ")],
            at(11),
        );

        let found = all(scratch.home(), CWD);

        assert_eq!(found[0].title.as_deref(), Some("first question"));
        assert_eq!(found[1].title.as_deref(), Some("Fix the login bug"));
    }

    #[test]
    fn array_content_user_message_uses_its_first_text_part() {
        let scratch = Scratch::new("array");
        let lines = [
            json!({"type": "user", "cwd": CWD, "message": {"content": [
                {"type": "tool_result", "content": "ignored"}
            ]}}),
            json!({"type": "user", "cwd": CWD, "message": {"content": [
                {"type": "image"},
                {"type": "text", "text": "look at this"},
                {"type": "text", "text": "and this"}
            ]}}),
        ];
        scratch.transcript(CWD, "a.jsonl", &lines, at(10));

        assert_eq!(
            all(scratch.home(), CWD)[0].title.as_deref(),
            Some("look at this")
        );
    }

    fn title_of(lines: &[Value]) -> Option<String> {
        let scratch = Scratch::new("noise");
        scratch.transcript(CWD, "n.jsonl", lines, at(10));
        all(scratch.home(), CWD)[0].title.clone()
    }

    #[test]
    fn meta_user_messages_are_skipped_for_the_title() {
        let lines = [
            json!({"type": "user", "cwd": CWD, "isMeta": true,
                   "message": {"content": "injected context"}}),
            user(CWD, "the real question"),
        ];
        assert_eq!(title_of(&lines).as_deref(), Some("the real question"));
    }

    #[test]
    fn command_noise_string_contents_are_skipped_for_the_title() {
        for noise in [
            "<command-name>/clear</command-name>",
            "<command-message>clear</command-message>",
            "<local-command-stdout></local-command-stdout>",
            "<local-command-caveat>Caveat: generated</local-command-caveat>",
            "Caveat: The messages below were generated by the user",
        ] {
            let lines = [user(CWD, noise), user(CWD, "after the noise")];
            assert_eq!(
                title_of(&lines).as_deref(),
                Some("after the noise"),
                "{noise} is skipped"
            );
        }
    }

    #[test]
    fn command_noise_first_text_part_is_skipped_for_the_title() {
        let lines = [
            json!({"type": "user", "cwd": CWD, "message": {"content": [
                {"type": "text", "text": "<command-name>/model</command-name>"}
            ]}}),
            json!({"type": "user", "cwd": CWD, "message": {"content": [
                {"type": "text", "text": "Caveat: local output"}
            ]}}),
            json!({"type": "user", "cwd": CWD, "message": {"content": [
                {"type": "text", "text": "array question"}
            ]}}),
        ];
        assert_eq!(title_of(&lines).as_deref(), Some("array question"));
    }

    #[test]
    fn only_noise_leaves_no_title() {
        let lines = [
            user(CWD, "<command-name>/clear</command-name>"),
            json!({"type": "user", "cwd": CWD, "isMeta": true, "message": {"content": "x"}}),
        ];
        assert_eq!(title_of(&lines), None);
    }

    #[test]
    fn known_conversation_reports_mtime_and_title() {
        let scratch = Scratch::new("known");
        scratch.transcript(CWD, "abc.jsonl", &[user(CWD, "hello there")], at(10));

        let found = known_conversation(scratch.home(), CWD, "abc").expect("found");

        assert_eq!(found.id, "abc");
        assert_eq!(found.last_active, at(10));
        assert_eq!(found.title.as_deref(), Some("hello there"));
        assert_eq!(known_conversation(scratch.home(), CWD, "missing"), None);
    }

    #[test]
    fn long_title_is_cut_to_eighty_chars_with_ellipsis() {
        let scratch = Scratch::new("cut");
        let long = "é".repeat(100);
        scratch.transcript(CWD, "long.jsonl", &[user(CWD, &long)], at(10));

        let title = all(scratch.home(), CWD)[0].title.clone().expect("title");

        assert_eq!(title.chars().count(), 80);
        assert_eq!(title, format!("{}…", "é".repeat(79)));
    }

    #[test]
    fn candidates_are_newest_first_and_limited() {
        let scratch = Scratch::new("sort");
        for (name, hour) in [("a", 10), ("b", 12), ("c", 11), ("d", 9)] {
            scratch.transcript(CWD, &format!("{name}.jsonl"), &[user(CWD, "hi")], at(hour));
        }

        let found = candidates(scratch.home(), CWD, at(0), at(23), 3);

        assert_eq!(ids(&found), ["b", "c", "a"]);
    }

    #[test]
    fn missing_project_folder_yields_nothing() {
        let scratch = Scratch::new("missing");

        assert!(all(scratch.home(), CWD).is_empty());
        assert!(!transcript_exists(scratch.home(), CWD, "abc"));
    }

    #[test]
    fn transcript_exists_finds_only_plain_ids_in_the_cwd_folder() {
        let scratch = Scratch::new("exists");
        scratch.transcript(CWD, "abc-1.jsonl", &[user(CWD, "hi")], at(10));
        scratch.transcript(r"D:\other", "xyz.jsonl", &[user(r"D:\other", "hi")], at(10));

        assert!(transcript_exists(scratch.home(), CWD, "abc-1"));
        assert!(!transcript_exists(scratch.home(), CWD, "xyz"));
        assert!(!transcript_exists(scratch.home(), CWD, r"..\D--other\xyz"));
        assert!(!transcript_exists(scratch.home(), CWD, ""));
    }

    #[test]
    fn garbage_lines_are_skipped() {
        let scratch = Scratch::new("garbage");
        let mut body = b"not json\n\n[1, 2]\n\xff\xfe broken utf8\n{\"type\": \"user\"\n".to_vec();
        body.extend_from_slice(format!("{}\r\n", user(CWD, "real question")).as_bytes());
        scratch.raw_transcript(CWD, "g.jsonl", &body, at(10));
        scratch.raw_transcript(CWD, "empty.jsonl", b"", at(10));

        let found = all(scratch.home(), CWD);

        assert_eq!(ids(&found), ["g"]);
        assert_eq!(found[0].title.as_deref(), Some("real question"));
    }
}
