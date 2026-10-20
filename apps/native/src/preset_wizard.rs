//! The preset wizard's state and rules: the stage on screen, the prompt
//! source picked among the preset's allowed ones and its readiness, the
//! variables asked for at launch and which of them are still required, and
//! the launch source each prompt source produces. Plain Rust, so every rule
//! is tested without a window; the wizard's view renders it.

use std::collections::HashMap;
use std::path::PathBuf;

use protocol::{
    LaunchPresetSource, PresetEntry, PresetPromptSource, PresetTarget, PresetVariable,
    PresetVariableKind,
};

use crate::FileFilter;
use crate::preset_prompts::{issue_summary, parse_issue_spec, parse_prompts};

/// The tooltip of the last stage's Next, which is disabled.
pub const PREVIEW_LATER_TIP: &str = "Preview and launch arrive with the next update";
pub const SOURCE_LEGEND: &str = "Prompt source";
pub const FILE_LABEL: &str = "Prompts file";
pub const FILE_PLACEHOLDER: &str = "Pick a .txt or .md file…";
pub const FOLDER_LABEL: &str = "Prompts folder";
pub const FOLDER_PLACEHOLDER: &str = "Pick a folder of .md files…";
pub const PICK_LABEL: &str = "Pick…";
pub const CLEAR_LABEL: &str = "Clear";
pub const INLINE_LABEL: &str = "Prompts (one per bullet or paragraph)";
pub const INLINE_PLACEHOLDER: &str = "- first prompt\n- second prompt\n\nor paragraph-separated:\n\nfirst prompt\nstill first\n\nsecond prompt";
pub const ISSUES_LABEL: &str = "GitHub issues";
pub const ISSUES_NOTE: &str = " · one session per issue";
pub const ISSUES_PLACEHOLDER: &str = "123-127, 131, 134-136";
pub const ISSUES_HINT: &str = "Comma-separated issue numbers; ranges like 123-127 are inclusive.";
pub const REQUIRED_NOTE: &str = " · required";
pub const REQUIRED_HINT: &str = "Required — fill in before continuing.";
/// The file filter the File source's picker offers.
pub const PROMPTS_FILTER_NAME: &str = "Prompts";
pub const PROMPTS_FILTER_EXTENSIONS: [&str; 2] = ["txt", "md"];

/// The stage on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Source,
    Variables,
}

/// The kind of prompt source picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    File,
    Folder,
    Inline,
    GithubIssues,
}

impl SourceKind {
    /// The kind a preset's allowed source names.
    #[must_use]
    pub fn of(source: &PresetPromptSource) -> Self {
        match source {
            PresetPromptSource::File => Self::File,
            PresetPromptSource::Folder { .. } => Self::Folder,
            PresetPromptSource::Inline => Self::Inline,
            PresetPromptSource::GithubIssues => Self::GithubIssues,
        }
    }

    /// The selector of the kind's radio row.
    #[must_use]
    pub fn selector(self) -> &'static str {
        match self {
            Self::File => "preset-source-kind-file",
            Self::Folder => "preset-source-kind-folder",
            Self::Inline => "preset-source-kind-inline",
            Self::GithubIssues => "preset-source-kind-github_issues",
        }
    }
}

impl Stage {
    /// The stage's name, for the spec harness.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Variables => "variables",
        }
    }
}

/// What a picker's answer fills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    SourceFile,
    SourceFolder,
    VariableFile(String),
    VariableFolder(String),
}

impl Pick {
    /// The pick that fills `variable`, when it is a file or folder path.
    #[must_use]
    pub fn for_variable(variable: &PresetVariable) -> Option<Self> {
        match variable.kind {
            PresetVariableKind::FilePath { .. } => Some(Self::VariableFile(variable.name.clone())),
            PresetVariableKind::FolderPath => Some(Self::VariableFolder(variable.name.clone())),
            _ => None,
        }
    }
}

/// Next as drawn: whether it can be pressed, and its tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextButton {
    pub enabled: bool,
    pub tip: Option<String>,
}

/// The open wizard's state.
#[derive(Debug, Clone)]
pub struct PresetWizard {
    entry: PresetEntry,
    #[expect(
        dead_code,
        reason = "the launch request carries it; Next on the last stage sends none"
    )]
    target: PresetTarget,
    stage: Stage,
    source_kind: SourceKind,
    file_path: Option<String>,
    folder_path: Option<String>,
    inline_text: String,
    issues_spec: String,
    values: HashMap<String, String>,
}

