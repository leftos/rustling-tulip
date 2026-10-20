//! Preset launching in plain Rust: the preview's grouping by tab, its rows
//! and summary, the max-panes field's rule, the script commands the preview
//! shows before they run, the Launch button's label, and the registry of the
//! launch jobs this window started, which folds the daemon's job events into
//! the toast each job shows.

use std::collections::HashMap;

use protocol::{
    DaemonMessage, PresetEntry, PresetLaunchJobSnapshot, PresetLaunchJobStatus, PresetVariable,
    PresetVariableKind, TabGroupingConfig,
};

use crate::notices::{ToastKind, ToastSpec};

/// The most characters of a prompt's first line a preview row shows.
pub const FIRST_LINE_PREVIEW_LIMIT: usize = 80;
pub const PREVIEW_LOADING: &str = "Resolving prompts…";
pub const PREVIEW_EMPTY: &str = "No prompts found in the chosen source.";
pub const MAX_PER_TAB_LABEL: &str = "Max panes per tab";
pub const MAX_PER_TAB_PLACEHOLDER: &str = "(leave blank for the preset default)";
pub const MAX_PER_TAB_INVALID: &str = "Max panes per tab must be a whole number of 1 or more";
pub const LAUNCH_CAVEAT: &str = "Launch closes this dialog; progress shows in a notification. Cancel stops before the next session spawns; sessions already created stay.";
pub const SCRIPTS_LEGEND: &str = "Scripts to run";
pub const SCRIPTS_RUNNING_NOTE: &str = " · running…";
pub const SCRIPTS_DONE_NOTE: &str = " · done";
pub const SCRIPTS_NOTE: &str = "These commands run on the daemon before any session spawns. They execute with the repo as their working directory.";
pub const SCRIPT_UNRESOLVED: &str = "Unable to render command — fix earlier variable inputs.";

/// The preview of the prompts a launch would spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preview {
    /// Waiting on the daemon's answer to request `id`.
    Loading {
        id: String,
    },
    Ready(Vec<String>),
    Failed(String),
}

/// One row of the preview: the prompt's 1-based place and its first line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRow {
    pub index: usize,
    pub line: String,
}

impl PreviewRow {
    /// The row as drawn: `[index] line`.
    #[must_use]
    pub fn text(&self) -> String {
        format!("[{}] {}", self.index, self.line)
    }
}

/// The line over the preview list: waiting, the reason it failed, or the
/// summary.
#[must_use]
pub fn preview_line(preview: &Preview, summary: Option<String>) -> String {
    match preview {
        Preview::Loading { .. } => PREVIEW_LOADING.to_owned(),
        Preview::Failed(err) => format!("Could not resolve prompts: {err}"),
        Preview::Ready(_) => summary.unwrap_or_default(),
    }
}

/// The max-panes field read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxPerTab {
    /// Blank: the preset's own cap.
    Default,
    Cap(u32),
    /// Not a whole number of 1 or more.
    Invalid,
}

impl MaxPerTab {
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let text = text.trim();
        if text.is_empty() {
            return Self::Default;
        }
        match text.parse::<u32>() {
            Ok(cap) if cap >= 1 => Self::Cap(cap),
            _ => Self::Invalid,
        }
    }

    /// The cap typed, when one is.
    #[must_use]
    pub fn cap(self) -> Option<u32> {
        match self {
            Self::Cap(cap) => Some(cap),
            Self::Default | Self::Invalid => None,
        }
    }
}

/// Whether the preset packs its sessions into tabs.
#[must_use]
pub fn groups_by_tab(entry: &PresetEntry) -> bool {
    matches!(entry.tab_grouping, TabGroupingConfig::NewTab { .. })
}

/// The preset's own cap on panes per tab, 0 read as 1 as the daemon reads
/// it; None when unlimited or ungrouped.
#[must_use]
pub fn default_cap(entry: &PresetEntry) -> Option<u32> {
    match entry.tab_grouping {
        TabGroupingConfig::NewTab {
            max_panes_per_tab, ..
        } => max_panes_per_tab.map(|cap| cap.max(1)),
        TabGroupingConfig::None => None,
    }
}

