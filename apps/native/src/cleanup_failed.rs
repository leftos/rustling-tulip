//! The worktree-cleanup-failed dialog's model: the daemon's failures queued
//! one per session, the locking processes ticked for killing, the focused
//! control and the retry they build.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use protocol::{
    ClientMessage, RetryWorktreeTarget, WorktreeCleanupFailure, WorktreeLockingProcess,
};

/// How many characters of a locking process's command line the dialog shows.
pub(crate) const CMDLINE_MAX: usize = 80;

/// The prefix of the per-branch folder a session's worktrees sit in.
const WT_PREFIX: &str = "wt.";

const NO_LOCKERS: &str = "The Restart Manager couldn't identify any specific process holding these dirs open. The \"Kill processes & retry\" option below will just re-run the cleanup — sometimes a transient antivirus or indexer hold clears on the second try.";

/// A control of the dialog, in Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Control {
    /// The ✕ in the title, which ignores.
    Close,
    /// The Open folder button of the failure at this index.
    OpenFolder(usize),
    /// The checkbox of the locking process with this pid.
    Process(u32),
    Ignore,
    Retry,
}

impl Control {
    pub(crate) fn selector(self) -> String {
        match self {
            Self::Close => "cleanup-close".to_owned(),
            Self::OpenFolder(index) => format!("cleanup-open-folder-{index}"),
            Self::Process(pid) => format!("cleanup-proc-{pid}"),
            Self::Ignore => "cleanup-ignore".to_owned(),
            Self::Retry => "cleanup-retry".to_owned(),
        }
    }
}

/// One session's failed cleanup: what is still on disk, who holds it and
/// which of them the user wants killed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CleanupFailed {
    session_id: String,
    label: String,
    failures: Vec<WorktreeCleanupFailure>,
    processes: Vec<WorktreeLockingProcess>,
    ticked: BTreeSet<u32>,
    focused: Control,
}

impl CleanupFailed {
    /// Every locking process once, in the order the failures name them, all
    /// ticked, with Ignore focused.
    pub(crate) fn new(
        session_id: String,
        label: String,
        failures: Vec<WorktreeCleanupFailure>,
    ) -> Self {
        let mut processes: Vec<WorktreeLockingProcess> = Vec::new();
        for process in failures
            .iter()
            .flat_map(|failure| &failure.locking_processes)
        {
            if !processes.iter().any(|seen| seen.pid == process.pid) {
                processes.push(process.clone());
            }
        }
        let ticked = processes.iter().map(|process| process.pid).collect();
        Self {
            session_id,
            label,
            failures,
            processes,
            ticked,
            focused: Control::Ignore,
        }
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn failures(&self) -> &[WorktreeCleanupFailure] {
        &self.failures
    }

    /// The locking processes, each once.
    pub(crate) fn processes(&self) -> &[WorktreeLockingProcess] {
        &self.processes
    }

    pub(crate) fn is_ticked(&self, pid: u32) -> bool {
        self.ticked.contains(&pid)
    }

    /// The ticked pids, in the order the processes are listed.
    pub(crate) fn ticked_pids(&self) -> Vec<u32> {
        self.processes
            .iter()
            .map(|process| process.pid)
            .filter(|pid| self.ticked.contains(pid))
            .collect()
    }

    /// Ticks `pid` when it is unticked, else unticks it.
    pub(crate) fn toggle(&mut self, pid: u32) {
        if !self.ticked.remove(&pid) {
            self.ticked.insert(pid);
        }
    }

    /// What the dialog says under its title.
    pub(crate) fn body(&self) -> String {
        let what = match self.failures.len() {
            1 => "its worktree directory is still on disk".to_owned(),
            count => format!("{count} of its worktree directories are still on disk"),
        };
        format!(
            "Session {} stopped, but {what}. On Windows this usually means a child process (a dev server, a build, your editor) still has files open inside it.",
            self.label
        )
    }

    /// The note shown when no process was found holding the folders.
    pub(crate) fn no_lockers_note(&self) -> Option<&'static str> {
        self.processes.is_empty().then_some(NO_LOCKERS)
    }

    /// The retry button's label, which says what pressing it kills.
    pub(crate) fn retry_label(&self) -> String {
        let all = self.processes.len();
        let ticked = self.ticked_pids().len();
        if all == 0 {
            "Retry cleanup".to_owned()
        } else if ticked == 0 {
            "Retry without killing".to_owned()
        } else if ticked == all {
            let noun = if ticked == 1 { "process" } else { "processes" };
            format!("Kill {ticked} {noun} & retry")
        } else {
            format!("Kill {ticked} selected & retry")
        }
    }

    /// The retry: the ticked pids and every failure as a target.
    pub(crate) fn retry_message(&self) -> ClientMessage {
        ClientMessage::RetryWorktreeCleanup {
            session_id: self.session_id.clone(),
            kill_pids: self.ticked_pids(),
            targets: self
                .failures
                .iter()
                .map(|failure| RetryWorktreeTarget {
                    member_path: failure.member_path.clone(),
                    repo_path: failure.repo_path.clone(),
                })
                .collect(),
        }
    }

