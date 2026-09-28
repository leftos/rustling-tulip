//! The Manage worktrees modal's model: the worktrees root's snapshot, the
//! requests out for it, the expanded member lists, the focused control, and
//! the delete and share confirms with what they send or launch.

use std::collections::BTreeSet;

use protocol::{ClientMessage, RootWorktreeEntry, RootWorktreeMember, RootWorktreeStatus};

use crate::spawn_form::{human_relative_time, human_size};

pub(crate) const TITLE: &str = "Manage worktrees";
pub(crate) const SCANNING: &str = "Scanning worktrees root…";
pub(crate) const EMPTY: &str = "No managed worktrees under this root.";
const NOT_CONNECTED: &str = "(daemon not connected)";
const DELETE_TITLE: &str = "Confirm delete";
const DELETE_HINT: &str = "The daemon will run git worktree remove --force per member where the originating repo is reachable, and fall back to filesystem delete otherwise.";
const SHARE_TITLE: &str = "Share this worktree?";
const SHARE_HINT: &str = "They will see each other's uncommitted edits, and concurrent writes to the same file will overwrite one another.";
const UNKNOWN_LAUNCH: &str = "this build doesn't know how to launch this group";
/// The prefix of the per-branch folder; an entry that is that folder itself
/// is an older layout with no group marker.
const WT_PREFIX: &str = "wt.";

/// A control of the manager, in Tab order; a row's controls name its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Control {
    Refresh,
    DeleteStale,
    /// The members toggle of the row at this path.
    Members(String),
    Launch(String),
    Delete(String),
    Close,
}

/// The two buttons of a confirm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmButton {
    Cancel,
    Ok,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfirmKind {
    Delete {
        paths: Vec<String>,
        bytes: u64,
        bulk: bool,
    },
    Share {
        path: String,
        slug: String,
    },
}

/// A confirm over the manager: a delete, or a launch into a group a session
/// is running in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Confirm {
    kind: ConfirmKind,
    focused: ConfirmButton,
}

impl Confirm {
    fn new(kind: ConfirmKind) -> Self {
        Self {
            kind,
            focused: ConfirmButton::Cancel,
        }
    }

    pub(crate) fn title(&self) -> &'static str {
        match self.kind {
            ConfirmKind::Delete { .. } => DELETE_TITLE,
            ConfirmKind::Share { .. } => SHARE_TITLE,
        }
    }

    pub(crate) fn body(&self) -> String {
        match &self.kind {
            ConfirmKind::Delete { paths, bytes, bulk } => {
                let what = if *bulk {
                    let count = paths.len();
                    let plural = if count == 1 { "" } else { "s" };
                    format!("Delete {count} stale worktree group{plural}?")
                } else {
                    "Delete this worktree group?".to_owned()
                };
                if *bytes > 0 {
                    format!("{what} Frees ~{} on disk.", human_size(*bytes))
                } else {
                    what
                }
            }
            ConfirmKind::Share { slug, .. } => format!(
                "A session is already running in {slug}. Launching here puts a second agent in the same working tree."
            ),
        }
    }

    pub(crate) fn hint(&self) -> &'static str {
        match self.kind {
            ConfirmKind::Delete { .. } => DELETE_HINT,
            ConfirmKind::Share { .. } => SHARE_HINT,
        }
    }

    pub(crate) fn label(&self, button: ConfirmButton) -> &'static str {
        match (button, &self.kind) {
            (ConfirmButton::Cancel, _) => "Cancel",
            (ConfirmButton::Ok, ConfirmKind::Delete { .. }) => "Delete",
            (ConfirmButton::Ok, ConfirmKind::Share { .. }) => "Launch anyway",
        }
    }

    pub(crate) fn selector(&self, button: ConfirmButton) -> &'static str {
        match (button, &self.kind) {
            (ConfirmButton::Cancel, ConfirmKind::Delete { .. }) => {
                "worktrees-manager-confirm-cancel"
            }
            (ConfirmButton::Ok, ConfirmKind::Delete { .. }) => "worktrees-manager-confirm-ok",
            (ConfirmButton::Cancel, ConfirmKind::Share { .. }) => "worktrees-manager-share-cancel",
            (ConfirmButton::Ok, ConfirmKind::Share { .. }) => "worktrees-manager-share-ok",
        }
    }

    pub(crate) fn focused(&self) -> ConfirmButton {
        self.focused
    }

    /// Tab or Shift+Tab: the other button.
    pub(crate) fn toggle_focus(&mut self) {
        self.focused = match self.focused {
            ConfirmButton::Cancel => ConfirmButton::Ok,
            ConfirmButton::Ok => ConfirmButton::Cancel,
        };
    }
}

