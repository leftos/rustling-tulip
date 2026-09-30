//! What the user can do to a session from its context menu, its pane header
//! and the stopped-pane overlay, and the daemon messages each choice sends.
//! Mirrors the Tauri app's `SessionContextMenu.tsx` and `SessionPane.tsx`.

use std::collections::HashMap;

use protocol::{ClientMessage, SessionSnapshot, SessionStatus, TabEntry};

use crate::headless;
use crate::sidebar::{LeafState, can_attach, runtime_label};
use crate::tabs::{
    PaneBinding, PaneTarget, Placement, TabsModel, collect_panes, find_tab_containing_session,
};

/// Which set of actions a session offers. Parked, stopped and running
/// follow `SessionContextMenu.tsx`'s state buckets. An abandoned session
/// gets the Resume and Dismiss of `SessionPane.tsx`'s abandoned view
/// instead of Stop or Restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActionState {
    Running,
    Stopped,
    Inactive,
    Abandoned,
}

/// Something the user can do to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SessionAction {
    Rename,
    StopKeepWorktree,
    StopDeleteWorktree,
    Restart,
    /// "Remove pane, keep worktree": the session moves to inactive.
    Park,
    RemovePane,
    RemovePaneDeleteWorktree,
    Resume,
    RemoveFromSidebar,
    RemoveFromSidebarDeleteWorktree,
    ResumeAbandoned,
    DismissAbandoned,
}

/// What the context menu shows besides the name editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuMode {
    /// The session's actions, the stop choices folded under one entry.
    Actions,
    /// "Stop session…" was chosen: the stop choices and Cancel.
    StopChoice,
}

/// A row of the context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuEntry {
    Action(SessionAction),
    /// "Stop session…", which opens the stop choices.
    StopChoice,
    /// Back from the stop choices to the actions.
    Cancel,
}

/// What choosing an action does.
#[derive(Debug, Clone)]
pub(crate) enum Step {
    /// Send these, in order.
    Send(Vec<ClientMessage>),
    /// Open the name editor; nothing is sent until it is submitted.
    EditName,
    /// Duplicate the session under a fresh request id, then place the
    /// duplicate and discard the original ([`Duplicates`]).
    Duplicate,
    /// Open the delete-worktree confirm, whose answer sends the messages
    /// ([`crate::branch_fate`]).
    ConfirmWorktreeDelete,
}

/// The pane header's Stop waiting for its confirming second click, keyed by
/// the session it stops, so it never carries over to another session.
#[derive(Debug, Default)]
pub(crate) struct HeaderStopConfirm {
    armed: Option<String>,
}

/// Which submenu replaces the context menu's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Submenu {
    /// None: the menu shows its rows.
    #[default]
    None,
    Accent,
    Duplicate,
    MoveTo,
}

/// A row of the menu's group between the state actions and the appearance
/// rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionRow {
    /// "Duplicate ▸", which opens the Duplicate submenu.
    Duplicate,
    /// "Move to ▸", which opens the Move to submenu; offered while a pane
    /// shows the session.
    MoveTo,
    /// Places the session where a leaf click would; offered while no pane
    /// shows it. `new_tab` when that placement opens a new tab (no grid tab
    /// is active), which the label then says.
    AddToCurrentTab { new_tab: bool },
    /// Opens the session in a new tab; offered while no pane shows it.
    AddToNewTab,
    /// Opens the folder of the session's own worktree.
    RevealWorktree,
}

/// Where Move to ▸ moves a pane showing the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MoveTarget {
    /// A new tab of its own.
    NewTab,
    /// Grid tab `id`, at its balanced drop target.
    Tab(String),
}

/// A [`SessionRow`] as one session offers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OfferedRow {
    pub(crate) row: SessionRow,
    /// Why the row is dimmed and does nothing; `None` when it can be chosen.
    pub(crate) disabled: Option<&'static str>,
}

/// Why a headless session's Duplicate ▸ is dimmed.
pub(crate) const HEADLESS_DUPLICATE_TIP: &str =
    "Headless sessions are one-shot kickoffs; spawn a new one instead";

/// A line of a submenu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubmenuLine<T> {
    /// "‹ Back" to the menu's rows, tagged `selector`.
    Back {
        selector: &'static str,
    },
    Separator,
    Choice {
        selector: String,
        label: String,
        choice: T,
    },
}

/// Where a duplicate goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DuplicateTarget {
    /// A restart or resume: the copy takes every pane that showed the
    /// original, or a new tab when none did, and the original is discarded.
    Restart,
    /// A new tab of its own; the original stays.
    NewTab,
    /// Grid tab `id`, by [`crate::tabs::pane_target_for_session`]; the
    /// original stays.
    Tab(String),
}

/// A duplicate on its way: the session it copies and where it goes.
#[derive(Debug)]
struct PendingDuplicate {
    original: String,
    target: DuplicateTarget,
}

/// The sessions being duplicated, by the request id of their
/// `DuplicateSession`.
#[derive(Debug, Default)]
pub(crate) struct Duplicates {
    pending: HashMap<String, PendingDuplicate>,
}

/// Where the view turns once a duplicate's placement is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlacedFocus {
    /// A new tab is on its way; its arrival shows it.
    NewTab,
    /// Show `tab_id` with `pane_id` focused.
    Pane { tab_id: String, pane_id: String },
    /// Show `tab_id`; the pane the placement adds takes the focus when it
    /// arrives.
    Tab(String),
}

/// How a duplicate that arrived is placed.
#[derive(Debug, Clone)]
pub(crate) struct Placed {
    /// Where the duplicate goes, then, for a restart, the original's
    /// discard.
    pub(crate) messages: Vec<ClientMessage>,
    pub(crate) focus: PlacedFocus,
}

pub(crate) fn action_state(session: &SessionSnapshot) -> ActionState {
    if session.is_inactive {
        ActionState::Inactive
    } else if session.is_abandoned {
        ActionState::Abandoned
    } else if matches!(
        session.status,
        SessionStatus::Stopped | SessionStatus::Error
    ) {
        ActionState::Stopped
    } else {
        ActionState::Running
    }
}

/// Whether the pane header gives the exit code instead of Stop: any
/// stopped session, as in `SessionPane.tsx`'s header.
pub(crate) fn header_shows_exit_code(session: &SessionSnapshot) -> bool {
    session.status == SessionStatus::Stopped
}

/// Whether the stopped-pane overlay covers the terminal: a stopped session
/// that was not abandoned, and that had a terminal to cover — a headless
/// pane keeps its stats and log, and the session's menu still offers the
/// actions.
pub(crate) fn pane_shows_exit(session: &SessionSnapshot) -> bool {
    header_shows_exit_code(session) && !session.is_abandoned && !headless::is_headless(session)
}

/// A button a sidebar leaf or the abandoned-pane overlay draws beside the
/// session's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InlineAction {
    pub action: SessionAction,
    /// The button's part of its debug selector.
    pub key: &'static str,
    pub tip: &'static str,
}

const RESUME_ABANDONED: InlineAction = InlineAction {
    action: SessionAction::ResumeAbandoned,
    key: "resume",
    tip: "Spawn a fresh session from the captured config and replay the prompt",
};
const DISMISS_ABANDONED: InlineAction = InlineAction {
    action: SessionAction::DismissAbandoned,
    key: "dismiss",
    tip: "Dismiss this abandoned session without resuming",
};
const RESUME_PARKED: InlineAction = InlineAction {
    action: SessionAction::Resume,
    key: "resume",
    tip: "Spawn a fresh session that reuses this worktree",
};