/// The muted note after the max-panes label.
#[must_use]
pub fn max_per_tab_note(entry: &PresetEntry) -> String {
    match default_cap(entry) {
        Some(cap) => format!(" · default {cap}"),
        None => " · default unlimited".to_owned(),
    }
}

/// The cap a launch packs tabs by: the field's when it holds one, else the
/// preset's; None without tab grouping.
#[must_use]
pub fn effective_cap(entry: &PresetEntry, field: MaxPerTab) -> Option<u32> {
    if !groups_by_tab(entry) {
        return None;
    }
    match field {
        MaxPerTab::Cap(cap) => Some(cap),
        MaxPerTab::Default | MaxPerTab::Invalid => default_cap(entry),
    }
}

/// The prompts in the tabs they land in, `cap` to a tab in order, as the
/// daemon packs them; one group without a cap.
#[must_use]
pub fn group_by_tab(prompts: &[String], cap: Option<u32>) -> Vec<Vec<PreviewRow>> {
    let rows: Vec<PreviewRow> = prompts
        .iter()
        .enumerate()
        .map(|(at, prompt)| PreviewRow {
            index: at + 1,
            line: first_line_preview(prompt),
        })
        .collect();
    let size = cap.map_or(usize::MAX, |cap| {
        usize::try_from(cap.max(1)).unwrap_or(usize::MAX)
    });
    if rows.len() <= size {
        return vec![rows];
    }
    rows.chunks(size).map(<[PreviewRow]>::to_vec).collect()
}

/// The header over the prompts of tab `at` (from 0) of `count`.
#[must_use]
pub fn tab_header(at: usize, count: usize) -> String {
    format!("Tab {} of {count}", at + 1)
}

/// A prompt's first line, cut to [`FIRST_LINE_PREVIEW_LIMIT`] characters
/// with `...` when longer.
#[must_use]
pub fn first_line_preview(prompt: &str) -> String {
    let first = prompt.split('\n').next().unwrap_or_default();
    if first.chars().count() <= FIRST_LINE_PREVIEW_LIMIT {
        return first.to_owned();
    }
    let kept: String = first.chars().take(FIRST_LINE_PREVIEW_LIMIT - 3).collect();
    format!("{kept}...")
}

/// `count` and `noun`, plural unless one.
fn counted(count: impl Into<u64>, noun: &str) -> String {
    let count = count.into();
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// `noun`, plural unless `count` is one.
fn noun_for(count: impl Into<u64>, noun: &str) -> String {
    if count.into() == 1 {
        noun.to_owned()
    } else {
        format!("{noun}s")
    }
}

/// The line over the preview list; `tabs` only with tab grouping.
#[must_use]
pub fn preview_summary(count: usize, tabs: Option<usize>) -> String {
    let count = count as u64;
    let line = format!("{} ready to spawn.", counted(count, "prompt"));
    match tabs.filter(|_| count > 0) {
        Some(tabs) => format!("{line} → {}.", counted(tabs as u64, "tab")),
        None => line,
    }
}

/// Where the script variables' run stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptStage {
    Idle,
    /// Waiting on the daemon's answer to request `id`.
    Running {
        id: String,
    },
    Failed {
        error: String,
    },
    Done,
}

impl ScriptStage {
    /// The line under the script commands after a failed run.
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        match self {
            Self::Failed { error } => Some(format!("Script resolution failed: {error}")),
            _ => None,
        }
    }

    /// The note after the scripts' legend.
    #[must_use]
    pub fn legend_note(&self) -> Option<&'static str> {
        match self {
            Self::Running { .. } => Some(SCRIPTS_RUNNING_NOTE),
            Self::Done => Some(SCRIPTS_DONE_NOTE),
            Self::Idle | Self::Failed { .. } => None,
        }
    }
}