impl PresetWizard {
    /// The wizard for `entry` launched on `target`, on its Source stage.
    #[must_use]
    pub fn new(entry: PresetEntry, target: PresetTarget, source_repo_path: Option<&str>) -> Self {
        let source_kind = entry
            .prompt_sources
            .first()
            .map_or(SourceKind::Inline, SourceKind::of);
        let folder_path =
            source_repo_path.and_then(|repo| default_folder_path(&entry.prompt_sources, repo));
        let values = initial_values(&entry.variables);
        Self {
            entry,
            target,
            stage: Stage::Source,
            source_kind,
            file_path: None,
            folder_path,
            inline_text: String::new(),
            issues_spec: String::new(),
            values,
        }
    }

    #[must_use]
    pub fn stage(&self) -> Stage {
        self.stage
    }

    #[must_use]
    pub fn source_kind(&self) -> SourceKind {
        self.source_kind
    }

    /// Picks `kind` when the preset allows it; returns whether it did.
    pub fn set_source_kind(&mut self, kind: SourceKind) -> bool {
        let allowed = self
            .entry
            .prompt_sources
            .iter()
            .any(|source| SourceKind::of(source) == kind);
        if allowed {
            self.source_kind = kind;
        }
        allowed
    }

    #[must_use]
    pub fn folder_path(&self) -> Option<&str> {
        self.folder_path.as_deref()
    }

    pub fn set_file_path(&mut self, path: Option<String>) {
        self.file_path = path;
    }

    pub fn set_folder_path(&mut self, path: Option<String>) {
        self.folder_path = path;
    }

    pub fn set_inline_text(&mut self, text: String) {
        self.inline_text = text;
    }

    pub fn set_issues_spec(&mut self, spec: String) {
        self.issues_spec = spec;
    }

    /// Whether the picked source can produce prompts: a file or folder is
    /// picked, the inline text parses to a prompt, or the issue spec is
    /// valid.
    #[must_use]
    pub fn is_source_ready(&self) -> bool {
        match self.source_kind {
            SourceKind::File => non_empty(self.file_path.as_deref()).is_some(),
            SourceKind::Folder => non_empty(self.folder_path.as_deref()).is_some(),
            SourceKind::Inline => !parse_prompts(&self.inline_text).is_empty(),
            SourceKind::GithubIssues => parse_issue_spec(&self.issues_spec).is_ok(),
        }
    }

    /// The launch source the picked source produces, once it is ready. An
    /// issue spec goes as typed: the daemon expands it.
    #[must_use]
    #[cfg_attr(not(test), expect(dead_code, reason = "called only from unit tests"))]
    pub fn launch_source(&self) -> Option<LaunchPresetSource> {
        if !self.is_source_ready() {
            return None;
        }
        Some(match self.source_kind {
            SourceKind::File => LaunchPresetSource::File {
                path: self.file_path.clone().unwrap_or_default(),
            },
            SourceKind::Folder => LaunchPresetSource::Folder {
                path: self.folder_path.clone().unwrap_or_default(),
            },
            SourceKind::Inline => LaunchPresetSource::Inline {
                prompts: parse_prompts(&self.inline_text),
            },
            SourceKind::GithubIssues => LaunchPresetSource::GithubIssues {
                spec: self.issues_spec.clone(),
            },
        })
    }

    /// What the line under the issue spec says.
    #[must_use]
    pub fn issues_feedback(&self) -> IssuesFeedback {
        if self.issues_spec.trim().is_empty() {
            return IssuesFeedback::Hint;
        }
        match parse_issue_spec(&self.issues_spec) {
            Ok(issues) => IssuesFeedback::Ok(issue_summary(&issues)),
            Err(err) => IssuesFeedback::Error(err),
        }
    }

    /// The preset this wizard launches.
    #[must_use]
    pub fn entry(&self) -> &PresetEntry {
        &self.entry
    }

    /// The wizard's heading.
    #[must_use]
    pub fn title(&self) -> String {
        format!("Launch preset · {}", self.entry.name)
    }

    #[must_use]
    pub fn file_path(&self) -> Option<&str> {
        self.file_path.as_deref()
    }

    /// The variables the wizard asks for, in the preset's order.
    #[must_use]
    pub fn prompted_variables(&self) -> Vec<&PresetVariable> {
        self.entry
            .variables
            .iter()
            .filter(|variable| variable.prompt_at_launch)
            .collect()
    }

    /// The value `name` holds now, `""` when it holds none.
    #[must_use]
    pub fn value(&self, name: &str) -> &str {
        self.values.get(name).map_or("", String::as_str)
    }