/// The abandoned session's Resume and Dismiss.
pub(crate) const ABANDONED_ACTIONS: [InlineAction; 2] = [RESUME_ABANDONED, DISMISS_ABANDONED];

/// The buttons a leaf offers by the session's state, as the menu ranks it:
/// Resume for a parked session, Resume and Dismiss for an abandoned one.
pub(crate) fn inline_actions(state: &LeafState) -> Vec<InlineAction> {
    match state {
        LeafState::Inactive { .. } => vec![RESUME_PARKED],
        LeafState::Abandoned { .. } => ABANDONED_ACTIONS.to_vec(),
        LeafState::Live | LeafState::Orphan => Vec::new(),
    }
}

/// The abandoned-pane overlay's text: what happened, then the prompt it was
/// running when there was one.
pub(crate) fn abandoned_lines(session: &SessionSnapshot) -> Vec<String> {
    let mut lines = vec!["Session abandoned during daemon restart.".to_owned()];
    if let Some(prompt) = session
        .last_prompt
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        lines.push(format!("Last prompt: {prompt}"));
    }
    lines
}

/// The banner over an orphan's pane, naming what still runs under it.
pub(crate) fn orphan_banner_text(session: &SessionSnapshot) -> String {
    let runtime = runtime_label(session).unwrap_or_else(|| "shell".to_owned());
    format!(
        "PTY stream lost across daemon restart. The underlying {runtime} process is still \
         running, but live input/output is not available. Use Stop to kill the recorded PID and \
         clean up, then spawn a new session."
    )
}

/// Whether `session` just exited on its own: it stopped with its child's
/// exit code straight from a live status. A Stop reports stopped without a
/// code first, so the code that follows it compares against stopped.
/// Parked, abandoned and headless sessions are kept whatever their exit.
pub(crate) fn self_exited(prev: Option<SessionStatus>, session: &SessionSnapshot) -> bool {
    prev.is_some_and(|prev| !matches!(prev, SessionStatus::Stopped | SessionStatus::Error))
        && session.status == SessionStatus::Stopped
        && session.exit_code.is_some()
        && !session.is_inactive
        && !session.is_abandoned
        && !headless::is_headless(session)
}

fn exit_code(session: &SessionSnapshot) -> String {
    session
        .exit_code
        .map_or_else(|| "?".to_owned(), |code| code.to_string())
}

/// The pane header's note for an exited session.
pub(crate) fn exit_code_label(session: &SessionSnapshot) -> String {
    format!("exit code {}", exit_code(session))
}

/// The stopped-pane overlay's message.
pub(crate) fn exited_message(session: &SessionSnapshot) -> String {
    format!("Session exited (code {})", exit_code(session))
}

/// The actions a session offers, by its state; the worktree variants only
/// when it has a worktree of its own.
pub(crate) fn menu_actions(session: &SessionSnapshot) -> Vec<SessionAction> {
    let worktree = session.has_per_session_worktree;
    match action_state(session) {
        ActionState::Running => {
            let mut actions = vec![SessionAction::Rename, SessionAction::StopKeepWorktree];
            if worktree {
                actions.push(SessionAction::StopDeleteWorktree);
            }
            actions
        }
        ActionState::Stopped => overlay_actions(session),
        ActionState::Inactive => {
            let mut actions = vec![SessionAction::Resume, SessionAction::RemoveFromSidebar];
            if worktree {
                actions.push(SessionAction::RemoveFromSidebarDeleteWorktree);
            }
            actions
        }
        ActionState::Abandoned => vec![
            SessionAction::ResumeAbandoned,
            SessionAction::DismissAbandoned,
        ],
    }
}

/// The stopped-pane overlay's buttons: Restart, then keep the worktree
/// (when there is one), then remove the pane, deleting the worktree when
/// there is one.
pub(crate) fn overlay_actions(session: &SessionSnapshot) -> Vec<SessionAction> {
    if session.has_per_session_worktree {
        vec![
            SessionAction::Restart,
            SessionAction::Park,
            SessionAction::RemovePaneDeleteWorktree,
        ]
    } else {
        vec![SessionAction::Restart, SessionAction::RemovePane]
    }
}

/// The context menu's rows in `mode`. The stop choices list the worktree
/// delete first, as the Tauri confirm does; a session with no stop choices
/// left (it stopped meanwhile) shows its actions instead.
pub(crate) fn menu_entries(session: &SessionSnapshot, mode: MenuMode) -> Vec<MenuEntry> {
    let actions = menu_actions(session);
    let stops: Vec<MenuEntry> = actions
        .iter()
        .rev()
        .filter(|action| action.is_stop())
        .map(|&action| MenuEntry::Action(action))
        .collect();
    if mode == MenuMode::StopChoice && !stops.is_empty() {
        let mut entries = stops;
        entries.push(MenuEntry::Cancel);
        return entries;
    }
    let mut entries: Vec<MenuEntry> = actions
        .iter()
        .filter(|action| !action.is_stop())
        .map(|&action| MenuEntry::Action(action))
        .collect();
    if !stops.is_empty() {
        entries.push(MenuEntry::StopChoice);
    }
    entries
}

/// The rows of the group between the state actions and the appearance
/// rows: Duplicate ▸ in every state (dimmed for a headless session); Move
/// to ▸ while a pane shows the session, else Add to current tab and Add to
/// new tab when it can be shown in a pane (only the first, reading Add to
/// new tab, when placing it would open a tab anyway); Reveal worktree when
/// it has a worktree of its own.
pub(crate) fn session_rows(
    session: &SessionSnapshot,
    tabs: &TabsModel,
    sessions: &[SessionSnapshot],
) -> Vec<OfferedRow> {
    let offered = |row| OfferedRow {
        row,
        disabled: None,
    };
    let mut rows = vec![OfferedRow {
        row: SessionRow::Duplicate,
        disabled: headless::is_headless(session).then_some(HEADLESS_DUPLICATE_TIP),
    }];
    if find_tab_containing_session(tabs.tabs(), &session.id).is_some() {
        rows.push(offered(SessionRow::MoveTo));
    } else if can_attach(session) {
        let new_tab = matches!(tabs.place(session, sessions), Placement::NewTab);
        rows.push(offered(SessionRow::AddToCurrentTab { new_tab }));
        if !new_tab {
            rows.push(offered(SessionRow::AddToNewTab));
        }
    }
    if worktree_to_reveal(session).is_some() {
        rows.push(offered(SessionRow::RevealWorktree));
    }
    rows
}

/// The folder Reveal worktree opens: the first member's worktree, for a
/// session with a worktree of its own.
pub(crate) fn worktree_to_reveal(session: &SessionSnapshot) -> Option<&str> {
    if !session.has_per_session_worktree {
        return None;
    }
    session
        .members
        .first()
        .map(|member| member.worktree_path.as_str())
        .filter(|path| !path.is_empty())
}