/// The Launch button's label.
#[must_use]
pub fn launch_button_label(stage: &ScriptStage, scripts: usize, prompts: usize) -> String {
    let sessions = counted(prompts as u64, "session");
    let script = if scripts == 1 { "script" } else { "scripts" };
    match stage {
        ScriptStage::Running { .. } => "Running scripts…".to_owned(),
        ScriptStage::Failed { .. } => format!("Retry {script} & launch {sessions}"),
        ScriptStage::Idle | ScriptStage::Done if scripts == 0 => format!("Launch {sessions}"),
        ScriptStage::Idle | ScriptStage::Done => format!("Run {script} & launch {sessions}"),
    }
}

/// A script command as the preview shows it, before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptLine {
    /// The command and its arguments, `{name}` placeholders filled in.
    Ready(String),
    /// Skipped because the guard variable named is empty.
    Skipped(String),
    /// A placeholder names nothing known.
    Unresolved,
}

impl ScriptLine {
    /// The muted note after the command's label.
    #[must_use]
    pub fn note(&self) -> Option<String> {
        match self {
            Self::Skipped(guard) => Some(format!(" · skipped ({guard} empty)")),
            Self::Ready(_) | Self::Unresolved => None,
        }
    }
}

/// A script variable's command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptCommand {
    pub name: String,
    pub label: String,
    pub line: ScriptLine,
}

/// The preset's script variables, rendered against `values`; `repo_root`
/// fills `{repo_root}`.
#[must_use]
pub fn script_commands(
    variables: &[PresetVariable],
    values: &HashMap<String, String>,
    repo_root: Option<&str>,
) -> Vec<ScriptCommand> {
    variables
        .iter()
        .filter_map(|variable| {
            let PresetVariableKind::Script {
                cmd,
                args,
                skip_if_empty,
                ..
            } = &variable.kind
            else {
                return None;
            };
            Some(ScriptCommand {
                name: variable.name.clone(),
                label: variable.label.clone(),
                line: script_line(cmd, args, skip_if_empty.as_deref(), values, repo_root),
            })
        })
        .collect()
}

fn script_line(
    cmd: &str,
    args: &[String],
    guard: Option<&str>,
    values: &HashMap<String, String>,
    repo_root: Option<&str>,
) -> ScriptLine {
    if let Some(guard) = guard
        && values
            .get(guard)
            .is_none_or(|value| value.trim().is_empty())
    {
        return ScriptLine::Skipped(guard.to_owned());
    }
    let mut parts = vec![cmd.to_owned()];
    for arg in args {
        match interpolate(arg, values, repo_root) {
            Some(part) => parts.push(part),
            None => return ScriptLine::Unresolved,
        }
    }
    ScriptLine::Ready(parts.join(" "))
}

/// `raw` with each `{name}` filled from `values` or `{repo_root}`; None
/// when one names nothing known. A `{` without its `}` stays as written.
fn interpolate(
    raw: &str,
    values: &HashMap<String, String>,
    repo_root: Option<&str>,
) -> Option<String> {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return Some(out);
        };
        let name = &after[..end];
        if name == "repo_root" {
            out.push_str(repo_root?);
        } else {
            out.push_str(values.get(name)?);
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// The key of a launch job's toast.
#[must_use]
pub fn toast_key(job_id: &str) -> String {
    format!("preset:{job_id}")
}

/// Where a launch job stands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum JobState {
    Running,
    Completed,
    Cancelled,
    /// Failed for the reason given; empty while no message named one.
    Failed(String),
}

/// A launch job this window started.
#[derive(Debug, Clone)]
struct LaunchJob {
    preset: String,
    grouped: bool,
    cap: Option<u32>,
    total: u32,
    launched: u32,
    tabs_created: usize,
    state: JobState,
    /// The user closed the progress toast: only the end shows again.
    progress_dismissed: bool,
}

impl LaunchJob {
    /// The tabs the launch fills.
    fn tabs(&self) -> u32 {
        self.cap.map_or(1, |cap| self.total.div_ceil(cap.max(1)))
    }

