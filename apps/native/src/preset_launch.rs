//! Preset launching in plain Rust: the preview's grouping by tab, its rows
//! and summary, the max-panes field's rule, the script commands the preview
//! shows before they run, the Launch button's label, and the registry of the
//! launch jobs this window started, which folds the daemon's job events into
//! the toast each job shows.

use std::collections::{HashMap, HashSet};

use protocol::{
    DaemonMessage, PresetEntry, PresetLaunchJobSnapshot, PresetLaunchJobStatus, PresetVariable,
    PresetVariableKind, TabGroupingConfig,
};

use crate::notices::{ToastAction, ToastKind, ToastSpec};

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

/// What every launch job's toast key starts with.
const TOAST_PREFIX: &str = "preset:";

/// The key of a launch job's toast.
#[must_use]
pub fn toast_key(job_id: &str) -> String {
    format!("{TOAST_PREFIX}{job_id}")
}

/// The job id of launch toast key `key`; None for another toast's key.
#[must_use]
pub fn job_id_of(key: &str) -> Option<&str> {
    key.strip_prefix(TOAST_PREFIX)
}

/// Whether `msg` can change a launch job or what its toast offers: a job
/// event, or a session or tab change. Terminal output and the rest cannot.
#[must_use]
pub fn touches_launches(msg: &DaemonMessage) -> bool {
    matches!(
        msg,
        DaemonMessage::Sessions { .. }
            | DaemonMessage::SessionUpdated { .. }
            | DaemonMessage::SessionRemoved { .. }
            | DaemonMessage::Tabs { .. }
            | DaemonMessage::TabUpdated { .. }
            | DaemonMessage::PresetLaunchJobUpdated { .. }
            | DaemonMessage::PresetLaunchProgress { .. }
            | DaemonMessage::PresetLaunchFailed { .. }
    )
}

/// The job a job event is about.
fn job_of_message(msg: &DaemonMessage) -> Option<&str> {
    match msg {
        DaemonMessage::PresetLaunchJobUpdated { job } => Some(&job.job_id),
        DaemonMessage::PresetLaunchProgress { job_id, .. }
        | DaemonMessage::PresetLaunchFailed { job_id, .. } => Some(job_id),
        _ => None,
    }
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

/// What a launch job knows of its first tab.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FirstTab {
    /// No message named a tab yet.
    Unknown,
    /// Named, not yet handed out to be shown.
    Known(String),
    /// Handed out; it is never shown again.
    Shown,
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
    /// The sessions the job made, partial ones of a failure included, in
    /// the order first heard.
    session_ids: Vec<String>,
    /// The first tab the job made, as far as it is known and shown.
    first_tab: FirstTab,
    /// A cancel went out for the running job; its end is still to come.
    cancel_requested: bool,
}

/// The toast button that cancels a running launch.
pub const CANCEL: ToastAction = ToastAction {
    id: "cancel",
    label: "Cancel",
    danger: true,
};
/// The toast button that marks the launch's sessions in the sidebar.
pub const SELECT: ToastAction = ToastAction {
    id: "select",
    label: "Select",
    danger: false,
};
/// The toast button that stops the launch's live sessions.
pub const STOP_ALL: ToastAction = ToastAction {
    id: "stop-all",
    label: "Stop all",
    danger: true,
};

/// What a press on a launch toast's button does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobAction {
    /// Cancel the running job of this id.
    Cancel(String),
    /// Mark the job's sessions, following it as it makes more.
    Select,
    /// Cancel the running job `cancel` names first, then stop these live
    /// sessions.
    StopAll {
        cancel: Option<String>,
        sessions: Vec<String>,
    },
}