/// Whether pane `pane_id` of tab `tab_id` shows `session_id`.
fn pane_shows(tabs: &[TabEntry], (tab_id, pane_id): &(String, String), session_id: &str) -> bool {
    tabs.iter()
        .find(|tab| tab.id == *tab_id)
        .and_then(TabEntry::grid)
        .is_some_and(|grid| {
            collect_panes(grid)
                .iter()
                .any(|pane| pane.id == pane_id.as_str() && pane.session == Some(session_id))
        })
}

/// A submenu of tabs: Back (tagged `back`), New tab, then `tabs` by name
/// behind a separator when there are any, tagged `<prefix>-new-tab` and
/// `<prefix>-tab-<id>`.
fn tab_lines<'a, T>(
    back: &'static str,
    prefix: &str,
    new_tab: T,
    tabs: impl Iterator<Item = &'a TabEntry>,
    to_tab: impl Fn(String) -> T,
) -> Vec<SubmenuLine<T>> {
    let mut lines = vec![
        SubmenuLine::Back { selector: back },
        SubmenuLine::Choice {
            selector: format!("{prefix}-new-tab"),
            label: "New tab".to_owned(),
            choice: new_tab,
        },
    ];
    let targets: Vec<SubmenuLine<T>> = tabs
        .map(|tab| SubmenuLine::Choice {
            selector: format!("{prefix}-tab-{}", tab.id),
            label: tab.name.clone(),
            choice: to_tab(tab.id.clone()),
        })
        .collect();
    if !targets.is_empty() {
        lines.push(SubmenuLine::Separator);
        lines.extend(targets);
    }
    lines
}

/// The Duplicate submenu: Back, New tab, then every grid tab by name
/// behind a separator.
pub(crate) fn duplicate_lines(tabs: &[TabEntry]) -> Vec<SubmenuLine<DuplicateTarget>> {
    tab_lines(
        "duplicate-back",
        "duplicate",
        DuplicateTarget::NewTab,
        tabs.iter().filter(|tab| tab.grid().is_some()),
        DuplicateTarget::Tab,
    )
}

/// The Move to submenu: Back, New tab, then every grid tab no pane of
/// which shows `session_id`, behind a separator.
pub(crate) fn move_lines(tabs: &[TabEntry], session_id: &str) -> Vec<SubmenuLine<MoveTarget>> {
    let targets = tabs.iter().filter(|tab| {
        tab.grid().is_some_and(|grid| {
            !collect_panes(grid)
                .iter()
                .any(|pane| pane.session == Some(session_id))
        })
    });
    tab_lines(
        "move-back",
        "move",
        MoveTarget::NewTab,
        targets,
        MoveTarget::Tab,
    )
}

/// The pane Move to ▸ moves: `clicked`, the `(tab, pane)` whose header
/// opened the menu, while it still shows `session_id`; else the first pane
/// that does.
pub(crate) fn move_source(
    tabs: &[TabEntry],
    session_id: &str,
    clicked: Option<&(String, String)>,
) -> Option<(String, String)> {
    clicked
        .filter(|clicked| pane_shows(tabs, clicked, session_id))
        .cloned()
        .or_else(|| find_tab_containing_session(tabs, session_id))
}

impl SessionRow {
    /// The row's debug selector.
    pub(crate) fn selector(self) -> &'static str {
        match self {
            Self::Duplicate => "session-menu-duplicate",
            Self::MoveTo => "session-menu-move",
            Self::AddToCurrentTab { .. } => "session-menu-add-current",
            Self::AddToNewTab => "session-menu-add-new",
            Self::RevealWorktree => "session-menu-reveal",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Duplicate => "Duplicate ▸",
            Self::MoveTo => "Move to ▸",
            Self::AddToCurrentTab { new_tab: false } => "Add to current tab",
            Self::AddToCurrentTab { new_tab: true } | Self::AddToNewTab => "Add to new tab",
            Self::RevealWorktree => "Reveal worktree",
        }
    }
}

/// The rename a submitted name sends; a blank name restores the default.
pub(crate) fn rename_message(session_id: &str, text: &str) -> ClientMessage {
    let label = text.trim();
    ClientMessage::RenameSession {
        session_id: session_id.to_owned(),
        label: (!label.is_empty()).then(|| label.to_owned()),
    }
}

/// What `action` does to `session`; `shown` says whether a pane shows it.
/// A stop that leaves no pane behind also parks the session when it has a
/// worktree to keep, else discards it, as `SessionContextMenu.tsx`'s
/// `sendStop` does. Deleting a worktree goes through the delete-worktree
/// confirm.
pub(crate) fn plan(action: SessionAction, session: &SessionSnapshot, shown: bool) -> Step {
    let id = session.id.clone();
    let stop = || ClientMessage::StopSession {
        session_id: id.clone(),
        cleanup: Vec::new(),
    };
    let discard = || ClientMessage::DiscardSession {
        session_id: id.clone(),
        cleanup: Vec::new(),
    };
    let park = || ClientMessage::ParkSession {
        session_id: id.clone(),
    };
    let messages = match action {
        SessionAction::Rename => return Step::EditName,
        SessionAction::Restart | SessionAction::Resume => return Step::Duplicate,
        SessionAction::StopDeleteWorktree
        | SessionAction::RemovePaneDeleteWorktree
        | SessionAction::RemoveFromSidebarDeleteWorktree => return Step::ConfirmWorktreeDelete,
        SessionAction::StopKeepWorktree if shown => vec![stop()],
        SessionAction::StopKeepWorktree if session.has_per_session_worktree => vec![stop(), park()],
        SessionAction::StopKeepWorktree => vec![stop(), discard()],
        SessionAction::Park => vec![park()],
        SessionAction::RemovePane | SessionAction::RemoveFromSidebar => vec![discard()],
        SessionAction::ResumeAbandoned => vec![ClientMessage::ResumeAbandoned {
            session_id: id.clone(),
        }],
        SessionAction::DismissAbandoned => vec![ClientMessage::DiscardAbandoned {
            session_id: id.clone(),
        }],
    };
    Step::Send(messages)
}

impl SessionAction {
    /// The action's part of its debug selector.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::StopKeepWorktree => "stop-keep",
            Self::StopDeleteWorktree => "stop-delete",
            Self::Restart => "restart",
            Self::Park => "park",
            Self::RemovePane => "remove-pane",
            Self::RemovePaneDeleteWorktree => "remove-pane-delete",
            Self::Resume => "resume",
            Self::RemoveFromSidebar => "remove",
            Self::RemoveFromSidebarDeleteWorktree => "remove-delete",
            Self::ResumeAbandoned => "resume-abandoned",
            Self::DismissAbandoned => "dismiss",
        }
    }

    /// The action's text. A stop that keeps nothing says so plainly, and a
    /// stopped session no pane shows is removed as a session, not a pane.
    pub(crate) fn label(self, has_worktree: bool, in_pane: bool) -> &'static str {
        match self {
            Self::Rename => "Rename…",
            Self::StopKeepWorktree if has_worktree => "Stop, keep worktree",
            Self::StopKeepWorktree => "Stop session",
            Self::StopDeleteWorktree => "Stop and delete worktree",
            Self::Restart => "Restart",
            Self::Park if in_pane => "Remove pane, keep worktree",
            Self::Park => "Remove session, keep worktree",
            Self::RemovePane if in_pane => "Remove pane",
            Self::RemovePane => "Remove session",
            Self::RemovePaneDeleteWorktree if in_pane => "Remove pane and delete worktree",
            Self::RemovePaneDeleteWorktree => "Remove session and delete worktree",
            Self::Resume | Self::ResumeAbandoned => "Resume",
            Self::RemoveFromSidebar => "Remove from sidebar",
            Self::RemoveFromSidebarDeleteWorktree => "Remove from sidebar and delete worktree",
            Self::DismissAbandoned => "Dismiss",
        }
    }

    /// The text of a restart or resume while its duplicate is on the way.
    pub(crate) fn pending_label(self) -> Option<&'static str> {
        match self {
            Self::Restart => Some("Restarting…"),
            Self::Resume => Some("Resuming…"),
            _ => None,
        }
    }

    /// Whether the action deletes a worktree, and so goes through the
    /// delete-worktree confirm.
    pub(crate) fn deletes_worktree(self) -> bool {
        matches!(
            self,
            Self::StopDeleteWorktree
                | Self::RemovePaneDeleteWorktree
                | Self::RemoveFromSidebarDeleteWorktree
        )
    }

    fn is_stop(self) -> bool {
        matches!(self, Self::StopKeepWorktree | Self::StopDeleteWorktree)
    }
}