/// What a press or a confirm's answer asks of the view.
#[derive(Debug)]
pub(crate) enum Action {
    Nothing,
    Send(Vec<ClientMessage>),
    Close,
    /// Close the manager and open the spawn dialog on this group.
    Launch(Box<RootWorktreeEntry>),
}

/// A row as it reads: its title, the lines under it, and its members while
/// they are expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) title: String,
    pub(crate) lines: Vec<String>,
    pub(crate) members: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct Snapshot {
    root: String,
    is_override: bool,
    entries: Vec<RootWorktreeEntry>,
}

/// The open manager.
#[derive(Debug)]
pub(crate) struct WorktreesManager {
    snapshot: Option<Snapshot>,
    /// How many requests still owe an answer; pending while above zero.
    pending: usize,
    expanded: BTreeSet<String>,
    focused: Control,
    confirm: Option<Confirm>,
}

impl WorktreesManager {
    /// The manager scanning the root, and the request that scans it.
    pub(crate) fn open() -> (Self, ClientMessage) {
        let manager = Self {
            snapshot: None,
            pending: 1,
            expanded: BTreeSet::new(),
            focused: Control::Refresh,
            confirm: None,
        };
        (manager, ClientMessage::InspectWorktreesRoot)
    }

    pub(crate) fn pending(&self) -> bool {
        self.pending > 0
    }

    /// Whether the first snapshot is still on its way. An error that
    /// answers nothing leaves the manager scanning: only a snapshot ends
    /// the scan.
    pub(crate) fn scanning(&self) -> bool {
        self.snapshot.is_none()
    }

    pub(crate) fn entries(&self) -> &[RootWorktreeEntry] {
        self.snapshot.as_ref().map_or(&[], |s| s.entries.as_slice())
    }

    /// Whether a snapshot came and listed nothing.
    pub(crate) fn empty(&self) -> bool {
        self.snapshot.as_ref().is_some_and(|s| s.entries.is_empty())
    }

    /// The root line: the snapshot's root, else `known` (the root the
    /// connection last heard), and whether the user set it.
    pub(crate) fn root_line(&self, known: Option<(&str, bool)>) -> String {
        let root = self
            .snapshot
            .as_ref()
            .map(|s| (s.root.as_str(), s.is_override))
            .or(known);
        match root {
            Some((root, true)) => format!("Root: {root} — user override"),
            Some((root, false)) => format!("Root: {root} — default"),
            None => format!("Root: {NOT_CONNECTED} — default"),
        }
    }

    /// A snapshot arrived: it replaces the rows and answers one request.
    pub(crate) fn on_snapshot(
        &mut self,
        root: &str,
        is_override: bool,
        entries: &[RootWorktreeEntry],
    ) {
        self.expanded
            .retain(|path| entries.iter().any(|entry| &entry.path == path));
        self.snapshot = Some(Snapshot {
            root: root.to_owned(),
            is_override,
            entries: entries.to_vec(),
        });
        self.pending = self.pending.saturating_sub(1);
    }

    /// A daemon error answers one of the manager's own requests, if one is
    /// outstanding; an error answering none (another client's, say) leaves
    /// the manager as it was.
    pub(crate) fn on_error(&mut self) {
        if self.pending > 0 {
            self.pending -= 1;
        }
    }