    /// The controls in Tab order: ✕, each Open folder, each checkbox,
    /// Ignore and the retry button.
    pub(crate) fn controls(&self) -> Vec<Control> {
        let mut controls = vec![Control::Close];
        controls.extend((0..self.failures.len()).map(Control::OpenFolder));
        controls.extend(
            self.processes
                .iter()
                .map(|process| Control::Process(process.pid)),
        );
        controls.extend([Control::Ignore, Control::Retry]);
        controls
    }

    pub(crate) fn focused(&self) -> Control {
        self.focused
    }

    /// Moves the focus to the next control, or the previous one, wrapping.
    pub(crate) fn move_focus(&mut self, forward: bool) {
        let controls = self.controls();
        let count = controls.len();
        let at = controls
            .iter()
            .position(|control| *control == self.focused)
            .unwrap_or(0);
        let next = if forward {
            (at + 1) % count
        } else {
            (at + count - 1) % count
        };
        self.focused = controls[next];
    }

    /// The label a control shows.
    pub(crate) fn label(&self, control: Control) -> String {
        match control {
            Control::Close => "✕".to_owned(),
            Control::OpenFolder(_) => "Open folder".to_owned(),
            Control::Process(pid) => self
                .processes
                .iter()
                .find(|process| process.pid == pid)
                .map(|process| process.name.clone())
                .unwrap_or_default(),
            Control::Ignore => "Ignore".to_owned(),
            Control::Retry => self.retry_label(),
        }
    }
}

/// The failed cleanups waiting to be shown, one per session; the head is the
/// one on screen.
#[derive(Debug, Default)]
pub(crate) struct CleanupQueue {
    entries: VecDeque<CleanupFailed>,
    /// The label each session was first shown with, kept after its entry
    /// closes, since a retry's failure names the session by its id.
    labels: HashMap<String, String>,
}

impl CleanupQueue {
    /// Queues a session's failure, under the label the session was first
    /// shown with. A session already queued has its entry replaced in place.
    pub(crate) fn push(
        &mut self,
        session_id: String,
        label: String,
        failures: Vec<WorktreeCleanupFailure>,
    ) {
        let label = self
            .labels
            .entry(session_id.clone())
            .or_insert(label)
            .clone();
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.session_id == session_id)
        {
            *entry = CleanupFailed::new(session_id, label, failures);
        } else {
            self.entries
                .push_back(CleanupFailed::new(session_id, label, failures));
        }
    }

    pub(crate) fn head(&self) -> Option<&CleanupFailed> {
        self.entries.front()
    }

    pub(crate) fn head_mut(&mut self) -> Option<&mut CleanupFailed> {
        self.entries.front_mut()
    }

    /// Closes the entry on screen, showing the next.
    pub(crate) fn pop(&mut self) -> Option<CleanupFailed> {
        self.entries.pop_front()
    }

    /// Drops every entry and every remembered label.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.labels.clear();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The folder Open folder reveals for `member_path`: the nearest folder on
/// its path whose name starts with `wt.`, which holds every worktree of the
/// session, else the path itself.
pub(crate) fn open_folder_target(member_path: &str) -> PathBuf {
    let path = Path::new(member_path);
    path.ancestors()
        .find(|ancestor| {
            ancestor
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(WT_PREFIX))
        })
        .unwrap_or(path)
        .to_path_buf()
}