impl HeaderStopConfirm {
    /// A Stop click for `session_id`: true when exactly that session was
    /// armed (and disarms it), else arms it and returns false.
    pub(crate) fn click(&mut self, session_id: &str) -> bool {
        if self.is_armed(session_id) {
            self.armed = None;
            true
        } else {
            self.armed = Some(session_id.to_owned());
            false
        }
    }

    pub(crate) fn is_armed(&self, session_id: &str) -> bool {
        self.armed.as_deref() == Some(session_id)
    }

    pub(crate) fn armed(&self) -> Option<&str> {
        self.armed.as_deref()
    }

    /// Returns whether anything was armed.
    pub(crate) fn disarm(&mut self) -> bool {
        self.armed.take().is_some()
    }
}

impl Duplicates {
    /// Records a duplicate of `session_id` to `target` under `request_id`
    /// and returns the request to send; `None` for a restart while one of
    /// the session is on the way.
    pub(crate) fn request(
        &mut self,
        session_id: &str,
        request_id: String,
        target: DuplicateTarget,
    ) -> Option<ClientMessage> {
        if target == DuplicateTarget::Restart && self.is_pending(session_id) {
            return None;
        }
        self.pending.insert(
            request_id.clone(),
            PendingDuplicate {
                original: session_id.to_owned(),
                target,
            },
        );
        Some(ClientMessage::DuplicateSession {
            session_id: session_id.to_owned(),
            request_id: Some(request_id),
        })
    }

    /// Whether a restart or resume of `session_id` waits for its reply.
    pub(crate) fn is_pending(&self, session_id: &str) -> bool {
        self.pending
            .values()
            .any(|dup| dup.target == DuplicateTarget::Restart && dup.original == session_id)
    }

    /// Whether `request_id` is one of this client's duplicates.
    pub(crate) fn has_request(&self, request_id: &str) -> bool {
        self.pending.contains_key(request_id)
    }

    /// A failure reply for `request_id`; returns whether it was pending.
    pub(crate) fn fail(&mut self, request_id: &str) -> bool {
        self.pending.remove(request_id).is_some()
    }

    /// Forgets every duplicate, as a new connection must.
    pub(crate) fn clear(&mut self) {
        self.pending.clear();
    }

    /// When `request_id` names a pending duplicate: the messages that place
    /// `copy` by its target, and where the view turns. A restart takes the
    /// original's place ([`restart_placement`]); a copy goes to a new tab,
    /// or into its tab by pane target, and keeps the original.
    pub(crate) fn place(
        &mut self,
        request_id: &str,
        copy: &SessionSnapshot,
        tabs: &TabsModel,
        sessions: &[SessionSnapshot],
    ) -> Option<Placed> {
        let pending = self.pending.remove(request_id)?;
        let placement = match pending.target {
            DuplicateTarget::Restart => {
                return Some(restart_placement(
                    pending.original,
                    &copy.id,
                    &tabs.bindings(),
                ));
            }
            DuplicateTarget::NewTab => Placement::NewTab,
            DuplicateTarget::Tab(tab_id) => tabs.place_in(&tab_id, copy, sessions),
        };
        let focus = match &placement {
            Placement::NewTab => PlacedFocus::NewTab,
            Placement::Pane {
                tab_id,
                target: PaneTarget::Replace { pane_id },
            } => PlacedFocus::Pane {
                tab_id: tab_id.clone(),
                pane_id: pane_id.clone(),
            },
            Placement::Pane { tab_id, .. } => PlacedFocus::Tab(tab_id.clone()),
        };
        Some(Placed {
            messages: vec![placement.message(&copy.id)],
            focus,
        })
    }
}

/// The messages that put `new_id` where `original` was (every pane that
/// showed it, focusing the first, or a new tab when none did) and then
/// discard the original. The discard goes last so the daemon has rebound
/// the panes before it closes the original's.
fn restart_placement(original: String, new_id: &str, bindings: &[PaneBinding]) -> Placed {
    let panes: Vec<&PaneBinding> = bindings
        .iter()
        .filter(|binding| binding.session_id.as_deref() == Some(original.as_str()))
        .collect();
    let mut messages: Vec<ClientMessage> = panes
        .iter()
        .map(|binding| ClientMessage::ReplacePaneSession {
            tab_id: binding.tab_id.clone(),
            pane_id: binding.pane_id.clone(),
            session_id: Some(new_id.to_owned()),
        })
        .collect();
    if panes.is_empty() {
        messages.push(ClientMessage::CreateTab {
            name: None,
            initial_session_id: Some(new_id.to_owned()),
        });
    }
    messages.push(ClientMessage::DiscardSession {
        session_id: original,
        cleanup: Vec::new(),
    });
    let focus = panes
        .first()
        .map_or(PlacedFocus::NewTab, |binding| PlacedFocus::Pane {
            tab_id: binding.tab_id.clone(),
            pane_id: binding.pane_id.clone(),
        });
    Placed { messages, focus }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect and panic; failure messages aid debugging"
)]
mod tests {
    use super::{
        ActionState, DuplicateTarget, Duplicates, HEADLESS_DUPLICATE_TIP, HeaderStopConfirm,
        MenuEntry, MenuMode, MoveTarget, OfferedRow, PlacedFocus, SessionAction, SessionRow, Step,
        SubmenuLine, action_state, exit_code_label, exited_message, header_shows_exit_code,
        menu_actions, menu_entries, move_lines, move_source, overlay_actions, pane_shows_exit,
        plan, rename_message, self_exited, session_rows, worktree_to_reveal,
    };
    use crate::tabs::TabsModel;
    use crate::tabs::tests::{model_with, pane, tab};
    use protocol::{
        CleanupAction, ClientMessage, GridNode, SessionMode, SessionSnapshot, SessionStatus,
        SplitDirection,
    };
    use serde_json::json;

    use SessionAction as A;

    fn session(status: &str) -> SessionSnapshot {
        serde_json::from_value(json!({
            "id": "s1",
            "label": "repo:main",
            "kind": "single",
            "members": [
                { "repo_id": "r1", "repo_name": "r1", "branch": "wt/a", "worktree_path": "" },
                { "repo_id": "r2", "repo_name": "r2", "branch": "wt/a", "worktree_path": "" },
            ],
            "status": status,
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": null },
            "recent_actions": [],
            "agent": "claude",
        }))
        .expect("session fixture")
    }