    /// One more request owes an answer.
    pub(crate) fn request_scan(&mut self) {
        self.pending += 1;
    }

    fn entry(&self, path: &str) -> Option<&RootWorktreeEntry> {
        self.entries().iter().find(|entry| entry.path == path)
    }

    fn stale(&self) -> impl Iterator<Item = &RootWorktreeEntry> {
        self.entries()
            .iter()
            .filter(|entry| entry.status == RootWorktreeStatus::Stale)
    }

    /// Every control drawn, enabled or not, in Tab order.
    pub(crate) fn all_controls(&self) -> Vec<Control> {
        let mut controls = vec![Control::Refresh, Control::DeleteStale];
        for entry in self.entries() {
            if !entry.members.is_empty() {
                controls.push(Control::Members(entry.path.clone()));
            }
            controls.push(Control::Launch(entry.path.clone()));
            controls.push(Control::Delete(entry.path.clone()));
        }
        controls.push(Control::Close);
        controls
    }

    /// The controls Tab reaches: the enabled ones.
    pub(crate) fn controls(&self) -> Vec<Control> {
        self.all_controls()
            .into_iter()
            .filter(|control| self.enabled(control))
            .collect()
    }

    pub(crate) fn enabled(&self, control: &Control) -> bool {
        let idle = !self.pending() && !self.scanning();
        match control {
            Control::Refresh => idle,
            Control::DeleteStale => idle && self.stale().next().is_some(),
            Control::Members(path) => self.entry(path).is_some_and(|e| !e.members.is_empty()),
            Control::Launch(path) => idle && self.entry(path).is_some_and(launchable),
            Control::Delete(path) => self
                .entry(path)
                .is_some_and(|e| idle && e.status != RootWorktreeStatus::Active),
            Control::Close => true,
        }
    }

    pub(crate) fn label(&self, control: &Control) -> String {
        match control {
            Control::Refresh => "Refresh".to_owned(),
            Control::DeleteStale => format!("Delete all stale ({})", self.stale().count()),
            Control::Members(path) => {
                let count = self.entry(path).map_or(0, |e| e.members.len());
                let noun = if count == 1 { "member" } else { "members" };
                let mark = if self.expanded.contains(path) {
                    "▾"
                } else {
                    "▸"
                };
                format!("{count} {noun} {mark}")
            }
            Control::Launch(_) => "Launch session here".to_owned(),
            Control::Delete(_) => "Delete".to_owned(),
            Control::Close => "Close".to_owned(),
        }
    }

    /// The tooltip a row's Launch or Delete carries.
    pub(crate) fn tooltip(&self, control: &Control) -> Option<String> {
        match control {
            Control::Launch(path) => self.entry(path).map(launch_tooltip),
            Control::Delete(path) => self.entry(path).map(|entry| {
                if entry.status == RootWorktreeStatus::Active {
                    "Stop the active session before deleting".to_owned()
                } else {
                    "Delete this worktree group".to_owned()
                }
            }),
            _ => None,
        }
    }

    pub(crate) fn selector(&self, control: &Control) -> String {
        let row = |path: &str| {
            self.entries()
                .iter()
                .position(|entry| entry.path == path)
                .unwrap_or(usize::MAX)
        };
        match control {
            Control::Refresh => "worktrees-manager-refresh".to_owned(),
            Control::DeleteStale => "worktrees-manager-bulk-delete".to_owned(),
            Control::Members(path) => format!("worktrees-manager-row-members-{}", row(path)),
            Control::Launch(path) => format!("worktrees-manager-row-launch-{}", row(path)),
            Control::Delete(path) => format!("worktrees-manager-row-delete-{}", row(path)),
            Control::Close => "worktrees-manager-close".to_owned(),
        }
    }

    /// The focused control; one gone or disabled gives way to the first.
    pub(crate) fn focused(&self) -> Control {
        let controls = self.controls();
        if controls.contains(&self.focused) {
            self.focused.clone()
        } else {
            controls.into_iter().next().unwrap_or(Control::Close)
        }
    }