    pub fn set_value(&mut self, name: &str, value: String) {
        self.values.insert(name.to_owned(), value);
    }

    /// Whether toggle `name` is checked: its value is `true` in any case,
    /// as the daemon reads it.
    #[must_use]
    pub fn toggle_on(&self, name: &str) -> bool {
        self.value(name).eq_ignore_ascii_case("true")
    }

    /// Flips toggle `name`, writing `true` or `false`.
    pub fn toggle(&mut self, name: &str) {
        let next = if self.toggle_on(name) {
            "false"
        } else {
            "true"
        };
        self.set_value(name, next.to_owned());
    }

    /// The filter a file variable's picker offers: its extensions under its
    /// label, or every file when it names none.
    #[must_use]
    pub fn variable_filter(&self, name: &str) -> FileFilter {
        let variable = self.entry.variables.iter().find(|v| v.name == name);
        let extensions = match variable.map(|v| &v.kind) {
            Some(PresetVariableKind::FilePath { extensions }) => extensions.clone(),
            _ => Vec::new(),
        };
        FileFilter {
            name: variable.map(|v| v.label.clone()).unwrap_or_default(),
            extensions,
        }
    }

    /// The filter of the file picker `pick` opens; `None` when it opens the
    /// folder picker.
    #[must_use]
    pub fn pick_filter(&self, pick: &Pick) -> Option<FileFilter> {
        match pick {
            Pick::SourceFile => Some(FileFilter::new(
                PROMPTS_FILTER_NAME,
                &PROMPTS_FILTER_EXTENSIONS,
            )),
            Pick::VariableFile(name) => Some(self.variable_filter(name)),
            Pick::SourceFolder | Pick::VariableFolder(_) => None,
        }
    }

    /// The folder the picker for `pick` opens at: the folder source's
    /// folder shown, a folder variable's value when it holds one, and the
    /// picker's own choice for a file.
    #[must_use]
    pub fn pick_start(&self, pick: &Pick) -> Option<PathBuf> {
        match pick {
            Pick::SourceFile | Pick::VariableFile(_) => None,
            Pick::SourceFolder => non_empty(self.folder_path()).map(PathBuf::from),
            Pick::VariableFolder(name) => non_empty(Some(self.value(name))).map(PathBuf::from),
        }
    }

    /// Fills what `pick` asked for with the picked `path`.
    pub fn apply_pick(&mut self, pick: Pick, path: String) {
        match pick {
            Pick::SourceFile => self.set_file_path(Some(path)),
            Pick::SourceFolder => self.set_folder_path(Some(path)),
            Pick::VariableFile(name) | Pick::VariableFolder(name) => self.set_value(&name, path),
        }
    }

    /// The prompted variables still required: not optional, not a toggle,
    /// and empty after trimming.
    #[must_use]
    pub fn missing_required(&self) -> Vec<&PresetVariable> {
        self.prompted_variables()
            .into_iter()
            .filter(|variable| self.is_missing(variable))
            .collect()
    }

    /// Whether `variable` is required and still empty.
    #[must_use]
    pub fn is_missing(&self, variable: &PresetVariable) -> bool {
        variable.prompt_at_launch
            && shows_required(variable)
            && self.value(&variable.name).trim().is_empty()
    }

    /// Next on the stage on screen. Source waits for its source, then moves
    /// to the variables, or is the last Next when none is prompted; on the
    /// variables it names the labels still missing. Next on the last stage
    /// is disabled.
    #[must_use]
    pub fn next_button(&self) -> NextButton {
        let last = || NextButton {
            enabled: false,
            tip: Some(PREVIEW_LATER_TIP.to_owned()),
        };
        match self.stage {
            Stage::Source if !self.is_source_ready() => NextButton {
                enabled: false,
                tip: None,
            },
            Stage::Source if self.prompted_variables().is_empty() => last(),
            Stage::Source => NextButton {
                enabled: true,
                tip: None,
            },
            Stage::Variables => {
                let missing = self.missing_required();
                if missing.is_empty() {
                    return last();
                }
                let labels: Vec<&str> = missing.iter().map(|v| v.label.as_str()).collect();
                NextButton {
                    enabled: false,
                    tip: Some(format!("Fill in: {}", labels.join(", "))),
                }
            }
        }
    }

    /// Moves to the next stage when Next can be pressed; returns whether it
    /// moved.
    pub fn advance(&mut self) -> bool {
        let moves = self.stage == Stage::Source && self.next_button().enabled;
        if moves {
            self.stage = Stage::Variables;
        }
        moves
    }

