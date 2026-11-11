//! The live-run file format the daemon reads (`docs/plans/dispatch-follow.md`
//! section 3).

use chrono::{DateTime, Utc};
use protocol::AgentId;
use serde::Deserialize;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The live-run file schema this daemon reads.
pub(crate) const SCHEMA: u32 = 1;

/// One dispatch tool's live run, as `<run_id>.json` records it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct LiveRunFile {
    /// The file's schema.
    pub(crate) schema: u32,
    /// The run's id: its run folder's name and the child's pid.
    pub(crate) run_id: String,
    /// The brief's title, for the run's leaf and view.
    pub(crate) title: String,
    /// The dispatch backend, opaque to the daemon.
    pub(crate) backend: String,
    /// The dispatch tier the run was started with.
    #[serde(default)]
    pub(crate) tier: Option<u8>,
    /// The dispatching session's conversation id, when it had one.
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    /// The agent whose stream parser reads the run's stream; absent is the
    /// default agent.
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// The folder the run works in.
    pub(crate) worktree: PathBuf,
    /// The run's output folder.
    pub(crate) run_dir: PathBuf,
    /// The stream file the daemon tails.
    pub(crate) stream: PathBuf,
    /// The run's child process; Stop kills this tree.
    pub(crate) child_pid: u32,
    /// The child's start time, as the tool's liveness check compares it; null
    /// off Windows.
    #[serde(default)]
    pub(crate) child_start: Option<u64>,
    /// The Python dispatch process, shown but never killed.
    #[serde(default)]
    pub(crate) runner_pid: Option<u32>,
    /// The runner's start time; null off Windows.
    #[serde(default)]
    pub(crate) runner_start: Option<u64>,
    /// When the child was spawned, UTC.
    pub(crate) started_at: DateTime<Utc>,
    /// The conversation id a resume continues.
    #[serde(default)]
    pub(crate) resumed_from: Option<String>,
    /// Whether the tool started this run to resume another by itself.
    #[serde(default)]
    pub(crate) auto_resume: bool,
}

impl LiveRunFile {
    /// The agent whose stream parser reads this run: the file's `agent` when
    /// it names a registered plugin, else the default agent.
    pub(crate) fn agent_id(&self) -> AgentId {
        self.agent
            .as_ref()
            .map(|agent| AgentId(agent.clone()))
            .filter(|agent| agent_registry::plugin(agent).is_some())
            .unwrap_or_else(agent_registry::default_agent)
    }
}

/// What [`parse`] made of a live-run file's text.
#[expect(
    clippy::large_enum_variant,
    reason = "the record goes straight to the caller; boxing it would allocate per file"
)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Parsed {
    /// A file this daemon knows how to read.
    Run(LiveRunFile),
    /// A file written under a schema this daemon does not know.
    UnknownSchema(u32),
    /// A file that does not parse, with the reason to log.
    Invalid(String),
}

/// The one field read before the whole record, so a file this daemon cannot
/// read is classified without decoding it.
#[derive(Deserialize)]
struct SchemaProbe {
    schema: u32,
}

/// Classify a live-run file's text: its schema first, then the whole record.
pub(crate) fn parse(text: &str) -> Parsed {
    let probe: SchemaProbe = match serde_json::from_str(text) {
        Ok(probe) => probe,
        Err(err) => return Parsed::Invalid(err.to_string()),
    };
    if probe.schema != SCHEMA {
        return Parsed::UnknownSchema(probe.schema);
    }
    match serde_json::from_str(text) {
        Ok(run) => Parsed::Run(run),
        Err(err) => Parsed::Invalid(err.to_string()),
    }
}

/// Whether `path`'s name is a live-run file's: a `.json` file. The tool's
/// temporary `.json.tmp` files and its `.stop` markers are not.
pub(crate) fn is_run_file(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("json"))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert parsed fields and fixtures with expect"
)]
mod tests {
    use super::*;