    fn fold_snapshot(&mut self, job: &PresetLaunchJobSnapshot) {
        self.total = job.total;
        self.launched = job.launched;
        self.tabs_created = job.created_tab_ids.len();
        self.state = match job.status {
            PresetLaunchJobStatus::Completed => JobState::Completed,
            PresetLaunchJobStatus::Cancelled => JobState::Cancelled,
            PresetLaunchJobStatus::Failed => {
                JobState::Failed(job.error.clone().unwrap_or_default())
            }
            PresetLaunchJobStatus::Resolving
            | PresetLaunchJobStatus::RunningScripts
            | PresetLaunchJobStatus::Spawning
            | PresetLaunchJobStatus::Unknown => JobState::Running,
        };
    }

    fn progress_detail(&self) -> Option<String> {
        if self.total == 0 {
            return None;
        }
        let sessions = format!(
            "{} of {} {}",
            self.launched,
            self.total,
            noun_for(self.total, "session")
        );
        if !self.grouped {
            return Some(sessions);
        }
        let tabs = self.tabs();
        Some(format!(
            "{sessions} · {} of {tabs} {}",
            self.tabs_created,
            noun_for(tabs, "tab")
        ))
    }

    fn toast(&self, job_id: &str) -> ToastSpec {
        let preset = &self.preset;
        let (kind, title, detail, sticky) = match &self.state {
            JobState::Running => (
                ToastKind::Info,
                format!("Launching {preset}"),
                self.progress_detail(),
                true,
            ),
            JobState::Completed => {
                let sessions = counted(self.total, "session");
                let detail = if self.grouped {
                    format!("{sessions} in {}", counted(self.tabs_created as u64, "tab"))
                } else {
                    sessions
                };
                (
                    ToastKind::Info,
                    format!("Launched {preset}"),
                    Some(detail),
                    false,
                )
            }
            JobState::Cancelled => (
                ToastKind::Info,
                format!("Cancelled {preset}"),
                Some(format!(
                    "after {} of {} {}",
                    self.launched,
                    self.total,
                    noun_for(self.total, "session")
                )),
                false,
            ),
            JobState::Failed(reason) => (
                ToastKind::Warning,
                format!("Preset launch failed: {preset}"),
                (!reason.is_empty()).then(|| reason.clone()),
                false,
            ),
        };
        ToastSpec {
            kind,
            title,
            detail,
            key: Some(toast_key(job_id)),
            sticky,
        }
    }
}

/// The launch jobs this window started, by job id. The daemon broadcasts
/// every client's job events; only these jobs' events count.
#[derive(Debug, Default)]
pub struct PresetLaunches {
    jobs: HashMap<String, LaunchJob>,
}

impl PresetLaunches {
    /// Records job `job_id` of preset `preset`, sent just now, packing its
    /// sessions `cap` to a tab when `grouped`; returns its first toast.
    pub fn start(
        &mut self,
        job_id: String,
        preset: String,
        grouped: bool,
        cap: Option<u32>,
    ) -> ToastSpec {
        let job = LaunchJob {
            preset,
            grouped,
            cap: cap.filter(|_| grouped),
            total: 0,
            launched: 0,
            tabs_created: 0,
            state: JobState::Running,
            progress_dismissed: false,
        };
        let spec = job.toast(&job_id);
        self.jobs.insert(job_id, job);
        spec
    }

    /// Folds a job event of one of these jobs; returns the toast it now
    /// shows, or None when the message changes nothing shown.
    pub fn on_message(&mut self, msg: &DaemonMessage) -> Option<ToastSpec> {
        let job_id = match msg {
            DaemonMessage::PresetLaunchJobUpdated { job } => &job.job_id,
            DaemonMessage::PresetLaunchProgress { job_id, .. }
            | DaemonMessage::PresetLaunchFailed { job_id, .. } => job_id,
            _ => return None,
        };
        let launch = self.jobs.get_mut(job_id)?;
        match msg {
            DaemonMessage::PresetLaunchJobUpdated { job } if launch.state == JobState::Running => {
                launch.fold_snapshot(job);
            }
            DaemonMessage::PresetLaunchProgress {
                total,
                launched,
                tab_ids,
                ..
            } if launch.state == JobState::Running => {
                launch.total = *total;
                launch.launched = *launched;
                launch.tabs_created = tab_ids.len();
            }
            DaemonMessage::PresetLaunchFailed { error, .. } => {
                let known = matches!(&launch.state, JobState::Failed(reason) if !reason.is_empty());
                let ended = matches!(launch.state, JobState::Completed | JobState::Cancelled);
                if known || ended {
                    return None;
                }
                launch.state = JobState::Failed(error.clone());
            }
            _ => return None,
        }
        (launch.state != JobState::Running || !launch.progress_dismissed)
            .then(|| launch.toast(job_id))
    }