    /// Tab (`forward`) or Shift+Tab: the next or previous control, wrapping.
    pub(crate) fn move_focus(&mut self, forward: bool) {
        let controls = self.controls();
        let focused = self.focused();
        let count = controls.len();
        let at = controls.iter().position(|c| *c == focused).unwrap_or(0);
        let next = if forward {
            (at + 1) % count
        } else {
            (at + count - 1) % count
        };
        self.focused = controls[next].clone();
    }

    /// A click on `control`, or Space or Enter while it has the focus. A
    /// disabled control does nothing.
    pub(crate) fn press(&mut self, control: &Control) -> Action {
        if !self.enabled(control) {
            return Action::Nothing;
        }
        self.focused = control.clone();
        match control {
            Control::Refresh => {
                self.request_scan();
                Action::Send(vec![ClientMessage::InspectWorktreesRoot])
            }
            Control::DeleteStale => {
                let stale: Vec<&RootWorktreeEntry> = self.stale().collect();
                let kind = ConfirmKind::Delete {
                    paths: stale.iter().map(|e| e.path.clone()).collect(),
                    bytes: stale.iter().filter_map(|e| e.size_bytes).sum(),
                    bulk: true,
                };
                self.confirm = Some(Confirm::new(kind));
                Action::Nothing
            }
            Control::Members(path) => {
                if !self.expanded.remove(path) {
                    self.expanded.insert(path.clone());
                }
                Action::Nothing
            }
            Control::Launch(path) => self.ask_launch(path),
            Control::Delete(path) => {
                let bytes = self.entry(path).and_then(|e| e.size_bytes).unwrap_or(0);
                self.confirm = Some(Confirm::new(ConfirmKind::Delete {
                    paths: vec![path.clone()],
                    bytes,
                    bulk: false,
                }));
                Action::Nothing
            }
            Control::Close => Action::Close,
        }
    }

    /// Launches into the group at `path`, asking first when a session is
    /// running in it.
    fn ask_launch(&mut self, path: &str) -> Action {
        let Some(entry) = self.entry(path) else {
            return Action::Nothing;
        };
        if entry.status == RootWorktreeStatus::Active {
            let kind = ConfirmKind::Share {
                path: path.to_owned(),
                slug: entry.branch_slug.clone(),
            };
            self.confirm = Some(Confirm::new(kind));
            return Action::Nothing;
        }
        Action::Launch(Box::new(entry.clone()))
    }

    pub(crate) fn confirm(&self) -> Option<&Confirm> {
        self.confirm.as_ref()
    }

    pub(crate) fn confirm_mut(&mut self) -> Option<&mut Confirm> {
        self.confirm.as_mut()
    }

    /// The open confirm's answer: Cancel closes it; OK sends its deletes,
    /// each owing an answer, or launches.
    pub(crate) fn answer(&mut self, button: ConfirmButton) -> Action {
        let Some(confirm) = self.confirm.take() else {
            return Action::Nothing;
        };
        if button == ConfirmButton::Cancel {
            return Action::Nothing;
        }
        match confirm.kind {
            ConfirmKind::Delete { paths, .. } => {
                self.pending += paths.len();
                Action::Send(
                    paths
                        .into_iter()
                        .map(|path| ClientMessage::DeleteWorktreeAt { path })
                        .collect(),
                )
            }
            ConfirmKind::Share { path, .. } => self
                .entry(&path)
                .map_or(Action::Nothing, |e| Action::Launch(Box::new(e.clone()))),
        }
    }