    fn with_worktree(mut s: SessionSnapshot) -> SessionSnapshot {
        s.has_per_session_worktree = true;
        s
    }

    fn inactive(mut s: SessionSnapshot) -> SessionSnapshot {
        s.is_inactive = true;
        s
    }

    fn abandoned() -> SessionSnapshot {
        let mut s = session("stopped");
        s.is_abandoned = true;
        s
    }

    fn exited_by_itself() -> SessionSnapshot {
        let mut s = session("stopped");
        s.exit_code = Some(0);
        s
    }

    #[test]
    fn exit_from_a_running_state_with_a_code_is_a_self_exit() {
        for prev in [
            SessionStatus::Idle,
            SessionStatus::Working,
            SessionStatus::AwaitingInput,
            SessionStatus::Spawning,
        ] {
            assert!(
                self_exited(Some(prev), &exited_by_itself()),
                "from {prev:?}"
            );
        }
    }

    #[test]
    fn exit_after_a_stop_or_an_error_is_not_a_self_exit() {
        let s = exited_by_itself();
        assert!(!self_exited(Some(SessionStatus::Stopped), &s));
        assert!(!self_exited(Some(SessionStatus::Error), &s));
        assert!(!self_exited(None, &s), "a session first seen stopped");
    }

    #[test]
    fn stop_without_an_exit_code_is_not_a_self_exit() {
        let mut s = exited_by_itself();
        s.exit_code = None;
        assert!(!self_exited(Some(SessionStatus::Working), &s));
        let mut s = exited_by_itself();
        s.status = SessionStatus::Error;
        assert!(!self_exited(Some(SessionStatus::Working), &s));
    }

    #[test]
    fn parked_abandoned_and_headless_exits_are_not_self_exits() {
        let prev = Some(SessionStatus::Working);
        assert!(!self_exited(prev, &inactive(exited_by_itself())));
        let mut s = exited_by_itself();
        s.is_abandoned = true;
        assert!(!self_exited(prev, &s));
        let mut s = exited_by_itself();
        s.mode = SessionMode::Headless;
        assert!(!self_exited(prev, &s));
    }

    fn sent(step: Step) -> Vec<ClientMessage> {
        match step {
            Step::Send(messages) => messages,
            other => panic!("expected messages, got {other:?}"),
        }
    }

    fn is_discard(msg: &ClientMessage, cleanup: &[CleanupAction]) -> bool {
        matches!(msg, ClientMessage::DiscardSession { session_id, cleanup: c } if session_id == "s1" && c == cleanup)
    }

    fn is_stop(msg: &ClientMessage) -> bool {
        matches!(msg, ClientMessage::StopSession { session_id, cleanup } if session_id == "s1" && cleanup.is_empty())
    }

    #[test]
    fn actions_state_follows_the_tauri_buckets() {
        for status in ["spawning", "idle", "working", "awaiting_input"] {
            assert_eq!(
                action_state(&session(status)),
                ActionState::Running,
                "{status}"
            );
        }
        assert_eq!(action_state(&session("stopped")), ActionState::Stopped);
        assert_eq!(action_state(&session("error")), ActionState::Stopped);
        assert_eq!(
            action_state(&inactive(session("stopped"))),
            ActionState::Inactive,
            "parked wins over stopped"
        );
        assert_eq!(action_state(&abandoned()), ActionState::Abandoned);
    }

    #[test]
    fn actions_by_state_offer_worktree_variants_only_with_a_worktree() {
        assert_eq!(
            menu_actions(&session("idle")),
            [A::Rename, A::StopKeepWorktree]
        );
        assert_eq!(
            menu_actions(&with_worktree(session("idle"))),
            [A::Rename, A::StopKeepWorktree, A::StopDeleteWorktree]
        );
        assert_eq!(
            menu_actions(&session("stopped")),
            [A::Restart, A::RemovePane]
        );
        assert_eq!(
            menu_actions(&with_worktree(session("stopped"))),
            [A::Restart, A::Park, A::RemovePaneDeleteWorktree]
        );
        assert_eq!(
            menu_actions(&inactive(session("stopped"))),
            [A::Resume, A::RemoveFromSidebar]
        );
        assert_eq!(
            menu_actions(&inactive(with_worktree(session("stopped")))),
            [
                A::Resume,
                A::RemoveFromSidebar,
                A::RemoveFromSidebarDeleteWorktree
            ]
        );
        assert_eq!(
            overlay_actions(&with_worktree(session("stopped"))),
            [A::Restart, A::Park, A::RemovePaneDeleteWorktree]
        );
        assert_eq!(
            menu_actions(&with_worktree(abandoned())),
            [A::ResumeAbandoned, A::DismissAbandoned],
            "no Restart and no worktree delete for an abandoned session"
        );
    }

    #[test]
    fn actions_menu_folds_the_stop_choices_under_one_entry() {
        let running = with_worktree(session("idle"));
        assert_eq!(
            menu_entries(&running, MenuMode::Actions),
            [MenuEntry::Action(A::Rename), MenuEntry::StopChoice]
        );
        assert_eq!(
            menu_entries(&running, MenuMode::StopChoice),
            [
                MenuEntry::Action(A::StopDeleteWorktree),
                MenuEntry::Action(A::StopKeepWorktree),
                MenuEntry::Cancel
            ]
        );
        assert_eq!(
            menu_entries(&session("idle"), MenuMode::StopChoice),
            [MenuEntry::Action(A::StopKeepWorktree), MenuEntry::Cancel]
        );
        assert_eq!(
            menu_entries(&session("stopped"), MenuMode::Actions),
            [
                MenuEntry::Action(A::Restart),
                MenuEntry::Action(A::RemovePane)
            ]
        );
    }

    #[test]
    fn actions_stop_choice_of_a_session_that_stopped_shows_its_actions() {
        assert_eq!(
            menu_entries(&session("stopped"), MenuMode::StopChoice),
            [
                MenuEntry::Action(A::Restart),
                MenuEntry::Action(A::RemovePane)
            ]
        );
    }

    #[test]
    fn actions_labels_and_confirm_flags() {
        assert_eq!(A::StopKeepWorktree.label(true, true), "Stop, keep worktree");
        assert_eq!(A::StopKeepWorktree.label(false, true), "Stop session");
        assert_eq!(A::Park.label(true, true), "Remove pane, keep worktree");
        assert_eq!(A::Park.label(true, false), "Remove session, keep worktree");
        assert_eq!(A::RemovePane.label(false, false), "Remove session");
        assert_eq!(
            A::RemovePaneDeleteWorktree.label(true, true),
            "Remove pane and delete worktree"
        );
        assert_eq!(
            A::RemovePaneDeleteWorktree.label(true, false),
            "Remove session and delete worktree"
        );
        assert_eq!(A::Restart.pending_label(), Some("Restarting…"));
        assert_eq!(A::Park.pending_label(), None);
        let deleting: Vec<SessionAction> = [
            A::Rename,
            A::StopKeepWorktree,
            A::StopDeleteWorktree,
            A::Restart,
            A::Park,
            A::RemovePane,
            A::RemovePaneDeleteWorktree,
            A::Resume,
            A::RemoveFromSidebar,
            A::RemoveFromSidebarDeleteWorktree,
            A::ResumeAbandoned,
            A::DismissAbandoned,
        ]
        .into_iter()
        .filter(|a| a.deletes_worktree())
        .collect();
        assert_eq!(
            deleting,
            [
                A::StopDeleteWorktree,
                A::RemovePaneDeleteWorktree,
                A::RemoveFromSidebarDeleteWorktree
            ]
        );
    }