impl LaunchJob {
    /// Notes the sessions and the first tab job event `msg` names.
    fn fold_ids_of(&mut self, msg: &DaemonMessage) {
        match msg {
            DaemonMessage::PresetLaunchJobUpdated { job } => {
                self.fold_ids(&job.created_session_ids, &job.created_tab_ids);
            }
            DaemonMessage::PresetLaunchProgress { tab_ids, .. } => self.fold_ids(&[], tab_ids),
            DaemonMessage::PresetLaunchFailed {
                partial_session_ids,
                partial_tab_ids,
                ..
            } => self.fold_ids(partial_session_ids, partial_tab_ids),
            _ => {}
        }
    }

    /// Folds job event `msg`'s state and counts; returns whether its toast
    /// changes. Only a running job takes a snapshot or progress.
    fn fold_event(&mut self, msg: &DaemonMessage) -> bool {
        let running = self.state == JobState::Running;
        match msg {
            DaemonMessage::PresetLaunchJobUpdated { job } if running => {
                self.fold_snapshot(job);
                true
            }
            DaemonMessage::PresetLaunchProgress {
                total,
                launched,
                tab_ids,
                ..
            } if running => {
                self.total = *total;
                self.launched = *launched;
                self.tabs_created = tab_ids.len();
                true
            }
            DaemonMessage::PresetLaunchFailed { error, .. } => self.fold_failure(error),
            _ => false,
        }
    }

    /// A failure message: fails the job unless it ended otherwise or its
    /// failure already has a reason; returns whether it did.
    fn fold_failure(&mut self, error: &str) -> bool {
        let known = matches!(&self.state, JobState::Failed(reason) if !reason.is_empty());
        let ended = matches!(self.state, JobState::Completed | JobState::Cancelled);
        if known || ended {
            return false;
        }
        self.state = JobState::Failed(error.to_owned());
        true
    }

    /// Marks the running job cancel-requested; returns whether a cancel is
    /// to go out: not once it ended, nor a second time.
    fn request_cancel(&mut self) -> bool {
        let send = self.state == JobState::Running && !self.cancel_requested;
        self.cancel_requested |= send;
        send
    }

    /// Stop all: the live sessions, after a cancel when the job still runs;
    /// None with nothing live to stop.
    fn stop_all(&mut self, job_id: &str, live: &dyn Fn(&str) -> bool) -> Option<JobAction> {
        let sessions: Vec<String> = self
            .session_ids
            .iter()
            .filter(|id| live(id))
            .cloned()
            .collect();
        if sessions.is_empty() {
            return None;
        }
        let cancel = self.request_cancel().then(|| job_id.to_owned());
        Some(JobAction::StopAll { cancel, sessions })
    }

    /// Notes the sessions and the first tab a message names.
    fn fold_ids(&mut self, sessions: &[String], tabs: &[String]) {
        for id in sessions {
            if !self.session_ids.contains(id) {
                self.session_ids.push(id.clone());
            }
        }
        if self.first_tab == FirstTab::Unknown
            && let Some(tab) = tabs.first()
        {
            self.first_tab = FirstTab::Known(tab.clone());
        }
    }

    /// The toast's buttons: Cancel while it runs and no cancel went out,
    /// Select, and Stop all while one of its sessions is `live`.
    fn actions(&self, live: &dyn Fn(&str) -> bool) -> Vec<ToastAction> {
        let cancel = (self.state == JobState::Running && !self.cancel_requested).then_some(CANCEL);
        let stop = self
            .session_ids
            .iter()
            .any(|id| live(id))
            .then_some(STOP_ALL);
        cancel.into_iter().chain([SELECT]).chain(stop).collect()
    }

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