    /// The row of `entry` as it reads at `now_unix`.
    pub(crate) fn row(&self, entry: &RootWorktreeEntry, now_unix: i64) -> Row {
        let title = row_title(entry);
        let mut lines = vec![status_label(entry.status).to_owned()];
        if title != entry.path {
            lines.push(entry.path.clone());
        }
        lines.push(format!("Branch: {}", entry.branch_slug));
        let size = entry.size_bytes.map_or_else(|| "—".to_owned(), human_size);
        lines.push(format!("Size: {size}"));
        let modified = entry
            .last_modified_unix
            .map_or_else(|| "—".to_owned(), |t| human_relative_time(now_unix, t));
        lines.push(format!("Modified: {modified}"));
        if let Some(session) = &entry.session_id {
            lines.push(format!("Session: {session}"));
        }
        let members = self
            .expanded
            .contains(&entry.path)
            .then(|| entry.members.iter().map(member_line).collect());
        Row {
            title,
            lines,
            members,
        }
    }
}

/// A row's title: the group's name for a marked group folder, else the
/// folder's path (an older layout, whose entry is the `wt.` folder itself).
pub(crate) fn row_title(entry: &RootWorktreeEntry) -> String {
    let folder = entry
        .path
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    if entry.anchor.is_empty() || folder.starts_with(WT_PREFIX) {
        entry.path.clone()
    } else {
        entry.anchor.clone()
    }
}

pub(crate) fn status_label(status: RootWorktreeStatus) -> &'static str {
    match status {
        RootWorktreeStatus::Active => "Active",
        RootWorktreeStatus::Detached => "Detached",
        RootWorktreeStatus::Stale => "Stale",
        RootWorktreeStatus::Unknown => "Unknown",
    }
}

fn member_line(member: &RootWorktreeMember) -> String {
    match &member.repo_path {
        Some(repo) => format!("{} ← {repo}", member.repo_name_hint),
        None => format!(
            "{} (repo unreachable — will fall back to fs delete)",
            member.repo_name_hint
        ),
    }
}

fn launchable(entry: &RootWorktreeEntry) -> bool {
    entry
        .launch
        .as_ref()
        .is_some_and(|launch| !matches!(launch, protocol::WorktreeLaunchTarget::Unknown))
}