    /// The design's example live-run file, verbatim.
    const DESIGN_EXAMPLE: &str = r#"{
  "schema": 1,
  "run_id": "20260928-141503-deepseek-flash-51234",
  "title": "DF.5 Daemon watcher and tailer",
  "backend": "deepseek-flash",
  "tier": 2,
  "session_id": "6f1c0d6e-1a7b-4c39-9d0e-3b8a2f1e7c55",
  "worktree": "D:\\orca\\workspaces\\rustling-tulip\\acornworm",
  "run_dir": "D:\\orca\\workspaces\\rustling-tulip\\acornworm\\.tmp\\dispatch\\20260928-141503-deepseek-flash",
  "stream": "D:\\orca\\workspaces\\rustling-tulip\\acornworm\\.tmp\\dispatch\\20260928-141503-deepseek-flash\\stream.jsonl",
  "child_pid": 51234,
  "child_start": 134036037030000000,
  "runner_pid": 50120,
  "runner_start": 134036037010000000,
  "started_at": "2026-09-28T14:15:03Z",
  "resumed_from": null,
  "auto_resume": false
}"#;

    /// The run `text` parses to.
    fn run_of(text: &str) -> LiveRunFile {
        match parse(text) {
            Parsed::Run(run) => run,
            other => unreachable!("expected a run, got {other:?}"),
        }
    }

    /// A path as the live-run file spells it.
    fn text(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    /// The design example with its `agent` field set to `agent`.
    fn with_agent(agent: &str) -> String {
        DESIGN_EXAMPLE.replace(
            "\"schema\": 1,",
            &format!("\"schema\": 1, \"agent\": {agent:?},"),
        )
    }

    #[test]
    fn parse_reads_the_design_example() {
        let run = run_of(DESIGN_EXAMPLE);
        assert_eq!(run.schema, 1);
        assert_eq!(run.run_id, "20260928-141503-deepseek-flash-51234");
        assert_eq!(run.title, "DF.5 Daemon watcher and tailer");
        assert_eq!(run.backend, "deepseek-flash");
        assert_eq!(run.tier, Some(2));
        assert_eq!(
            run.session_id.as_deref(),
            Some("6f1c0d6e-1a7b-4c39-9d0e-3b8a2f1e7c55")
        );
        assert_eq!(run.agent, None);
        assert_eq!(
            text(&run.worktree),
            r"D:\orca\workspaces\rustling-tulip\acornworm"
        );
        assert_eq!(
            text(&run.run_dir),
            r"D:\orca\workspaces\rustling-tulip\acornworm\.tmp\dispatch\20260928-141503-deepseek-flash"
        );
        assert_eq!(
            text(&run.stream),
            r"D:\orca\workspaces\rustling-tulip\acornworm\.tmp\dispatch\20260928-141503-deepseek-flash\stream.jsonl"
        );
        assert_eq!(run.child_pid, 51234);
        assert_eq!(run.child_start, Some(134_036_037_030_000_000));
        assert_eq!(run.runner_pid, Some(50120));
        assert_eq!(run.runner_start, Some(134_036_037_010_000_000));
        assert_eq!(
            run.started_at,
            "2026-09-28T14:15:03Z"
                .parse::<DateTime<Utc>>()
                .expect("the example's time parses")
        );
        assert_eq!(run.resumed_from, None);
        assert!(!run.auto_resume);
    }

    #[test]
    fn parse_skips_an_unknown_schema() {
        assert_eq!(parse(r#"{"schema": 2}"#), Parsed::UnknownSchema(2));
    }

    #[test]
    fn parse_ignores_unknown_fields() {
        let text = DESIGN_EXAMPLE.replace("\"schema\": 1,", "\"schema\": 1, \"color\": \"red\",");
        assert_eq!(run_of(&text).run_id, "20260928-141503-deepseek-flash-51234");
    }

    #[test]
    fn parse_reports_a_broken_file() {
        let truncated = &DESIGN_EXAMPLE[..DESIGN_EXAMPLE.len() / 2];
        assert!(matches!(parse(truncated), Parsed::Invalid(_)));
    }

    #[test]
    fn parse_reads_null_starts() {
        let text = DESIGN_EXAMPLE.replace(
            "\"child_start\": 134036037030000000",
            "\"child_start\": null",
        );
        assert_eq!(run_of(&text).child_start, None);
    }

    #[test]
    fn is_run_file_ignores_tmp_and_stop() {
        assert!(is_run_file(Path::new("run.json")));
        assert!(is_run_file(Path::new(
            "20260928-141503-deepseek-flash-51234.json"
        )));
        assert!(!is_run_file(Path::new("x.json.tmp")));
        assert!(!is_run_file(Path::new("x.stop")));
        assert!(!is_run_file(Path::new("x.JSON")));
    }

    #[test]
    fn agent_defaults_to_the_default_agent() {
        let default = agent_registry::default_agent();
        assert_eq!(run_of(DESIGN_EXAMPLE).agent_id(), default);
        assert_eq!(run_of(&with_agent("nope")).agent_id(), default);
        assert_eq!(run_of(&with_agent("")).agent_id(), default);
    }
}