    fn toast(&self, job_id: &str, live: &dyn Fn(&str) -> bool) -> ToastSpec {
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
                true,
            ),
        };
        ToastSpec {
            kind,
            title,
            detail,
            key: Some(toast_key(job_id)),
            sticky,
            actions: self.actions(live),
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
            session_ids: Vec::new(),
            first_tab: FirstTab::Unknown,
            cancel_requested: false,
        };
        let spec = job.toast(&job_id, &|_| false);
        self.jobs.insert(job_id, job);
        spec
    }

    /// Folds a job event of one of these jobs; returns the toast it now
    /// shows, its Stop all offered while a session is `live`, or None when
    /// the message changes nothing shown. Session and tab ids fold in
    /// whatever the job's state.
    pub fn on_message(
        &mut self,
        live: &dyn Fn(&str) -> bool,
        msg: &DaemonMessage,
    ) -> Option<ToastSpec> {
        let job_id = job_of_message(msg)?;
        let launch = self.jobs.get_mut(job_id)?;
        launch.fold_ids_of(msg);
        if !launch.fold_event(msg) {
            return None;
        }
        (launch.state != JobState::Running || !launch.progress_dismissed)
            .then(|| launch.toast(job_id, live))
    }

    /// Each job's toast key and its buttons, Stop all offered while one of
    /// its sessions is `live`; sorted by key.
    #[must_use]
    pub fn shown_actions(&self, live: &dyn Fn(&str) -> bool) -> Vec<(String, Vec<ToastAction>)> {
        let mut shown: Vec<(String, Vec<ToastAction>)> = self
            .jobs
            .iter()
            .map(|(job_id, job)| (toast_key(job_id), job.actions(live)))
            .collect();
        shown.sort_by(|a, b| a.0.cmp(&b.0));
        shown
    }

    /// A press of button `action` on toast `key`: what it does, or None when
    /// the toast is no job's or the job no longer offers it. Cancel goes
    /// out once, only while the job runs, and so does Stop all's cancel;
    /// Stop all needs a session `live`.
    pub fn press(
        &mut self,
        key: &str,
        action: &str,
        live: &dyn Fn(&str) -> bool,
    ) -> Option<JobAction> {
        let job_id = job_id_of(key)?;
        let job = self.jobs.get_mut(job_id)?;
        match action {
            id if id == CANCEL.id => job
                .request_cancel()
                .then(|| JobAction::Cancel(job_id.to_owned())),
            id if id == SELECT.id => Some(JobAction::Select),
            id if id == STOP_ALL.id => job.stop_all(job_id, live),
            _ => None,
        }
    }

    /// The sessions the job of toast `key` made so far; none for a key no
    /// job of these has.
    #[must_use]
    pub fn session_ids(&self, key: &str) -> Vec<String> {
        job_id_of(key)
            .and_then(|job_id| self.jobs.get(job_id))
            .map(|job| job.session_ids.clone())
            .unwrap_or_default()
    }

    /// The first tab of each tab-grouping job whose first tab is known and
    /// not yet handed out; each job's is handed out once.
    pub fn take_first_tabs(&mut self) -> Vec<String> {
        let mut tabs: Vec<String> = self
            .jobs
            .values_mut()
            .filter(|job| job.grouped)
            .filter_map(
                |job| match std::mem::replace(&mut job.first_tab, FirstTab::Shown) {
                    FirstTab::Known(tab) => Some(tab),
                    unknown_or_shown => {
                        job.first_tab = unknown_or_shown;
                        None
                    }
                },
            )
            .collect();
        tabs.sort();
        tabs
    }

    /// Whether the job of toast `key` is one of these.
    #[must_use]
    pub fn knows(&self, key: &str) -> bool {
        job_id_of(key).is_some_and(|job_id| self.jobs.contains_key(job_id))
    }

    /// Drops the ended jobs whose toast is no longer `shown` (closed or timed
    /// out); a running job stays, its end still to show.
    pub fn retain_shown(&mut self, shown: impl Fn(&str) -> bool) {
        self.jobs
            .retain(|job_id, job| job.state == JobState::Running || shown(&toast_key(job_id)));
    }

    /// How far the job of toast `key` has come, 0 to 1, while it runs.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a fraction from 0 to 1 fits an f32"
    )]
    pub fn progress(&self, key: &str) -> Option<f32> {
        let job = self.jobs.get(job_id_of(key)?)?;
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
        let job = job_id_of(key).and_then(|job_id| self.jobs.get_mut(job_id));
        if let Some(job) = job
            && job.state == JobState::Running
        {
            job.progress_dismissed = true;
        }
    }

    /// Forgets the running jobs, whose progress a new connection no longer
    /// reports; returns their toasts' keys. Ended jobs keep their toasts.
    pub fn drop_running(&mut self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .jobs
            .iter()
            .filter(|(_, job)| job.state == JobState::Running)
            .map(|(job_id, _)| toast_key(job_id))
            .collect();
        self.jobs.retain(|_, job| job.state != JobState::Running);
        keys.sort();
        keys
    }
}