    #[test]
    fn actions_exit_code_texts() {
        let mut s = session("stopped");
        assert!(pane_shows_exit(&s));
        assert_eq!(exit_code_label(&s), "exit code ?");
        s.exit_code = Some(3);
        assert_eq!(exit_code_label(&s), "exit code 3");
        assert_eq!(exited_message(&s), "Session exited (code 3)");
        assert!(
            !pane_shows_exit(&session("error")),
            "only stopped, as in SessionPane"
        );
        assert!(!pane_shows_exit(&session("idle")));
        assert!(!pane_shows_exit(&abandoned()), "abandoned gets no overlay");
        assert!(header_shows_exit_code(&abandoned()));
    }

    #[test]
    fn actions_rename_trims_and_blank_restores_the_default() {
        assert!(matches!(
            rename_message("s1", "  work  "),
            ClientMessage::RenameSession { session_id, label: Some(l) } if session_id == "s1" && l == "work"
        ));
        assert!(matches!(
            rename_message("s1", "   "),
            ClientMessage::RenameSession { label: None, .. }
        ));
    }

    #[test]
    fn actions_stop_parks_or_discards_only_without_a_pane() {
        let shown = sent(plan(
            A::StopKeepWorktree,
            &with_worktree(session("idle")),
            true,
        ));
        assert!(
            matches!(shown.as_slice(), [stop] if is_stop(stop)),
            "{shown:?}"
        );

        let parked = sent(plan(
            A::StopKeepWorktree,
            &with_worktree(session("idle")),
            false,
        ));
        assert!(
            matches!(parked.as_slice(), [stop, ClientMessage::ParkSession { session_id }] if is_stop(stop) && session_id == "s1"),
            "{parked:?}"
        );

        let discarded = sent(plan(A::StopKeepWorktree, &session("idle"), false));
        assert!(
            matches!(discarded.as_slice(), [stop, discard] if is_stop(stop) && is_discard(discard, &[])),
            "{discarded:?}"
        );
    }

    #[test]
    fn actions_worktree_deletes_go_through_the_confirm() {
        let running = with_worktree(session("idle"));
        assert!(matches!(
            plan(A::StopDeleteWorktree, &running, true),
            Step::ConfirmWorktreeDelete
        ));
        let stopped = with_worktree(session("stopped"));
        for action in [
            A::RemovePaneDeleteWorktree,
            A::RemoveFromSidebarDeleteWorktree,
        ] {
            for shown in [true, false] {
                assert!(
                    matches!(plan(action, &stopped, shown), Step::ConfirmWorktreeDelete),
                    "{action:?} never deletes in one click"
                );
            }
        }
        for action in [A::RemovePane, A::RemoveFromSidebar] {
            let msgs = sent(plan(action, &stopped, false));
            assert!(
                matches!(msgs.as_slice(), [d] if is_discard(d, &[])),
                "{msgs:?}"
            );
        }
        let park = sent(plan(A::Park, &stopped, true));
        assert!(
            matches!(park.as_slice(), [ClientMessage::ParkSession { session_id }] if session_id == "s1")
        );
    }

    #[test]
    fn actions_abandoned_resume_and_dismiss_use_the_abandoned_messages() {
        let resume = sent(plan(A::ResumeAbandoned, &abandoned(), true));
        assert!(
            matches!(resume.as_slice(), [ClientMessage::ResumeAbandoned { session_id }] if session_id == "s1"),
            "{resume:?}"
        );
        let dismiss = sent(plan(A::DismissAbandoned, &abandoned(), true));
        assert!(
            matches!(dismiss.as_slice(), [ClientMessage::DiscardAbandoned { session_id }] if session_id == "s1"),
            "{dismiss:?}"
        );
    }

    #[test]
    fn actions_rename_restart_and_resume_are_not_plain_sends() {
        let s = session("stopped");
        assert!(matches!(
            plan(A::Rename, &session("idle"), true),
            Step::EditName
        ));
        assert!(matches!(plan(A::Restart, &s, true), Step::Duplicate));
        assert!(matches!(
            plan(A::Resume, &inactive(s), false),
            Step::Duplicate
        ));
    }

    #[test]
    fn actions_header_stop_confirm_is_keyed_by_session() {
        let mut confirm = HeaderStopConfirm::default();
        assert!(!confirm.click("s1"), "the first click only arms");
        assert!(confirm.is_armed("s1"));
        assert_eq!(confirm.armed(), Some("s1"));
        assert!(
            !confirm.click("s2"),
            "another session arms its own, never stops on s1's arm"
        );
        assert!(!confirm.is_armed("s1"));
        assert!(confirm.click("s2"));
        assert_eq!(confirm.armed(), None, "acting disarms");
        assert!(!confirm.disarm());
        confirm.click("s1");
        assert!(confirm.disarm());
        assert!(!confirm.click("s1"), "a disarm needs two clicks again");
    }

    fn named(id: &str) -> SessionSnapshot {
        let mut s = session("idle");
        s.id = id.to_owned();
        s
    }