fn launch_tooltip(entry: &RootWorktreeEntry) -> String {
    if launchable(entry) {
        if entry.status == RootWorktreeStatus::Active {
            "Launch a second session in this worktree".to_owned()
        } else {
            "Launch a session in this worktree".to_owned()
        }
    } else {
        entry
            .launch_blocked_reason
            .clone()
            .unwrap_or_else(|| UNKNOWN_LAUNCH.to_owned())
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(path: &str, status: &str) -> RootWorktreeEntry {
        serde_json::from_value(json!({
            "path": path, "anchor": "", "branch_slug": "feat-x",
            "members": [], "status": { "kind": status }, "session_id": null,
            "size_bytes": null, "last_modified_unix": null,
            "launch": { "kind": "single", "repo_id": "r1", "branch": "feat/x",
                "worktree_path": format!("{path}/r1") },
        }))
        .expect("root entry fixture")
    }

    /// An action as comparable text: what it sends as wire JSON, the path
    /// it launches into.
    fn show(action: Action) -> String {
        match action {
            Action::Nothing => "nothing".to_owned(),
            Action::Close => "close".to_owned(),
            Action::Send(sent) => serde_json::to_string(&sent).expect("messages encode"),
            Action::Launch(entry) => format!("launch {}", entry.path),
        }
    }

    fn sends(msgs: &[ClientMessage]) -> String {
        show(Action::Send(msgs.to_vec()))
    }

    fn loaded(entries: &[RootWorktreeEntry]) -> WorktreesManager {
        let (mut manager, _) = WorktreesManager::open();
        manager.on_snapshot("C:/wt", false, entries);
        manager
    }

    #[test]
    fn open_scans_and_disables_refresh_until_the_snapshot() {
        let (mut manager, msg) = WorktreesManager::open();
        assert!(matches!(msg, ClientMessage::InspectWorktreesRoot));
        assert!(manager.scanning());
        assert_eq!(
            manager.controls(),
            [Control::Close],
            "only Close while scanning"
        );
        assert_eq!(show(manager.press(&Control::Refresh)), "nothing");
        manager.on_snapshot("C:/wt", true, &[]);
        assert!(!manager.scanning());
        assert!(manager.empty());
        assert_eq!(
            manager.focused(),
            Control::Refresh,
            "Refresh takes the focus once enabled"
        );
        assert_eq!(manager.root_line(None), "Root: C:/wt — user override");
        assert_eq!(
            show(manager.press(&Control::Refresh)),
            sends(&[ClientMessage::InspectWorktreesRoot])
        );
        assert!(manager.pending());
    }

    #[test]
    fn unrelated_error_during_first_scan_keeps_scanning() {
        let (mut manager, _) = WorktreesManager::open();
        manager.on_error();
        assert!(
            manager.scanning(),
            "an error answering nothing leaves the scan on its way"
        );
        assert!(
            !manager.enabled(&Control::Refresh),
            "the scan still owes a snapshot"
        );
        assert_eq!(
            manager.controls(),
            [Control::Close],
            "only Close while scanning"
        );
        manager.on_snapshot("C:/wt", false, &[]);
        assert!(!manager.scanning());
        assert!(manager.empty());
        assert!(
            manager.enabled(&Control::Refresh),
            "the snapshot answered it"
        );
    }

    #[test]
    fn root_line_falls_back_to_the_known_root() {
        let (manager, _) = WorktreesManager::open();
        assert_eq!(
            manager.root_line(Some(("D:/w", false))),
            "Root: D:/w — default"
        );
        assert_eq!(
            manager.root_line(None),
            "Root: (daemon not connected) — default"
        );
    }

    #[test]
    fn focus_order_walks_rows_and_skips_disabled() {
        let mut with_members = entry("C:/wt/wt.a/g", "stale");
        with_members.members = vec![RootWorktreeMember {
            worktree_path: "C:/wt/wt.a/g/r1".to_owned(),
            repo_path: Some("C:/r1".to_owned()),
            repo_name_hint: "r1".to_owned(),
        }];
        let active = entry("C:/wt/wt.b/g", "active");
        let manager = loaded(&[with_members, active]);
        let a = "C:/wt/wt.a/g".to_owned();
        let b = "C:/wt/wt.b/g".to_owned();
        assert_eq!(
            manager.controls(),
            [
                Control::Refresh,
                Control::DeleteStale,
                Control::Members(a.clone()),
                Control::Launch(a.clone()),
                Control::Delete(a.clone()),
                Control::Launch(b.clone()),
                Control::Close,
            ],
            "the active row's Delete is out of the ring"
        );
        assert_eq!(
            manager.selector(&Control::Delete(b)),
            "worktrees-manager-row-delete-1"
        );
        assert_eq!(
            manager.selector(&Control::Members(a)),
            "worktrees-manager-row-members-0"
        );
    }

    #[test]
    fn move_focus_wraps() {
        let mut manager = loaded(&[]);
        assert_eq!(manager.controls(), [Control::Refresh, Control::Close]);
        manager.move_focus(true);
        assert_eq!(manager.focused(), Control::Close);
        manager.move_focus(true);
        assert_eq!(manager.focused(), Control::Refresh);
        manager.move_focus(false);
        assert_eq!(manager.focused(), Control::Close);
    }

    #[test]
    fn launch_and_delete_rules_follow_status() {
        let mut blocked = entry("C:/wt/wt.c", "stale");
        blocked.launch = None;
        blocked.launch_blocked_reason = Some("repo not registered".to_owned());
        let mut unknown = entry("C:/wt/wt.d", "detached");
        unknown.launch = Some(protocol::WorktreeLaunchTarget::Unknown);
        let active = entry("C:/wt/wt.a", "active");
        let stale = entry("C:/wt/wt.b", "stale");
        let manager = loaded(&[active, stale, blocked, unknown]);
        let launch = |p: &str| Control::Launch(p.to_owned());
        let delete = |p: &str| Control::Delete(p.to_owned());
        assert!(manager.enabled(&launch("C:/wt/wt.a")));
        assert_eq!(
            manager.tooltip(&launch("C:/wt/wt.a")).as_deref(),
            Some("Launch a second session in this worktree")
        );
        assert_eq!(
            manager.tooltip(&launch("C:/wt/wt.b")).as_deref(),
            Some("Launch a session in this worktree")
        );
        assert!(!manager.enabled(&launch("C:/wt/wt.c")));
        assert_eq!(
            manager.tooltip(&launch("C:/wt/wt.c")).as_deref(),
            Some("repo not registered")
        );
        assert!(!manager.enabled(&launch("C:/wt/wt.d")));
        assert_eq!(
            manager.tooltip(&launch("C:/wt/wt.d")).as_deref(),
            Some("this build doesn't know how to launch this group")
        );
        assert!(!manager.enabled(&delete("C:/wt/wt.a")));
        assert_eq!(
            manager.tooltip(&delete("C:/wt/wt.a")).as_deref(),
            Some("Stop the active session before deleting")
        );
        assert!(manager.enabled(&delete("C:/wt/wt.d")));
        assert_eq!(
            manager.tooltip(&delete("C:/wt/wt.b")).as_deref(),
            Some("Delete this worktree group")
        );
        assert_eq!(manager.label(&Control::DeleteStale), "Delete all stale (2)");
    }

    #[test]
    fn row_is_titled_by_group_name_else_path() {
        let mut marked = entry("C:/wt/wt.feat-x/app", "detached");
        marked.anchor = "App".to_owned();
        marked.size_bytes = Some(2048);
        marked.last_modified_unix = Some(1000);
        marked.session_id = Some("s1".to_owned());
        let mut old = entry("C:\\wt\\wt.feat-y", "stale");
        old.anchor = "X/dev".to_owned();
        let manager = loaded(&[marked.clone(), old.clone()]);
        let row = manager.row(&marked, 1000 + 7200);
        assert_eq!(row.title, "App");
        assert_eq!(
            row.lines,
            [
                "Detached",
                "C:/wt/wt.feat-x/app",
                "Branch: feat-x",
                "Size: 2.0 KB",
                "Modified: 2h ago",
                "Session: s1"
            ]
        );
        let row = manager.row(&old, 0);
        assert_eq!(
            row.title, "C:\\wt\\wt.feat-y",
            "an unmarked folder shows its path"
        );
        assert_eq!(
            row.lines,
            ["Stale", "Branch: feat-x", "Size: —", "Modified: —"]
        );
        assert_eq!(row.members, None);
    }

    #[test]
    fn members_toggle_expands_each_member() {
        let mut group = entry("C:/wt/wt.a/g", "stale");
        group.members = vec![
            RootWorktreeMember {
                worktree_path: "C:/wt/wt.a/g/r1".to_owned(),
                repo_path: Some("C:/src/r1".to_owned()),
                repo_name_hint: "r1".to_owned(),
            },
            RootWorktreeMember {
                worktree_path: "C:/wt/wt.a/g/r2".to_owned(),
                repo_path: None,
                repo_name_hint: "r2".to_owned(),
            },
        ];
        let mut manager = loaded(std::slice::from_ref(&group));
        let toggle = Control::Members(group.path.clone());
        assert_eq!(manager.label(&toggle), "2 members ▸");
        manager.press(&toggle);
        assert_eq!(manager.label(&toggle), "2 members ▾");
        assert_eq!(
            manager.row(&group, 0).members.expect("expanded"),
            [
                "r1 ← C:/src/r1",
                "r2 (repo unreachable — will fall back to fs delete)"
            ]
        );
        manager.press(&toggle);
        assert_eq!(manager.row(&group, 0).members, None);
    }

    #[test]
    fn delete_asks_then_sends_and_waits_for_the_answer() {
        let mut one = entry("C:/wt/wt.a", "stale");
        one.size_bytes = Some(3 * 1024 * 1024);
        let mut manager = loaded(&[one]);
        assert_eq!(
            show(manager.press(&Control::Delete("C:/wt/wt.a".to_owned()))),
            "nothing"
        );
        let confirm = manager.confirm().expect("the confirm opened");
        assert_eq!(confirm.title(), "Confirm delete");
        assert_eq!(
            confirm.body(),
            "Delete this worktree group? Frees ~3.0 MB on disk."
        );
        assert!(
            confirm
                .hint()
                .starts_with("The daemon will run git worktree remove --force")
        );
        assert_eq!(confirm.focused(), ConfirmButton::Cancel, "Cancel first");
        assert_eq!(show(manager.answer(ConfirmButton::Cancel)), "nothing");
        assert!(manager.confirm().is_none());
        manager.press(&Control::Delete("C:/wt/wt.a".to_owned()));
        assert_eq!(
            show(manager.answer(ConfirmButton::Ok)),
            sends(&[ClientMessage::DeleteWorktreeAt {
                path: "C:/wt/wt.a".to_owned()
            }])
        );
        assert!(manager.pending());
        manager.on_error();
        assert!(!manager.pending(), "an error answers the delete");
    }

    #[test]
    fn bulk_delete_clears_pending_after_every_answer() {
        let mut manager = loaded(&[
            entry("C:/wt/wt.a", "stale"),
            entry("C:/wt/wt.b", "detached"),
            entry("C:/wt/wt.c", "stale"),
        ]);
        manager.press(&Control::DeleteStale);
        let confirm = manager.confirm().expect("the confirm opened");
        assert_eq!(
            confirm.body(),
            "Delete 2 stale worktree groups?",
            "no size, no Frees"
        );
        assert_eq!(
            show(manager.answer(ConfirmButton::Ok)),
            sends(&[
                ClientMessage::DeleteWorktreeAt {
                    path: "C:/wt/wt.a".to_owned()
                },
                ClientMessage::DeleteWorktreeAt {
                    path: "C:/wt/wt.c".to_owned()
                },
            ])
        );
        manager.on_snapshot("C:/wt", false, &[entry("C:/wt/wt.c", "stale")]);
        assert!(manager.pending(), "one delete still owes an answer");
        assert!(!manager.enabled(&Control::Refresh));
        manager.on_error();
        assert!(!manager.pending());
        assert!(manager.enabled(&Control::Refresh));
    }

    #[test]
    fn launch_into_active_asks_share_first() {
        let active = entry("C:/wt/wt.a", "active");
        let stale = entry("C:/wt/wt.b", "stale");
        let mut manager = loaded(&[active, stale]);
        assert_eq!(
            show(manager.press(&Control::Launch("C:/wt/wt.b".to_owned()))),
            "launch C:/wt/wt.b"
        );
        assert_eq!(
            show(manager.press(&Control::Launch("C:/wt/wt.a".to_owned()))),
            "nothing"
        );
        let confirm = manager.confirm_mut().expect("the share confirm opened");
        assert_eq!(confirm.title(), "Share this worktree?");
        assert_eq!(
            confirm.body(),
            "A session is already running in feat-x. Launching here puts a second agent in the same working tree."
        );
        assert!(
            confirm
                .hint()
                .starts_with("They will see each other's uncommitted edits")
        );
        assert_eq!(confirm.label(ConfirmButton::Ok), "Launch anyway");
        confirm.toggle_focus();
        assert_eq!(confirm.focused(), ConfirmButton::Ok);
        assert_eq!(show(manager.answer(ConfirmButton::Ok)), "launch C:/wt/wt.a");
    }

    #[test]
    fn one_stale_group_reads_singular() {
        let mut manager = loaded(&[entry("C:/wt/wt.a", "stale")]);
        manager.press(&Control::DeleteStale);
        assert_eq!(
            manager.confirm().expect("opened").body(),
            "Delete 1 stale worktree group?"
        );
    }
}