/// The sessions a launch toast's Select marked: the selected job's, kept
/// up as it makes more, until cleared. A session it marked stays marked
/// after the job leaves the registry.
#[derive(Debug, Default)]
pub struct LaunchHighlight {
    /// The toast key of the job the marks follow.
    job: Option<String>,
    sessions: HashSet<String>,
}

impl LaunchHighlight {
    /// Marks the sessions of the job of toast `key`, in place of any marks.
    pub fn select(&mut self, key: &str, launches: &PresetLaunches) {
        self.job = Some(key.to_owned());
        self.sessions = launches.session_ids(key).into_iter().collect();
    }

    /// Marks the sessions the followed job made since.
    pub fn follow(&mut self, launches: &PresetLaunches) {
        if let Some(key) = &self.job {
            self.sessions.extend(launches.session_ids(key));
        }
    }

    /// Drops every mark and follows no job.
    pub fn clear(&mut self) {
        self.job = None;
        self.sessions.clear();
    }

    /// Whether session `id` is marked.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.sessions.contains(id)
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
        assert_eq!(launches.on_message(&all_live, &updated(foreign)), None);
        assert_eq!(
            launches.on_message(&all_live, &progress("other", 1, &[])),
            None
        );
        assert_eq!(
            launches.on_message(&all_live, &failed("other", "boom")),
            None
        );
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
            .on_message(
                &all_live,
                &updated(snapshot("j1", PresetLaunchJobStatus::Spawning)),
            )
            .unwrap_or(first.clone());
        assert_eq!(
            spawning.detail.as_deref(),
            Some("0 of 6 sessions · 0 of 2 tabs")
        );
        let third = launches
            .on_message(&all_live, &progress("j1", 3, &["t1"]))
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
        let spec = launches.on_message(&all_live, &updated(done));
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
            launches.on_message(&all_live, &progress("j1", 6, &["t1", "t2"])),
            None,
            "a late event changes nothing"
        );

        let mut flat = PresetLaunches::default();
        flat.start("j2".to_owned(), "Flat".to_owned(), false, None);
        let mut one = snapshot("j2", PresetLaunchJobStatus::Spawning);
        one.total = 1;
        assert_eq!(
            flat.on_message(&all_live, &updated(one.clone()))
                .and_then(|spec| spec.detail),
            Some("0 of 1 session".to_owned()),
            "no tabs part without grouping"
        );
        one.status = PresetLaunchJobStatus::Completed;
        one.launched = 1;
        assert_eq!(
            flat.on_message(&all_live, &updated(one))
                .and_then(|spec| spec.detail),
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
            launches
                .on_message(&all_live, &updated(cancelled))
                .as_ref()
                .map(texts),
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
            launches
                .on_message(&all_live, &updated(job))
                .as_ref()
                .map(texts),
            Some((
                ToastKind::Warning,
                "Preset launch failed: Sweep",
                Some("no such folder"),
                true
            )),
            "a failure stays until closed"
        );
        assert_eq!(
            launches.on_message(&all_live, &failed("j1", "no such folder")),
            None
        );

        launches.start("j2".to_owned(), "Other".to_owned(), false, None);
        assert_eq!(
            launches
                .on_message(
                    &all_live,
                    &updated(snapshot("j2", PresetLaunchJobStatus::Failed))
                )
                .and_then(|spec| spec.detail),
            None,
            "the snapshot named no reason"
        );
        assert_eq!(
            launches
                .on_message(&all_live, &failed("j2", "spawn refused"))
                .and_then(|spec| spec.detail),
            Some("spawn refused".to_owned()),
            "the failure message fills the reason in on the same toast"
        );
        assert_eq!(
            launches.on_message(&all_live, &failed("j2", "spawn refused")),
            None
        );
    }

    #[test]
    fn a_new_connection_forgets_running_jobs_and_keeps_finished_ones() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        launches.start("j2".to_owned(), "Done".to_owned(), false, None);
        launches.on_message(
            &all_live,
            &updated(snapshot("j2", PresetLaunchJobStatus::Completed)),
        );
        assert_eq!(launches.drop_running(), ["preset:j1"]);
        assert_eq!(
            launches.on_message(&all_live, &progress("j1", 1, &[])),
            None,
            "a forgotten job is foreign"
        );
        assert!(
            launches.press("preset:j2", SELECT.id, &all_live).is_some(),
            "the finished job keeps its buttons"
        );
    }

    fn all_live(_: &str) -> bool {
        true
    }

    fn with_sessions(mut job: PresetLaunchJobSnapshot, ids: &[&str]) -> PresetLaunchJobSnapshot {
        job.created_session_ids = strings(ids);
        job
    }

    fn ids(actions: &[ToastAction]) -> Vec<&'static str> {
        actions.iter().map(|action| action.id).collect()
    }

    #[test]
    fn the_buttons_follow_the_job_and_its_live_sessions() {
        let mut launches = PresetLaunches::default();
        let first = launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        assert_eq!(
            ids(&first.actions),
            ["cancel", "select"],
            "nothing to stop yet"
        );
        let spawning = with_sessions(snapshot("j1", PresetLaunchJobStatus::Spawning), &["s1"]);
        let spec = launches.on_message(&all_live, &updated(spawning));
        assert_eq!(
            spec.map(|spec| ids(&spec.actions)),
            Some(vec!["cancel", "select", "stop-all"])
        );
        assert_eq!(
            [CANCEL.label, SELECT.label, STOP_ALL.label],
            ["Cancel", "Select", "Stop all"]
        );
        assert_eq!(
            [CANCEL.danger, SELECT.danger, STOP_ALL.danger],
            [true, false, true],
            "Cancel and Stop all are destructive"
        );
        let none_live = |_: &str| false;
        let shown = launches.shown_actions(&none_live);
        assert_eq!(
            shown
                .iter()
                .map(|(key, actions)| (key.as_str(), ids(actions)))
                .collect::<Vec<_>>(),
            [("preset:j1", vec!["cancel", "select"])],
            "Stop all goes once nothing it would stop runs"
        );
        let done = with_sessions(snapshot("j1", PresetLaunchJobStatus::Completed), &["s1"]);
        let spec = launches.on_message(&all_live, &updated(done));
        assert_eq!(
            spec.map(|spec| ids(&spec.actions)),
            Some(vec!["select", "stop-all"]),
            "an ended job drops Cancel"
        );
    }

    #[test]
    fn session_ids_fold_after_the_job_ends() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        let mut msg = failed("j1", "boom");
        if let DaemonMessage::PresetLaunchFailed {
            partial_session_ids,
            ..
        } = &mut msg
        {
            *partial_session_ids = strings(&["s1"]);
        }
        launches.on_message(&all_live, &msg);
        let late = with_sessions(snapshot("j1", PresetLaunchJobStatus::Failed), &["s1", "s2"]);
        assert_eq!(launches.on_message(&all_live, &updated(late)), None);
        assert_eq!(
            launches.session_ids("preset:j1"),
            strings(&["s1", "s2"]),
            "the partial ids and the later snapshot's, once each"
        );
        assert_eq!(
            launches.press("preset:j1", SELECT.id, &all_live),
            Some(JobAction::Select)
        );
    }

    #[test]
    fn a_pressed_cancel_goes_until_the_job_ends() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        assert_eq!(
            launches.press("preset:j1", CANCEL.id, &all_live),
            Some(JobAction::Cancel("j1".to_owned()))
        );
        assert_eq!(
            ids(&launches.shown_actions(&all_live)[0].1),
            ["select"],
            "Cancel goes once pressed"
        );
        assert_eq!(
            launches.press("preset:j1", CANCEL.id, &all_live),
            None,
            "a second press sends nothing"
        );
        let still = with_sessions(snapshot("j1", PresetLaunchJobStatus::Spawning), &["s1"]);
        let spec = launches.on_message(&all_live, &updated(still));
        assert_eq!(
            spec.map(|spec| ids(&spec.actions)),
            Some(vec!["select", "stop-all"]),
            "a snapshot of the still running job does not bring it back"
        );
    }

    #[test]
    fn stop_all_on_a_running_job_cancels_it_first_and_stops_the_live_sessions() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        let job = with_sessions(
            snapshot("j1", PresetLaunchJobStatus::Spawning),
            &["s1", "s2"],
        );
        launches.on_message(&all_live, &updated(job));
        let s2_live = |id: &str| id == "s2";
        assert_eq!(
            launches.press("preset:j1", STOP_ALL.id, &s2_live),
            Some(JobAction::StopAll {
                cancel: Some("j1".to_owned()),
                sessions: strings(&["s2"])
            })
        );
        assert_eq!(
            launches.press("preset:j1", CANCEL.id, &all_live),
            None,
            "Stop all already asked for the cancel"
        );
        assert_eq!(
            launches.press("preset:j1", STOP_ALL.id, &s2_live),
            Some(JobAction::StopAll {
                cancel: None,
                sessions: strings(&["s2"])
            }),
            "the cancel is asked once"
        );
    }

    #[test]
    fn stop_all_on_an_ended_job_only_stops_and_cancel_needs_a_running_job() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        assert_eq!(launches.press("preset:j1", "nope", &all_live), None);
        assert_eq!(launches.press("preset:other", SELECT.id, &all_live), None);
        let done = with_sessions(snapshot("j1", PresetLaunchJobStatus::Cancelled), &["s1"]);
        launches.on_message(&all_live, &updated(done));
        assert_eq!(
            launches.press("preset:j1", CANCEL.id, &all_live),
            None,
            "nothing to cancel once it ended"
        );
        assert_eq!(
            launches.press("preset:j1", STOP_ALL.id, &all_live),
            Some(JobAction::StopAll {
                cancel: None,
                sessions: strings(&["s1"])
            })
        );
        assert_eq!(
            launches.press("preset:j1", STOP_ALL.id, &|_| false),
            None,
            "nothing live to stop"
        );
    }

    #[test]
    fn the_highlight_follows_the_selected_job_until_cleared() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        launches.start("j2".to_owned(), "Other".to_owned(), false, None);
        let fold = |launches: &mut PresetLaunches, job_id: &str, ids: &[&str]| {
            let job = with_sessions(snapshot(job_id, PresetLaunchJobStatus::Spawning), ids);
            launches.on_message(&all_live, &updated(job));
        };
        fold(&mut launches, "j1", &["s1"]);
        fold(&mut launches, "j2", &["s9"]);
        let mut highlight = LaunchHighlight::default();
        highlight.select("preset:j1", &launches);
        assert!(highlight.contains("s1") && !highlight.contains("s9"));
        fold(&mut launches, "j1", &["s1", "s2"]);
        highlight.follow(&launches);
        assert!(highlight.contains("s2"), "a session made after the click");
        highlight.select("preset:j2", &launches);
        assert!(
            !highlight.contains("s1") && highlight.contains("s9"),
            "another job's Select replaces it"
        );
        highlight.clear();
        fold(&mut launches, "j2", &["s9", "s10"]);
        highlight.follow(&launches);
        assert!(
            !highlight.contains("s9") && !highlight.contains("s10"),
            "a cleared highlight follows nothing"
        );
    }

    #[test]
    fn only_session_tab_and_job_messages_touch_the_launches() {
        let output = DaemonMessage::PtyOutput {
            session_id: "s1".to_owned(),
            data_b64: String::new(),
        };
        assert!(!touches_launches(&output), "a terminal frame");
        assert!(touches_launches(&progress("j1", 1, &[])));
        assert!(touches_launches(&failed("j1", "boom")));
        assert!(touches_launches(&DaemonMessage::SessionRemoved {
            session_id: "s1".to_owned()
        }));
        assert!(touches_launches(&DaemonMessage::Tabs { tabs: Vec::new() }));
    }

    #[test]
    fn the_first_tab_is_handed_out_once_and_only_with_tab_grouping() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        launches.start("j2".to_owned(), "Flat".to_owned(), false, None);
        assert!(launches.take_first_tabs().is_empty(), "no tab known yet");
        launches.on_message(&all_live, &progress("j1", 1, &["t1"]));
        launches.on_message(&all_live, &progress("j2", 1, &["t9"]));
        assert_eq!(launches.take_first_tabs(), ["t1"]);
        launches.on_message(&all_live, &progress("j1", 4, &["t1", "t2"]));
        assert!(launches.take_first_tabs().is_empty(), "once per job");
    }

    #[test]
    fn a_finished_job_leaves_when_its_toast_is_gone() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        launches.start("j2".to_owned(), "Done".to_owned(), false, None);
        launches.on_message(
            &all_live,
            &updated(snapshot("j2", PresetLaunchJobStatus::Completed)),
        );
        launches.retain_shown(|_| false);
        assert!(
            launches.press("preset:j1", SELECT.id, &all_live).is_some(),
            "a running job stays with its toast closed"
        );
        assert_eq!(launches.press("preset:j2", SELECT.id, &all_live), None);
        assert!(launches.knows("preset:j1"));
        assert!(!launches.knows("preset:j2"), "the closed job is gone");
        assert!(!launches.knows("j1"), "a key, not a job id");
        assert_eq!(job_id_of(&toast_key("j1")), Some("j1"));
        assert_eq!(job_id_of("unc-refused:host"), None);
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
                .on_message(&all_live, &updated(job))
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
                .on_message(&all_live, &failed("j1", "boom"))
                .as_ref()
                .map(texts),
            Some((
                ToastKind::Warning,
                "Preset launch failed: Sweep",
                Some("boom"),
                true
            ))
        );
        let mut job = snapshot("j1", PresetLaunchJobStatus::Failed);
        job.error = Some("boom".to_owned());
        assert_eq!(launches.on_message(&all_live, &updated(job)), None);
    }

    #[test]
    fn a_dismissed_progress_toast_stays_dismissed_until_the_job_ends() {
        let mut launches = PresetLaunches::default();
        launches.start("j1".to_owned(), "Sweep".to_owned(), true, Some(3));
        launches.dismiss("preset:j1");
        launches.dismiss("preset:unknown");
        assert_eq!(
            launches.on_message(&all_live, &progress("j1", 2, &["t1"])),
            None
        );
        assert_eq!(
            launches.on_message(
                &all_live,
                &updated(snapshot("j1", PresetLaunchJobStatus::Spawning))
            ),
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
            launches
                .on_message(&all_live, &updated(done))
                .map(|spec| spec.title),
            Some("Launched Sweep".to_owned()),
            "the end still shows once"
        );
        assert_eq!(launches.on_message(&all_live, &failed("j1", "late")), None);
    }

    #[test]
    fn a_tab_header_counts_from_one() {
        assert_eq!(tab_header(0, 3), "Tab 1 of 3");
        assert_eq!(tab_header(2, 3), "Tab 3 of 3");
    }
}