    fn hsplit(first: GridNode, second: GridNode) -> GridNode {
        GridNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    fn diff_tab(id: &str) -> protocol::TabEntry {
        serde_json::from_value(json!({
            "id": id,
            "name": id,
            "content": { "kind": "diff", "repo_id": "r1", "path": "a.rs", "against": null },
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .expect("diff tab fixture")
    }

    fn pane_focus(tab_id: &str, pane_id: &str) -> PlacedFocus {
        PlacedFocus::Pane {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
        }
    }

    fn has_discard(messages: &[ClientMessage]) -> bool {
        messages
            .iter()
            .any(|m| matches!(m, ClientMessage::DiscardSession { .. }))
    }

    fn rows_of(s: &SessionSnapshot, tabs: &TabsModel) -> Vec<SessionRow> {
        session_rows(s, tabs, &[])
            .into_iter()
            .map(|offered| offered.row)
            .collect()
    }

    #[test]
    fn actions_duplicate_row_is_offered_for_non_headless_sessions() {
        let offered = OfferedRow {
            row: SessionRow::Duplicate,
            disabled: None,
        };
        let tabs = model_with(&[tab("t1", &pane("p1", Some("s1")))]);
        for s in [
            session("idle"),
            session("stopped"),
            inactive(with_worktree(session("stopped"))),
            abandoned(),
        ] {
            assert_eq!(
                session_rows(&s, &tabs, &[]).first(),
                Some(&offered),
                "{:?}",
                action_state(&s)
            );
        }
        assert_eq!(SessionRow::Duplicate.label(), "Duplicate ▸");
    }

    #[test]
    fn actions_headless_session_duplicate_row_is_disabled_with_tooltip() {
        let tabs = model_with(&[tab("t1", &pane("p1", Some("s1")))]);
        for status in ["working", "stopped"] {
            let mut s = session(status);
            s.mode = SessionMode::Headless;
            assert_eq!(
                session_rows(&s, &tabs, &[]).first(),
                Some(&OfferedRow {
                    row: SessionRow::Duplicate,
                    disabled: Some(HEADLESS_DUPLICATE_TIP),
                }),
                "{status}"
            );
        }
        assert_eq!(
            HEADLESS_DUPLICATE_TIP,
            "Headless sessions are one-shot kickoffs; spawn a new one instead"
        );
    }

    #[test]
    fn actions_bound_session_offers_move_to_not_add() {
        let tabs = model_with(&[
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", None)),
        ]);
        for s in [session("idle"), session("stopped")] {
            assert_eq!(
                rows_of(&s, &tabs),
                [SessionRow::Duplicate, SessionRow::MoveTo],
                "{:?}",
                s.status
            );
        }
        assert_eq!(SessionRow::MoveTo.label(), "Move to ▸");
        assert_eq!(SessionRow::MoveTo.selector(), "session-menu-move");
    }

    #[test]
    fn actions_unbound_session_offers_add_to_current_and_new_tab() {
        let mut tabs = model_with(&[tab("t1", &pane("p1", None)), diff_tab("d1")]);
        tabs.activate("t1");
        let current = SessionRow::AddToCurrentTab { new_tab: false };
        for s in [session("idle"), inactive(session("stopped"))] {
            assert_eq!(
                rows_of(&s, &tabs),
                [SessionRow::Duplicate, current, SessionRow::AddToNewTab],
                "{:?}",
                action_state(&s)
            );
        }
        assert_eq!(current.label(), "Add to current tab");
        assert_eq!(current.selector(), "session-menu-add-current");
        assert_eq!(SessionRow::AddToNewTab.label(), "Add to new tab");
        assert_eq!(SessionRow::AddToNewTab.selector(), "session-menu-add-new");
    }

    #[test]
    fn actions_one_add_to_new_tab_row_when_current_would_open_one() {
        let mut tabs = model_with(&[tab("t1", &pane("p1", None)), diff_tab("d1")]);
        tabs.activate("d1");
        let into_new = SessionRow::AddToCurrentTab { new_tab: true };
        assert_eq!(
            rows_of(&session("idle"), &tabs),
            [SessionRow::Duplicate, into_new],
            "with a diff tab active the current-tab row opens a tab, so it is the only one"
        );
        assert_eq!(into_new.label(), "Add to new tab");
        assert_eq!(into_new.selector(), "session-menu-add-current");
        let no_tabs = model_with(&[]);
        assert_eq!(
            rows_of(&session("idle"), &no_tabs),
            [SessionRow::Duplicate, into_new],
            "no tab at all"
        );
    }

    #[test]
    fn actions_add_rows_hidden_when_attach_is_refused() {
        let tabs = model_with(&[tab("t1", &pane("p1", None))]);
        let mut s = session("working");
        s.mode = SessionMode::Headless;
        assert_eq!(rows_of(&s, &tabs), [SessionRow::Duplicate]);
        let shown = model_with(&[tab("t1", &pane("p1", Some("s1")))]);
        assert_eq!(
            rows_of(&s, &shown),
            [SessionRow::Duplicate, SessionRow::MoveTo],
            "a headless pane still moves"
        );
    }

    #[test]
    fn actions_reveal_only_for_own_worktree() {
        let tabs = model_with(&[tab("t1", &pane("p1", Some("s1")))]);
        let mut own = with_worktree(session("idle"));
        own.members[0].worktree_path = "C:/wt/a".to_owned();
        assert_eq!(worktree_to_reveal(&own), Some("C:/wt/a"));
        assert_eq!(
            rows_of(&own, &tabs),
            [
                SessionRow::Duplicate,
                SessionRow::MoveTo,
                SessionRow::RevealWorktree
            ]
        );
        assert_eq!(SessionRow::RevealWorktree.label(), "Reveal worktree");
        assert_eq!(SessionRow::RevealWorktree.selector(), "session-menu-reveal");

        let blank = with_worktree(session("idle"));
        assert_eq!(worktree_to_reveal(&blank), None, "no path to open");
        let mut shared = session("idle");
        shared.members[0].worktree_path = "C:/repo".to_owned();
        assert_eq!(worktree_to_reveal(&shared), None, "not its own worktree");
        let mut second = with_worktree(session("idle"));
        second.members[1].worktree_path = "C:/wt/b".to_owned();
        assert_eq!(worktree_to_reveal(&second), None, "only the first member");
        for s in [blank, shared, second] {
            assert_eq!(
                rows_of(&s, &tabs),
                [SessionRow::Duplicate, SessionRow::MoveTo]
            );
        }
    }

    #[test]
    fn actions_move_to_lists_grid_tabs_not_showing_the_session() {
        let tabs = [
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", Some("s9"))),
            tab("t3", &hsplit(pane("a", Some("s9")), pane("b", Some("s1")))),
            diff_tab("d1"),
            tab("t4", &pane("p4", None)),
        ];
        let choice = |selector: &str, label: &str, choice: MoveTarget| SubmenuLine::Choice {
            selector: selector.to_owned(),
            label: label.to_owned(),
            choice,
        };
        let head = [
            SubmenuLine::Back {
                selector: "move-back",
            },
            choice("move-new-tab", "New tab", MoveTarget::NewTab),
        ];
        let mut expected = head.to_vec();
        expected.extend([
            SubmenuLine::Separator,
            choice("move-tab-t2", "t2", MoveTarget::Tab("t2".to_owned())),
            choice("move-tab-t4", "t4", MoveTarget::Tab("t4".to_owned())),
        ]);
        assert_eq!(move_lines(&tabs, "s1"), expected);
        let alone = [tab("t1", &pane("p1", Some("s1"))), diff_tab("d1")];
        assert_eq!(
            move_lines(&alone, "s1"),
            head,
            "no separator when no tab follows"
        );
    }

    #[test]
    fn actions_move_source_is_the_right_clicked_pane_else_first_binding() {
        let tabs = [
            tab("t1", &pane("p1", Some("s9"))),
            tab("t2", &hsplit(pane("a", Some("s1")), pane("b", Some("s1")))),
            tab("t3", &pane("c", Some("s1"))),
        ];
        let ids = |t: &str, p: &str| (t.to_owned(), p.to_owned());
        assert_eq!(
            move_source(&tabs, "s1", Some(&ids("t3", "c"))),
            Some(ids("t3", "c")),
            "the right-clicked pane"
        );
        assert_eq!(
            move_source(&tabs, "s1", Some(&ids("t2", "b"))),
            Some(ids("t2", "b"))
        );
        assert_eq!(
            move_source(&tabs, "s1", None),
            Some(ids("t2", "a")),
            "else the first binding"
        );
        for stale in [ids("t1", "p1"), ids("gone", "x")] {
            assert_eq!(
                move_source(&tabs, "s1", Some(&stale)),
                Some(ids("t2", "a")),
                "{stale:?} no longer shows s1"
            );
        }
        assert_eq!(move_source(&tabs, "s7", None), None, "no pane shows it");
    }

    #[test]
    fn actions_duplicate_to_new_tab_creates_a_tab_and_keeps_the_original() {
        let tabs = model_with(&[tab("t1", &pane("p1", Some("s1")))]);
        let mut dups = Duplicates::default();
        let request = dups.request("s1", "req-1".to_owned(), DuplicateTarget::NewTab);
        assert!(matches!(
            request,
            Some(ClientMessage::DuplicateSession { session_id, request_id: Some(id) }) if session_id == "s1" && id == "req-1"
        ));
        assert!(!dups.is_pending("s1"), "a copy is no restart on the way");
        assert!(dups.has_request("req-1"));
        let placed = dups
            .place("req-1", &named("s2"), &tabs, &[])
            .expect("placed");
        assert_eq!(placed.focus, PlacedFocus::NewTab);
        assert!(
            matches!(placed.messages.as_slice(), [
                ClientMessage::CreateTab { name: None, initial_session_id: Some(id) },
            ] if id == "s2"),
            "the copy opens a tab and nothing is discarded: {:?}",
            placed.messages
        );
    }

    #[test]
    fn actions_duplicate_into_a_tab_places_by_pane_target() {
        let tabs = model_with(&[
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &hsplit(pane("a", Some("s9")), pane("b", None))),
            tab("t3", &pane("c", Some("s9"))),
            diff_tab("d1"),
        ]);
        let place = |target: DuplicateTarget| {
            let mut dups = Duplicates::default();
            dups.request("s1", "req".to_owned(), target);
            dups.place("req", &named("s2"), &tabs, &[]).expect("placed")
        };

        let empty = place(DuplicateTarget::Tab("t2".to_owned()));
        assert_eq!(empty.focus, pane_focus("t2", "b"));
        assert!(
            matches!(empty.messages.as_slice(), [
                ClientMessage::ReplacePaneSession { tab_id, pane_id, session_id: Some(id) },
            ] if tab_id == "t2" && pane_id == "b" && id == "s2"),
            "the tab's empty pane takes the copy: {:?}",
            empty.messages
        );

        let full = place(DuplicateTarget::Tab("t3".to_owned()));
        assert_eq!(full.focus, PlacedFocus::Tab("t3".to_owned()));
        assert!(
            matches!(full.messages.as_slice(), [
                ClientMessage::SplitPane { tab_id, pane_id, new_session_id: Some(id), .. },
            ] if tab_id == "t3" && pane_id == "c" && id == "s2"),
            "a full tab splits its pane: {:?}",
            full.messages
        );

        for gone in ["gone", "d1"] {
            let placed = place(DuplicateTarget::Tab(gone.to_owned()));
            assert_eq!(placed.focus, PlacedFocus::NewTab, "{gone}");
            assert!(
                matches!(
                    placed.messages.as_slice(),
                    [ClientMessage::CreateTab { .. }]
                ),
                "{gone}: {:?}",
                placed.messages
            );
        }
    }

    #[test]
    fn actions_restart_still_replaces_and_discards() {
        let tabs = model_with(&[tab("t1", &pane("p1", Some("s1")))]);
        let mut dups = Duplicates::default();
        dups.request("s1", "copy".to_owned(), DuplicateTarget::NewTab);
        assert!(
            dups.request("s1", "restart".to_owned(), DuplicateTarget::Restart)
                .is_some(),
            "a copy on the way does not hold up a restart"
        );
        assert!(dups.is_pending("s1"));
        assert!(
            dups.request("s1", "again".to_owned(), DuplicateTarget::Restart)
                .is_none()
        );
        assert!(
            dups.request("s1", "copy-2".to_owned(), DuplicateTarget::NewTab)
                .is_some(),
            "copies are not held up by the restart"
        );

        let restarted = dups
            .place("restart", &named("s3"), &tabs, &[])
            .expect("placed");
        assert_eq!(restarted.focus, pane_focus("t1", "p1"));
        assert!(
            matches!(restarted.messages.as_slice(), [
                ClientMessage::ReplacePaneSession { tab_id, pane_id, session_id: Some(id) },
                discard,
            ] if tab_id == "t1" && pane_id == "p1" && id == "s3" && is_discard(discard, &[])),
            "{:?}",
            restarted.messages
        );
        assert!(!dups.is_pending("s1"));
        let copied = dups
            .place("copy", &named("s2"), &tabs, &[])
            .expect("placed");
        assert!(!has_discard(&copied.messages), "{:?}", copied.messages);
    }

    #[test]
    fn actions_duplicate_replaces_every_pane_of_the_original_then_discards_it() {
        let mut dups = Duplicates::default();
        let request = dups.request("s1", "req-1".to_owned(), DuplicateTarget::Restart);
        assert!(matches!(
            request,
            Some(ClientMessage::DuplicateSession { session_id, request_id: Some(id) }) if session_id == "s1" && id == "req-1"
        ));
        let tabs = model_with(&[
            tab(
                "t1",
                &hsplit(pane("p1", Some("s1")), pane("p2", Some("s9"))),
            ),
            tab("t2", &pane("p3", Some("s1"))),
        ]);
        assert!(dups.place("other", &named("s2"), &tabs, &[]).is_none());
        let placed = dups
            .place("req-1", &named("s2"), &tabs, &[])
            .expect("the matching reply places the duplicate");
        assert_eq!(placed.focus, pane_focus("t1", "p1"));
        let replaced: Vec<(&str, &str)> = placed
            .messages
            .iter()
            .filter_map(|m| match m {
                ClientMessage::ReplacePaneSession {
                    tab_id,
                    pane_id,
                    session_id,
                } if session_id.as_deref() == Some("s2") => {
                    Some((tab_id.as_str(), pane_id.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(replaced, [("t1", "p1"), ("t2", "p3")]);
        assert!(
            placed.messages.last().is_some_and(|m| is_discard(m, &[])),
            "the discard goes last: {:?}",
            placed.messages
        );
        assert!(
            dups.place("req-1", &named("s3"), &tabs, &[]).is_none(),
            "each request is placed once"
        );
    }

    #[test]
    fn actions_duplicate_of_a_session_no_pane_shows_opens_a_tab() {
        let mut dups = Duplicates::default();
        dups.request("s1", "req-1".to_owned(), DuplicateTarget::Restart);
        let tabs = model_with(&[tab("t1", &pane("p1", None))]);
        let placed = dups
            .place("req-1", &named("s2"), &tabs, &[])
            .expect("placed");
        assert_eq!(placed.focus, PlacedFocus::NewTab);
        assert!(
            matches!(placed.messages.as_slice(), [
                ClientMessage::CreateTab { name: None, initial_session_id: Some(id) },
                discard,
            ] if id == "s2" && is_discard(discard, &[])),
            "{:?}",
            placed.messages
        );
    }

    #[test]
    fn actions_duplicate_is_one_at_a_time_until_answered_or_cleared() {
        let restart = || DuplicateTarget::Restart;
        let mut dups = Duplicates::default();
        assert!(dups.request("s1", "req-1".to_owned(), restart()).is_some());
        assert!(dups.is_pending("s1"));
        assert!(dups.has_request("req-1"));
        assert!(
            dups.request("s1", "req-2".to_owned(), restart()).is_none(),
            "a second restart of s1 while the first is on the way"
        );
        assert!(dups.request("s2", "req-3".to_owned(), restart()).is_some());
        assert!(!dups.fail("unknown"));
        assert!(dups.fail("req-1"));
        assert!(!dups.is_pending("s1"));
        assert!(dups.request("s1", "req-4".to_owned(), restart()).is_some());
        dups.clear();
        assert!(!dups.is_pending("s1") && !dups.is_pending("s2"));
    }
}