/// `cmdline` cut to [`CMDLINE_MAX`] characters, with `…` when it was cut.
pub(crate) fn truncate_cmdline(cmdline: &str) -> String {
    if cmdline.chars().count() <= CMDLINE_MAX {
        return cmdline.to_owned();
    }
    let mut cut: String = cmdline.chars().take(CMDLINE_MAX).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;

    fn proc(pid: u32, name: &str) -> WorktreeLockingProcess {
        WorktreeLockingProcess {
            pid,
            name: name.to_owned(),
            cmdline: None,
        }
    }

    fn failure(member: &str, processes: Vec<WorktreeLockingProcess>) -> WorktreeCleanupFailure {
        WorktreeCleanupFailure {
            member_path: member.to_owned(),
            repo_path: format!("{member}-repo"),
            reason: "locked".to_owned(),
            locking_processes: processes,
        }
    }

    fn entry(failures: Vec<WorktreeCleanupFailure>) -> CleanupFailed {
        CleanupFailed::new("s1".to_owned(), "feat/x".to_owned(), failures)
    }

    #[test]
    fn unique_pids_all_selected() {
        let dialog = entry(vec![
            failure("a", vec![proc(10, "node.exe"), proc(20, "code.exe")]),
            failure("b", vec![proc(20, "code.exe"), proc(30, "cargo.exe")]),
        ]);
        let pids: Vec<u32> = dialog.processes().iter().map(|p| p.pid).collect();
        assert_eq!(pids, [10, 20, 30]);
        assert_eq!(dialog.ticked_pids(), [10, 20, 30]);
        assert_eq!(dialog.focused(), Control::Ignore);
    }

    #[test]
    fn retry_label_variants() {
        assert_eq!(
            entry(vec![failure("a", vec![])]).retry_label(),
            "Retry cleanup"
        );
        assert_eq!(
            entry(vec![failure("a", vec![proc(1, "a")])]).retry_label(),
            "Kill 1 process & retry"
        );
        let mut dialog = entry(vec![failure(
            "a",
            vec![proc(1, "a"), proc(2, "b"), proc(3, "c")],
        )]);
        assert_eq!(dialog.retry_label(), "Kill 3 processes & retry");
        dialog.toggle(2);
        assert_eq!(dialog.retry_label(), "Kill 2 selected & retry");
        dialog.toggle(1);
        dialog.toggle(3);
        assert_eq!(dialog.retry_label(), "Retry without killing");
        dialog.toggle(3);
        assert_eq!(dialog.retry_label(), "Kill 1 selected & retry");
    }

    #[test]
    fn retry_keeps_first_label() {
        let mut queue = CleanupQueue::default();
        queue.push(
            "s1".to_owned(),
            "feat/x".to_owned(),
            vec![failure("a", vec![])],
        );
        queue.push("s1".to_owned(), "s1".to_owned(), vec![failure("b", vec![])]);
        let head = queue.head().expect("queued");
        assert!(
            head.body()
                .starts_with("Session feat/x stopped, but its worktree")
        );
        assert_eq!(head.failures()[0].member_path, "b");
    }

    #[test]
    fn retry_after_pop_keeps_first_label() {
        let mut queue = CleanupQueue::default();
        queue.push(
            "s1".to_owned(),
            "feat/x".to_owned(),
            vec![failure("a", vec![])],
        );
        queue.pop();
        queue.push("s1".to_owned(), "s1".to_owned(), vec![failure("a", vec![])]);
        let head = queue.head().expect("the retry's failure is queued");
        assert!(head.body().starts_with("Session feat/x stopped"));
        queue.clear();
        queue.push("s1".to_owned(), "s1".to_owned(), vec![failure("a", vec![])]);
        let fresh = queue.head().expect("queued after a clear");
        assert!(
            fresh.body().starts_with("Session s1 stopped"),
            "a clear forgets the labels"
        );
    }

    #[test]
    fn same_session_replaces_queue_entry() {
        let mut queue = CleanupQueue::default();
        queue.push(
            "s1".to_owned(),
            "one".to_owned(),
            vec![failure("a", vec![])],
        );
        queue.push(
            "s2".to_owned(),
            "two".to_owned(),
            vec![failure("b", vec![])],
        );
        queue.push(
            "s2".to_owned(),
            "s2".to_owned(),
            vec![failure("c", vec![]), failure("d", vec![])],
        );
        assert_eq!(queue.head().map(CleanupFailed::session_id), Some("s1"));
        queue.pop();
        let next = queue.head().expect("s2 still queued");
        assert_eq!(next.session_id(), "s2");
        assert!(next.body().starts_with(
            "Session two stopped, but 2 of its worktree directories are still on disk. On Windows"
        ));
        queue.pop();
        assert!(queue.is_empty());
    }

    #[test]
    fn retry_message_carries_ticked_pids_and_every_target() {
        let mut dialog = entry(vec![
            failure("a", vec![proc(10, "x"), proc(20, "y")]),
            failure("b", vec![proc(30, "z")]),
        ]);
        dialog.toggle(20);
        let ClientMessage::RetryWorktreeCleanup {
            session_id,
            kill_pids,
            targets,
        } = dialog.retry_message()
        else {
            unreachable!("the retry is a RetryWorktreeCleanup");
        };
        assert_eq!(session_id, "s1");
        assert_eq!(kill_pids, [10, 30]);
        let expected = [
            RetryWorktreeTarget {
                member_path: "a".to_owned(),
                repo_path: "a-repo".to_owned(),
            },
            RetryWorktreeTarget {
                member_path: "b".to_owned(),
                repo_path: "b-repo".to_owned(),
            },
        ];
        assert_eq!(targets, expected);
    }

    #[test]
    fn open_folder_targets_parent_wt_dir() {
        assert_eq!(
            open_folder_target("C:/wts/wt.feat-x/code/repo"),
            PathBuf::from("C:/wts/wt.feat-x")
        );
        assert_eq!(
            open_folder_target("C:/wts/wt.feat-x"),
            PathBuf::from("C:/wts/wt.feat-x")
        );
        assert_eq!(
            open_folder_target("C:/elsewhere/repo"),
            PathBuf::from("C:/elsewhere/repo")
        );
    }

    #[test]
    fn command_line_truncates_at_80() {
        let exact = "a".repeat(CMDLINE_MAX);
        assert_eq!(truncate_cmdline(&exact), exact);
        let long = format!("{exact}bcd");
        assert_eq!(truncate_cmdline(&long), format!("{exact}…"));
        let wide = "é".repeat(CMDLINE_MAX + 1);
        assert_eq!(
            truncate_cmdline(&wide),
            format!("{}…", "é".repeat(CMDLINE_MAX))
        );
    }
}