    /// Moves back a stage; returns whether it moved.
    pub fn back(&mut self) -> bool {
        let moves = self.stage == Stage::Variables;
        if moves {
            self.stage = Stage::Source;
        }
        moves
    }
}

/// What the line under the issue spec says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssuesFeedback {
    /// The spec is empty: how to write one.
    Hint,
    /// The spec is valid: how many issues and sessions it makes.
    Ok(String),
    /// The spec is malformed: why.
    Error(String),
}

/// `text` when it holds something.
fn non_empty(text: Option<&str>) -> Option<&str> {
    text.filter(|text| !text.is_empty())
}

/// The folder source's default: its relative path under the source repo,
/// with the repo path's separators.
#[must_use]
pub fn default_folder_path(allowed: &[PresetPromptSource], repo_path: &str) -> Option<String> {
    allowed.iter().find_map(|source| match source {
        PresetPromptSource::Folder { relative_path } => Some(join_path(repo_path, relative_path)),
        _ => None,
    })
}

/// `rel` under `base`. The preset's relative path is whatever its author
/// wrote, so its separators follow the base's shape (a drive letter or a UNC
/// prefix is Windows) and stray separators at the seam are dropped.
fn join_path(base: &str, rel: &str) -> String {
    let bytes = base.as_bytes();
    let windows = (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || base.starts_with("\\\\");
    let is_sep = |c: char| c == '/' || c == '\\';
    let base = base.trim_end_matches(is_sep);
    let rel = rel.trim_start_matches(is_sep);
    if rel.is_empty() {
        return base.to_owned();
    }
    let (sep, rel) = if windows {
        ('\\', rel.replace('/', "\\"))
    } else {
        ('/', rel.replace('\\', "/"))
    };
    format!("{base}{sep}{rel}")
}

/// Every variable's default, as its starting value.
#[must_use]
pub fn initial_values(variables: &[PresetVariable]) -> HashMap<String, String> {
    variables
        .iter()
        .filter_map(|variable| {
            variable
                .default
                .clone()
                .map(|value| (variable.name.clone(), value))
        })
        .collect()
}

/// Whether a variable is edited in a text field: every kind but a toggle
/// and the two path pickers.
#[must_use]
pub fn takes_text(kind: &PresetVariableKind) -> bool {
    !matches!(
        kind,
        PresetVariableKind::Toggle { .. }
            | PresetVariableKind::FilePath { .. }
            | PresetVariableKind::FolderPath
    )
}

/// Whether a variable's label carries the required note: it is not
/// optional and not a toggle, which always holds a value.
#[must_use]
pub fn shows_required(variable: &PresetVariable) -> bool {
    !variable.optional && !matches!(variable.kind, PresetVariableKind::Toggle { .. })
}

/// The placeholder of a variable's field.
#[must_use]
pub fn variable_placeholder(variable: &PresetVariable) -> String {
    match &variable.kind {
        PresetVariableKind::FilePath { extensions } if extensions.is_empty() => {
            "Pick a file…".to_owned()
        }
        PresetVariableKind::FilePath { extensions } => {
            let names: Vec<&str> = extensions
                .iter()
                .map(|ext| ext.strip_prefix('.').unwrap_or(ext))
                .collect();
            format!("Pick a {} file…", names.join(" / "))
        }
        PresetVariableKind::FolderPath => "Pick a folder…".to_owned(),
        _ => variable.default.clone().unwrap_or_default(),
    }
}

/// A prompt source's radio label.
#[must_use]
pub fn source_label(source: &PresetPromptSource) -> String {
    match source {
        PresetPromptSource::File => "From file".to_owned(),
        PresetPromptSource::Folder { relative_path } => format!("From folder ({relative_path})"),
        PresetPromptSource::GithubIssues => "From GitHub issues".to_owned(),
        PresetPromptSource::Inline => "Inline".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[expect(
        clippy::expect_used,
        clippy::needless_pass_by_value,
        reason = "a test fails with the message of the precondition it lost"
    )]
    fn preset(sources: serde_json::Value, variables: serde_json::Value) -> PresetEntry {
        serde_json::from_value(json!({
            "id": "triage",
            "name": "Triage",
            "source_repo_id": "r1",
            "prompt_sources": sources,
            "variables": variables,
            "prompt_template": "{prompt}",
            "branch_template": "tmp/{index}",
            "tab_grouping": {"kind": "none"},
            "injector": {"startup_delay_ms": 0, "pre_input": [], "post_input": []}
        }))
        .expect("preset fixture")
    }

    fn target() -> PresetTarget {
        PresetTarget::Repo {
            repo_id: "r1".to_owned(),
        }
    }

    fn all_sources() -> serde_json::Value {
        json!([
            {"kind": "file"},
            {"kind": "folder", "relative_path": "docs/prompts"},
            {"kind": "inline"},
            {"kind": "github_issues"}
        ])
    }

    fn wizard(sources: serde_json::Value, variables: serde_json::Value) -> PresetWizard {
        PresetWizard::new(preset(sources, variables), target(), Some("D:\\src\\r1"))
    }

    fn text_variable(name: &str, label: &str) -> serde_json::Value {
        json!({"name": name, "label": label, "kind": {"kind": "text"}, "prompt_at_launch": true})
    }

    fn inline_ready(variables: serde_json::Value) -> PresetWizard {
        let mut wizard = wizard(json!([{"kind": "inline"}]), variables);
        wizard.set_inline_text("- one".to_owned());
        wizard
    }

    #[test]
    fn the_first_allowed_source_is_picked() {
        let wizard = wizard(
            json!([{"kind": "github_issues"}, {"kind": "file"}]),
            json!([]),
        );
        assert_eq!(wizard.source_kind(), SourceKind::GithubIssues);
        assert_eq!(wizard.stage(), Stage::Source);
    }

    #[test]
    fn a_preset_without_sources_starts_inline() {
        assert_eq!(
            wizard(json!([]), json!([])).source_kind(),
            SourceKind::Inline
        );
    }

    #[test]
    fn only_an_allowed_source_can_be_picked() {
        let mut wizard = wizard(json!([{"kind": "file"}, {"kind": "inline"}]), json!([]));
        assert!(!wizard.set_source_kind(SourceKind::GithubIssues));
        assert_eq!(wizard.source_kind(), SourceKind::File);
        assert!(wizard.set_source_kind(SourceKind::Inline));
        assert_eq!(wizard.source_kind(), SourceKind::Inline);
    }

    #[test]
    fn a_file_source_is_ready_once_a_file_is_picked() {
        let mut wizard = wizard(all_sources(), json!([]));
        assert!(!wizard.is_source_ready());
        assert_eq!(wizard.launch_source(), None);
        wizard.set_file_path(Some(String::new()));
        assert!(!wizard.is_source_ready(), "an empty path is no file");
        wizard.set_file_path(Some("D:\\prompts.md".to_owned()));
        assert!(wizard.is_source_ready());
        assert_eq!(
            wizard.launch_source(),
            Some(LaunchPresetSource::File {
                path: "D:\\prompts.md".to_owned()
            })
        );
    }

    #[test]
    fn a_folder_source_is_ready_with_its_default_and_not_once_cleared() {
        let mut wizard = wizard(all_sources(), json!([]));
        assert!(wizard.set_source_kind(SourceKind::Folder));
        assert!(wizard.is_source_ready(), "the default folder is a folder");
        assert_eq!(
            wizard.launch_source(),
            Some(LaunchPresetSource::Folder {
                path: "D:\\src\\r1\\docs\\prompts".to_owned()
            })
        );
        wizard.set_folder_path(None);
        assert!(!wizard.is_source_ready());
        assert_eq!(wizard.launch_source(), None);
    }

    #[test]
    fn an_inline_source_is_ready_once_it_parses_to_a_prompt() {
        let mut wizard = wizard(all_sources(), json!([]));
        assert!(wizard.set_source_kind(SourceKind::Inline));
        assert!(!wizard.is_source_ready());
        wizard.set_inline_text("  \n\n ".to_owned());
        assert!(!wizard.is_source_ready(), "blank text has no prompts");
        wizard.set_inline_text("- one\n- two".to_owned());
        assert!(wizard.is_source_ready());
        assert_eq!(
            wizard.launch_source(),
            Some(LaunchPresetSource::Inline {
                prompts: vec!["one".to_owned(), "two".to_owned()]
            })
        );
    }

    #[test]
    fn a_github_source_is_ready_on_a_valid_spec_and_keeps_it_raw() {
        let mut wizard = wizard(all_sources(), json!([]));
        assert!(wizard.set_source_kind(SourceKind::GithubIssues));
        assert!(!wizard.is_source_ready());
        wizard.set_issues_spec("12-9".to_owned());
        assert!(!wizard.is_source_ready(), "a reversed range is not ready");
        assert_eq!(wizard.launch_source(), None);
        wizard.set_issues_spec(" 7-9, 12 ".to_owned());
        assert!(wizard.is_source_ready());
        assert_eq!(
            wizard.launch_source(),
            Some(LaunchPresetSource::GithubIssues {
                spec: " 7-9, 12 ".to_owned()
            })
        );
    }

    #[test]
    fn the_folder_default_joins_the_repo_path_and_the_relative_path() {
        let folder = |rel: &str| json!([{"kind": "folder", "relative_path": rel}]);
        assert_eq!(
            default_folder_path(
                &preset(folder("docs/prompts"), json!([])).prompt_sources,
                "D:\\src\\r1\\"
            ),
            Some("D:\\src\\r1\\docs\\prompts".to_owned())
        );
        assert_eq!(
            default_folder_path(
                &preset(folder("\\docs\\p"), json!([])).prompt_sources,
                "/home/me/r1/"
            ),
            Some("/home/me/r1/docs/p".to_owned())
        );
        assert_eq!(
            default_folder_path(
                &preset(folder("/a/b"), json!([])).prompt_sources,
                "\\\\host\\share\\r1"
            ),
            Some("\\\\host\\share\\r1\\a\\b".to_owned())
        );
        assert_eq!(
            default_folder_path(
                &preset(folder(""), json!([])).prompt_sources,
                "D:\\src\\r1\\"
            ),
            Some("D:\\src\\r1".to_owned()),
            "an empty relative path is the repo itself"
        );
    }

    #[test]
    fn no_folder_default_without_a_source_repo_or_a_folder_source() {
        let without_repo = PresetWizard::new(preset(all_sources(), json!([])), target(), None);
        assert_eq!(without_repo.folder_path(), None);
        assert_eq!(
            default_folder_path(
                &preset(json!([{"kind": "file"}]), json!([])).prompt_sources,
                "D:\\r1"
            ),
            None
        );
    }

    #[test]
    fn variables_start_at_their_defaults() {
        let values = initial_values(
            &preset(
                json!([]),
                json!([
                    {"name": "branch", "label": "Branch", "kind": {"kind": "text"}, "default": "main"},
                    {"name": "logs", "label": "Logs", "kind": {"kind": "toggle", "value": "x"}, "default": "true"},
                    {"name": "note", "label": "Note", "kind": {"kind": "text"}}
                ]),
            )
            .variables,
        );
        assert_eq!(values.get("branch").map(String::as_str), Some("main"));
        assert_eq!(values.get("logs").map(String::as_str), Some("true"));
        assert_eq!(values.get("note"), None);
    }

    #[test]
    fn a_prompted_empty_text_variable_is_required() {
        let mut wizard = inline_ready(json!([text_variable("ticket", "Ticket")]));
        let names = |wizard: &PresetWizard| -> Vec<String> {
            wizard
                .missing_required()
                .iter()
                .map(|variable| variable.name.clone())
                .collect()
        };
        assert_eq!(names(&wizard), ["ticket"]);
        wizard.set_value("ticket", "   ".to_owned());
        assert_eq!(names(&wizard), ["ticket"], "whitespace is empty");
        wizard.set_value("ticket", "RT-21".to_owned());
        assert_eq!(names(&wizard), Vec::<String>::new());
    }

    #[test]
    fn optional_toggle_and_unprompted_variables_are_never_required() {
        let wizard = inline_ready(json!([
            {"name": "a", "label": "A", "kind": {"kind": "text"}, "prompt_at_launch": true, "optional": true},
            {"name": "b", "label": "B", "kind": {"kind": "toggle", "value": "v"}, "prompt_at_launch": true},
            {"name": "c", "label": "C", "kind": {"kind": "text"}},
            {"name": "d", "label": "D", "kind": {"kind": "file_path", "extensions": ["log"]}, "prompt_at_launch": true}
        ]));
        let missing: Vec<&str> = wizard
            .missing_required()
            .iter()
            .map(|variable| variable.name.as_str())
            .collect();
        assert_eq!(missing, ["d"], "a prompted file path is required like text");
    }

    #[test]
    fn next_waits_for_the_source() {
        let wizard = wizard(
            json!([{"kind": "inline"}]),
            json!([text_variable("t", "T")]),
        );
        assert_eq!(
            wizard.next_button(),
            NextButton {
                enabled: false,
                tip: None
            }
        );
    }

    #[test]
    fn next_moves_to_the_variables_when_some_are_prompted() {
        let mut wizard = inline_ready(json!([text_variable("t", "T")]));
        assert!(wizard.next_button().enabled);
        assert!(wizard.advance());
        assert_eq!(wizard.stage(), Stage::Variables);
    }

    #[test]
    fn the_variables_stage_is_skipped_when_none_is_prompted() {
        let mut wizard = inline_ready(json!([
            {"name": "c", "label": "C", "kind": {"kind": "env_var", "name": "HOME"}}
        ]));
        assert_eq!(
            wizard.next_button(),
            NextButton {
                enabled: false,
                tip: Some(PREVIEW_LATER_TIP.to_owned())
            },
            "Next on Source is the last Next"
        );
        assert!(!wizard.advance());
        assert_eq!(wizard.stage(), Stage::Source);
    }

    #[test]
    fn next_on_the_variables_names_the_missing_labels() {
        let mut wizard = inline_ready(json!([
            text_variable("a", "Ticket"),
            text_variable("b", "Area"),
            text_variable("c", "Owner")
        ]));
        assert!(wizard.advance());
        wizard.set_value("b", "ui".to_owned());
        assert_eq!(
            wizard.next_button(),
            NextButton {
                enabled: false,
                tip: Some("Fill in: Ticket, Owner".to_owned())
            }
        );
        wizard.set_value("a", "RT-21".to_owned());
        wizard.set_value("c", "me".to_owned());
        assert_eq!(
            wizard.next_button(),
            NextButton {
                enabled: false,
                tip: Some(PREVIEW_LATER_TIP.to_owned())
            }
        );
        assert!(!wizard.advance(), "Next on the last stage is disabled");
    }

    fn toggle_variable(name: &str, default: &str) -> serde_json::Value {
        json!({"name": name, "label": name, "kind": {"kind": "toggle", "value": "x"},
               "prompt_at_launch": true, "default": default})
    }

    #[test]
    fn a_toggle_reads_true_in_any_case_and_flips_to_lowercase() {
        let mut wizard = inline_ready(json!([
            toggle_variable("upper", "True"),
            toggle_variable("off", "FALSE"),
            toggle_variable("other", "yes")
        ]));
        assert!(wizard.toggle_on("upper"), "a default of True shows checked");
        assert!(!wizard.toggle_on("off"));
        assert!(!wizard.toggle_on("other"), "only true is on");
        assert!(!wizard.toggle_on("unset"), "no value is off");

        wizard.toggle("upper");
        assert_eq!(wizard.value("upper"), "false");
        wizard.toggle("upper");
        assert_eq!(wizard.value("upper"), "true");
        wizard.toggle("unset");
        assert_eq!(wizard.value("unset"), "true");
    }

    #[expect(
        clippy::expect_used,
        reason = "a test fails with the message of the precondition it lost"
    )]
    fn kind(kind: serde_json::Value) -> PresetVariableKind {
        serde_json::from_value(kind).expect("variable kind fixture")
    }

    #[test]
    fn text_kinds_take_a_text_field_and_toggles_and_paths_do_not() {
        assert!(takes_text(&kind(json!({"kind": "text"}))));
        assert!(takes_text(&kind(
            json!({"kind": "env_var", "name": "HOME"})
        )));
        assert!(!takes_text(&kind(json!({"kind": "toggle", "value": "x"}))));
        assert!(!takes_text(&kind(
            json!({"kind": "file_path", "extensions": ["log"]})
        )));
        assert!(!takes_text(&kind(json!({"kind": "folder_path"}))));
    }

    #[expect(
        clippy::expect_used,
        clippy::needless_pass_by_value,
        reason = "a test fails with the message of the precondition it lost"
    )]
    fn file_variable(extensions: serde_json::Value) -> PresetVariable {
        serde_json::from_value(json!({"name": "f", "label": "F",
            "kind": {"kind": "file_path", "extensions": extensions}}))
        .expect("file variable fixture")
    }

    #[test]
    fn shows_required_on_a_non_optional_variable_but_never_a_toggle() {
        let wizard = wizard(
            all_sources(),
            json!([
                text_variable("ticket", "Ticket"),
                {"name": "note", "label": "Note", "kind": {"kind": "text"},
                 "prompt_at_launch": true, "optional": true},
                toggle_variable("logs", "false")
            ]),
        );
        let shown: Vec<bool> = wizard
            .prompted_variables()
            .into_iter()
            .map(shows_required)
            .collect();
        assert_eq!(
            shown,
            [true, false, false],
            "a non-optional toggle shows no required note"
        );
    }

    #[test]
    fn variable_placeholder_names_the_extensions_without_their_dots() {
        let file = file_variable;
        assert_eq!(
            variable_placeholder(&file(json!([".txt"]))),
            "Pick a txt file…"
        );
        assert_eq!(
            variable_placeholder(&file(json!(["log", ".md"]))),
            "Pick a log / md file…"
        );
        assert_eq!(variable_placeholder(&file(json!([]))), "Pick a file…");
    }

    fn path_variables() -> serde_json::Value {
        json!([
            {"name": "log", "label": "Log file", "kind": {"kind": "file_path", "extensions": ["log", ".txt"]},
             "prompt_at_launch": true},
            {"name": "any", "label": "Any file", "kind": {"kind": "file_path", "extensions": []},
             "prompt_at_launch": true},
            {"name": "out", "label": "Out", "kind": {"kind": "folder_path"}, "prompt_at_launch": true},
            text_variable("note", "Note")
        ])
    }

    #[test]
    fn a_file_variable_filters_by_its_extensions_under_its_label() {
        let wizard = wizard(all_sources(), path_variables());
        assert_eq!(
            wizard.variable_filter("log"),
            FileFilter::new("Log file", &["log", ".txt"])
        );
        assert_eq!(
            wizard.variable_filter("any"),
            FileFilter::new("Any file", &[]),
            "no extensions is every file"
        );
        assert_eq!(wizard.variable_filter("missing"), FileFilter::new("", &[]));
        assert_eq!(
            wizard.pick_filter(&Pick::VariableFile("log".to_owned())),
            Some(FileFilter::new("Log file", &["log", ".txt"]))
        );
        assert_eq!(
            wizard.pick_filter(&Pick::SourceFile),
            Some(FileFilter::new("Prompts", &["txt", "md"]))
        );
        assert_eq!(wizard.pick_filter(&Pick::SourceFolder), None);
        assert_eq!(
            wizard.pick_filter(&Pick::VariableFolder("out".to_owned())),
            None,
            "a folder opens the folder picker"
        );
    }

    #[test]
    fn pickers_start_at_the_folder_shown_or_at_none() {
        let mut wizard = wizard(all_sources(), path_variables());
        wizard.set_file_path(Some("D:\\prompts\\a.md".to_owned()));
        assert_eq!(
            wizard.pick_start(&Pick::SourceFile),
            None,
            "the prompts file picker starts at its own folder"
        );
        assert_eq!(
            wizard.pick_start(&Pick::SourceFolder),
            Some(PathBuf::from("D:\\src\\r1\\docs\\prompts")),
            "the folder source starts at the folder shown"
        );
        wizard.set_folder_path(None);
        assert_eq!(wizard.pick_start(&Pick::SourceFolder), None);

        let out = Pick::VariableFolder("out".to_owned());
        assert_eq!(wizard.pick_start(&out), None, "an empty variable");
        wizard.set_value("out", "E:\\out".to_owned());
        assert_eq!(wizard.pick_start(&out), Some(PathBuf::from("E:\\out")));
    }

    #[test]
    fn a_pick_fills_what_it_asked_for() {
        let mut wizard = wizard(all_sources(), path_variables());
        wizard.apply_pick(Pick::SourceFile, "D:\\a.md".to_owned());
        wizard.apply_pick(Pick::SourceFolder, "D:\\p".to_owned());
        wizard.apply_pick(Pick::VariableFile("log".to_owned()), "D:\\x.log".to_owned());
        wizard.apply_pick(Pick::VariableFolder("out".to_owned()), "E:\\o".to_owned());
        assert_eq!(wizard.file_path(), Some("D:\\a.md"));
        assert_eq!(wizard.folder_path(), Some("D:\\p"));
        assert_eq!(wizard.value("log"), "D:\\x.log");
        assert_eq!(wizard.value("out"), "E:\\o");
    }

    #[test]
    fn only_path_variables_have_a_pick() {
        let wizard = wizard(all_sources(), path_variables());
        let picks: Vec<Option<Pick>> = wizard
            .prompted_variables()
            .into_iter()
            .map(Pick::for_variable)
            .collect();
        assert_eq!(
            picks,
            [
                Some(Pick::VariableFile("log".to_owned())),
                Some(Pick::VariableFile("any".to_owned())),
                Some(Pick::VariableFolder("out".to_owned())),
                None
            ]
        );
    }

    #[test]
    fn back_returns_to_the_source_keeping_the_values() {
        let mut wizard = inline_ready(json!([text_variable("t", "T")]));
        assert!(!wizard.back(), "the source stage is the first");
        assert!(wizard.advance());
        wizard.set_value("t", "kept".to_owned());
        assert!(wizard.back());
        assert_eq!(wizard.stage(), Stage::Source);
        assert!(wizard.advance());
        assert_eq!(wizard.value("t"), "kept");
    }
}