    /// How far the job of toast `key` has come, 0 to 1, while it runs.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a fraction from 0 to 1 fits an f32"
    )]
    pub fn progress(&self, key: &str) -> Option<f32> {
        let job_id = key.strip_prefix("preset:")?;
        let job = self.jobs.get(job_id)?;
        if job.state != JobState::Running {
            return None;
        }
        if job.total == 0 {
            return Some(0.0);
        }
        Some((f64::from(job.launched) / f64::from(job.total)).min(1.0) as f32)
    }

    /// The user closed toast `key`: when it is a running job's progress, its
    /// later progress stays hidden and only the job's end shows.
    pub fn dismiss(&mut self, key: &str) {
        let job = key
            .strip_prefix("preset:")
            .and_then(|job_id| self.jobs.get_mut(job_id));
        if let Some(job) = job
            && job.state == JobState::Running
        {
            job.progress_dismissed = true;
        }
    }

    /// Forgets every job, as a new connection must; returns their toasts'
    /// keys.
    pub fn clear(&mut self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .jobs
            .drain()
            .map(|(job_id, _)| toast_key(&job_id))
            .collect();
        keys.sort();
        keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{PresetLaunchJobSnapshot, PresetLaunchJobStatus, PresetTarget};

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn prompts_group_by_the_cap_in_order() {
        let prompts = strings(&["1", "2", "3", "4", "5", "6", "7"]);
        let groups = group_by_tab(&prompts, Some(3));
        let indices: Vec<Vec<usize>> = groups
            .iter()
            .map(|group| group.iter().map(|row| row.index).collect())
            .collect();
        assert_eq!(indices, [vec![1, 2, 3], vec![4, 5, 6], vec![7]]);
        assert_eq!(groups[2][0].line, "7");
        assert_eq!(group_by_tab(&prompts, None).len(), 1, "no cap");
        assert_eq!(
            group_by_tab(&prompts, Some(7)).len(),
            1,
            "a cap the prompts fit in"
        );
    }

    #[test]
    fn a_preview_row_shows_the_first_line_cut_at_eighty() {
        assert_eq!(first_line_preview("fix it\nsecond line"), "fix it");
        let eighty = "a".repeat(80);
        assert_eq!(first_line_preview(&eighty), eighty);
        let long = format!("{}é tail", "b".repeat(80));
        assert_eq!(first_line_preview(&long), format!("{}...", "b".repeat(77)));
        assert_eq!(first_line_preview(""), "");
    }

    #[test]
    fn max_panes_reads_blank_as_the_default_and_rejects_non_positive_and_fractions() {
        assert_eq!(MaxPerTab::parse(""), MaxPerTab::Default);
        assert_eq!(MaxPerTab::parse("  "), MaxPerTab::Default);
        assert_eq!(MaxPerTab::parse(" 4 "), MaxPerTab::Cap(4));
        for bad in ["0", "-1", "2.5", "x", "3x"] {
            assert_eq!(MaxPerTab::parse(bad), MaxPerTab::Invalid, "{bad:?}");
        }
    }

    #[test]
    fn the_launch_button_names_the_scripts_and_the_sessions() {
        let failed = ScriptStage::Failed {
            error: "boom".to_owned(),
        };
        let running = ScriptStage::Running {
            id: "r1".to_owned(),
        };
        assert_eq!(
            launch_button_label(&ScriptStage::Idle, 0, 1),
            "Launch 1 session"
        );
        assert_eq!(
            launch_button_label(&ScriptStage::Idle, 0, 4),
            "Launch 4 sessions"
        );
        assert_eq!(
            launch_button_label(&ScriptStage::Idle, 1, 4),
            "Run script & launch 4 sessions"
        );
        assert_eq!(
            launch_button_label(&ScriptStage::Idle, 2, 1),
            "Run scripts & launch 1 session"
        );
        assert_eq!(launch_button_label(&running, 2, 4), "Running scripts…");
        assert_eq!(
            launch_button_label(&failed, 1, 4),
            "Retry script & launch 4 sessions"
        );
        assert_eq!(
            launch_button_label(&failed, 3, 4),
            "Retry scripts & launch 4 sessions"
        );
    }

    #[test]
    fn script_commands_render_their_args_skip_on_an_empty_guard_and_flag_unknowns() {
        let variables: Vec<protocol::PresetVariable> = serde_json::from_value(serde_json::json!([
            {"name": "mins", "label": "Minutes", "kind": {"kind": "text"}},
            {"name": "logs", "label": "Fetch logs", "kind": {"kind": "script", "cmd": "fetch",
             "args": ["--since", "{mins}", "{repo_root}/out"], "skip_if_empty": "mins"}},
            {"name": "bad", "label": "Bad", "kind": {"kind": "script", "cmd": "x", "args": ["{nope}"]}},
            {"name": "raw", "label": "Raw", "kind": {"kind": "script", "cmd": "date", "args": ["{open"]}}
        ]))
        .unwrap_or_default();
        assert_eq!(variables.len(), 4, "the fixture parses");
        let mut values = std::collections::HashMap::new();
        let lines = |values: &std::collections::HashMap<String, String>| -> Vec<ScriptLine> {
            script_commands(&variables, values, Some("D:\\r1"))
                .into_iter()
                .map(|command| command.line)
                .collect()
        };
        assert_eq!(
            lines(&values),
            [
                ScriptLine::Skipped("mins".to_owned()),
                ScriptLine::Unresolved,
                ScriptLine::Ready("date {open".to_owned())
            ]
        );
        values.insert("mins".to_owned(), "15".to_owned());
        assert_eq!(
            lines(&values)[0],
            ScriptLine::Ready("fetch --since 15 D:\\r1/out".to_owned())
        );
        assert_eq!(
            script_commands(&variables, &values, None)[0].line,
            ScriptLine::Unresolved,
            "no repo root to put in"
        );
    }

    fn snapshot(job_id: &str, status: PresetLaunchJobStatus) -> PresetLaunchJobSnapshot {
        PresetLaunchJobSnapshot {
            job_id: job_id.to_owned(),
            preset_id: "sweep".to_owned(),
            target: PresetTarget::Repo {
                repo_id: "r1".to_owned(),
            },
            total: 6,
            launched: 0,
            created_session_ids: Vec::new(),
            created_tab_ids: Vec::new(),
            status,
            error: None,
            current_tab_id: None,
        }
    }

    fn updated(job: PresetLaunchJobSnapshot) -> DaemonMessage {
        DaemonMessage::PresetLaunchJobUpdated { job }
    }

    fn progress(job_id: &str, launched: u32, tabs: &[&str]) -> DaemonMessage {
        DaemonMessage::PresetLaunchProgress {
            job_id: job_id.to_owned(),
            preset_id: "sweep".to_owned(),
            total: 6,
            launched,
            current_tab_id: tabs.last().map(|tab| (*tab).to_owned()),
            tab_ids: strings(tabs),
        }
    }

    fn failed(job_id: &str, error: &str) -> DaemonMessage {
        DaemonMessage::PresetLaunchFailed {
            job_id: job_id.to_owned(),
            preset_id: "sweep".to_owned(),
            error: error.to_owned(),
            partial_session_ids: Vec::new(),
            partial_tab_ids: Vec::new(),
        }
    }

    fn texts(spec: &ToastSpec) -> (ToastKind, &str, Option<&str>, bool) {
        (
            spec.kind,
            spec.title.as_str(),
            spec.detail.as_deref(),
            spec.sticky,
        )
    }

    #[test]
    fn the_registry_ignores_jobs_this_window_did_not_launch() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        let foreign = snapshot("other", PresetLaunchJobStatus::Spawning);
        assert_eq!(launches.on_message(&updated(foreign)), None);
        assert_eq!(launches.on_message(&progress("other", 1, &[])), None);
        assert_eq!(launches.on_message(&failed("other", "boom")), None);
    }

    #[test]
    fn the_registry_folds_progress_into_one_sticky_toast() {
        let mut launches = PresetLaunches::default();
        let first = launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        assert_eq!(
            texts(&first),
            (ToastKind::Info, "Launching Sweep", None, true),
            "no count before the daemon knows the total"
        );
        assert_eq!(first.key.as_deref(), Some("preset:j1"));
        let spawning = launches
            .on_message(&updated(snapshot("j1", PresetLaunchJobStatus::Spawning)))
            .unwrap_or(first.clone());
        assert_eq!(
            spawning.detail.as_deref(),
            Some("0 of 6 sessions · 0 of 2 tabs")
        );
        let third = launches
            .on_message(&progress("j1", 3, &["t1"]))
            .unwrap_or(first.clone());
        assert_eq!(
            texts(&third),
            (
                ToastKind::Info,
                "Launching Sweep",
                Some("3 of 6 sessions · 1 of 2 tabs"),
                true
            )
        );
        assert_eq!(third.key, first.key, "updated in place");
        assert_eq!(launches.progress("preset:j1"), Some(0.5));
        assert_eq!(launches.progress("preset:other"), None);
    }

    #[test]
    fn a_completed_job_is_replaced_by_a_timed_toast() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        let mut done = snapshot("j1", PresetLaunchJobStatus::Completed);
        done.launched = 6;
        done.created_tab_ids = strings(&["t1", "t2"]);
        let spec = launches.on_message(&updated(done));
        assert_eq!(
            spec.as_ref().map(texts),
            Some((
                ToastKind::Info,
                "Launched Sweep",
                Some("6 sessions in 2 tabs"),
                false
            ))
        );
        assert_eq!(launches.progress("preset:j1"), None, "no bar once done");
        assert_eq!(
            launches.on_message(&progress("j1", 6, &["t1", "t2"])),
            None,
            "a late event changes nothing"
        );

        let mut flat = PresetLaunches::default();
        flat.start("j2".to_owned(), "Flat".to_owned(), false, None);
        let mut one = snapshot("j2", PresetLaunchJobStatus::Spawning);
        one.total = 1;
        assert_eq!(
            flat.on_message(&updated(one.clone()))
                .and_then(|spec| spec.detail),
            Some("0 of 1 session".to_owned()),
            "no tabs part without grouping"
        );
        one.status = PresetLaunchJobStatus::Completed;
        one.launched = 1;
        assert_eq!(
            flat.on_message(&updated(one)).and_then(|spec| spec.detail),
            Some("1 session".to_owned())
        );
    }

    #[test]
    fn a_cancelled_job_says_how_far_it_got() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        let mut cancelled = snapshot("j1", PresetLaunchJobStatus::Cancelled);
        cancelled.launched = 2;
        assert_eq!(
            launches.on_message(&updated(cancelled)).as_ref().map(texts),
            Some((
                ToastKind::Info,
                "Cancelled Sweep",
                Some("after 2 of 6 sessions"),
                false
            ))
        );
    }

    #[test]
    fn a_failed_job_and_its_failure_message_give_one_warning() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        let mut job = snapshot("j1", PresetLaunchJobStatus::Failed);
        job.error = Some("no such folder".to_owned());
        assert_eq!(
            launches.on_message(&updated(job)).as_ref().map(texts),
            Some((
                ToastKind::Warning,
                "Preset launch failed: Sweep",
                Some("no such folder"),
                false
            ))
        );
        assert_eq!(launches.on_message(&failed("j1", "no such folder")), None);

        launches.start("j2".to_owned(), "Other".to_owned(), false, None);
        assert_eq!(
            launches
                .on_message(&updated(snapshot("j2", PresetLaunchJobStatus::Failed)))
                .and_then(|spec| spec.detail),
            None,
            "the snapshot named no reason"
        );
        assert_eq!(
            launches
                .on_message(&failed("j2", "spawn refused"))
                .and_then(|spec| spec.detail),
            Some("spawn refused".to_owned()),
            "the failure message fills the reason in on the same toast"
        );
        assert_eq!(launches.on_message(&failed("j2", "spawn refused")), None);
    }

    #[test]
    fn clearing_forgets_every_job_and_names_their_toasts() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        assert_eq!(launches.clear(), ["preset:j1"]);
        assert_eq!(
            launches.on_message(&progress("j1", 1, &[])),
            None,
            "a forgotten job is foreign"
        );
    }

    #[expect(
        clippy::expect_used,
        reason = "a test fails with the message of the precondition it lost"
    )]
    fn zero_cap_preset() -> PresetEntry {
        serde_json::from_value(serde_json::json!({
            "id": "sweep", "name": "Sweep", "source_repo_id": "r1",
            "prompt_sources": [{"kind": "inline"}], "variables": [],
            "prompt_template": "{prompt}", "branch_template": "tmp/{index}",
            "tab_grouping": {"kind": "new_tab", "layout": "auto_grid", "max_panes_per_tab": 0},
            "injector": {"startup_delay_ms": 0, "pre_input": [], "post_input": []}
        }))
        .expect("preset fixture")
    }

    #[test]
    fn zero_preset_cap_groups_one_per_tab() {
        let entry = zero_cap_preset();
        assert_eq!(default_cap(&entry), Some(1), "the daemon reads 0 as 1");
        assert_eq!(effective_cap(&entry, MaxPerTab::Default), Some(1));
        assert_eq!(max_per_tab_note(&entry), " · default 1");
        let prompts = strings(&["a", "b", "c"]);
        assert_eq!(
            group_by_tab(&prompts, Some(0)).len(),
            3,
            "never a zero chunk"
        );
        assert_eq!(group_by_tab(&prompts, Some(1)).len(), 3);
    }

    #[test]
    fn zero_preset_cap_job_tabs_do_not_panic() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(0));
        let mut job = snapshot("j1", PresetLaunchJobStatus::Spawning);
        job.total = 3;
        assert_eq!(
            launches
                .on_message(&updated(job))
                .and_then(|spec| spec.detail),
            Some("0 of 3 sessions · 0 of 3 tabs".to_owned())
        );
    }

    #[test]
    fn a_failure_message_before_the_failed_snapshot_gives_one_warning() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        assert_eq!(
            launches
                .on_message(&failed("j1", "boom"))
                .as_ref()
                .map(texts),
            Some((
                ToastKind::Warning,
                "Preset launch failed: Sweep",
                Some("boom"),
                false
            ))
        );
        let mut job = snapshot("j1", PresetLaunchJobStatus::Failed);
        job.error = Some("boom".to_owned());
        assert_eq!(launches.on_message(&updated(job)), None);
    }

    #[test]
    fn a_dismissed_progress_toast_stays_dismissed_until_the_job_ends() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        launches.dismiss("preset:j1");
        launches.dismiss("preset:unknown");
        assert_eq!(launches.on_message(&progress("j1", 2, &["t1"])), None);
        assert_eq!(
            launches.on_message(&updated(snapshot("j1", PresetLaunchJobStatus::Spawning))),
            None
        );
        assert_eq!(
            launches.progress("preset:j1"),
            Some(0.0),
            "still folded while hidden"
        );
        let mut done = snapshot("j1", PresetLaunchJobStatus::Completed);
        done.launched = 6;
        assert_eq!(
            launches.on_message(&updated(done)).map(|spec| spec.title),
            Some("Launched Sweep".to_owned()),
            "the end still shows once"
        );
        assert_eq!(launches.on_message(&failed("j1", "late")), None);
    }

    #[test]
    fn a_tab_header_counts_from_one() {
        assert_eq!(tab_header(0, 3), "Tab 1 of 3");
        assert_eq!(tab_header(2, 3), "Tab 3 of 3");
    }
}
