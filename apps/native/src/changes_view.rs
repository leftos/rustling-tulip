//! The changes part of a source-control section: the Staged and Changes
//! buckets with their folded trees, the file rows' hover buttons and
//! right-click menu, the banner a failed git write leaves, and the discard
//! confirm. The state lives in [`crate::source_control`],
//! [`crate::sc_writes`] and [`crate::discard_confirm`]; this renders it and
//! forwards the clicks.

use gpui::{
    AnyElement, ClickEvent, Context, Div, ElementId, FocusHandle, FontWeight, Keystroke,
    MouseButton, MouseDownEvent, Pixels, Point, SharedString, Stateful, Window, anchored, deferred,
    div, prelude::*, px,
};
use protocol::{ClientMessage, DaemonMessage, GitFileChange};

use crate::discard_confirm::{DiscardButton, DiscardConfirm};
use crate::notice_view::modal_panel;
use crate::notices::ToastKind;
use crate::sc_writes::{ScWrites, WriteOp, bucket_paths, row_paths};
use crate::session_menu::{backdrop, dialog_button, menu_frame, menu_item, menu_separator};
use crate::source_control::{Bucket, Folder, Part, ScKey, ScModel, Status, build_tree};
use crate::source_control_view::ScSectionRow;
use crate::{BORDER, DANGER, DANGER_BG, HOVER_BG, MUTED, RootView, TEXT, tooltip};

/// A section body before its status arrives.
pub const LOADING_TEXT: &str = "loading…";
/// A loaded section body with nothing changed.
pub const CLEAN_TEXT: &str = "working tree clean";
/// What a section body says before the reason its status read failed.
const FAILED_PREFIX: &str = "couldn't load status: ";
/// The discard confirm's warning.
pub const DISCARD_BODY: &str = "Discarded edits cannot be recovered. Untracked files are removed; modified and deleted files are restored from the index.";
const DISMISS_TIP: &str = "Dismiss error";
const ROW_PADDING: f32 = 8.0;
const ROW_HEIGHT: f32 = 20.0;
const INDENT_STEP: f32 = 12.0;
const STATUS_WIDTH: f32 = 14.0;
const CARET_WIDTH: f32 = 10.0;
const PATHS_MAX_HEIGHT: f32 = 200.0;
/// The status letters' colours, as the Tauri changes tree draws them.
const MODIFIED: u32 = 0x00d4_a72c;
const ADDED: u32 = 0x004e_c9b0;
const RENAMED: u32 = 0x0056_9cd6;
const UNTRACKED: u32 = 0x006a_9955;

/// What a changes-view button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScAction {
    StageAll,
    UnstageAll,
    DiscardAll,
    Stage,
    Unstage,
    Discard,
}

impl ScAction {
    /// The label of the button that runs it.
    fn label(self) -> &'static str {
        match self {
            Self::StageAll => "Stage all",
            Self::UnstageAll => "Unstage all",
            Self::DiscardAll => "Discard all",
            Self::Stage => "+",
            Self::Unstage => "−",
            Self::Discard => "↺",
        }
    }

    /// The tooltip of a row's glyph button.
    fn tip(self) -> Option<&'static str> {
        match self {
            Self::Stage => Some("Stage"),
            Self::Unstage => Some("Unstage"),
            Self::Discard => Some("Discard"),
            Self::StageAll | Self::UnstageAll | Self::DiscardAll => None,
        }
    }

    fn is_discard(self) -> bool {
        matches!(self, Self::Discard | Self::DiscardAll)
    }
}

/// A bucket action, a row's hover button or a right-click menu item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScButton {
    pub selector: String,
    pub label: &'static str,
    pub tooltip: Option<&'static str>,
    pub action: ScAction,
    /// Drawn in the danger colour.
    pub danger: bool,
    /// `false` while the tree has a write out.
    pub enabled: bool,
    /// In a menu, a separator line sits above it.
    pub separated: bool,
}

/// A bucket's header and its folded tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScBucketRow {
    pub bucket: Bucket,
    pub selector: String,
    /// `Staged Changes` or `Changes`.
    pub title: &'static str,
    /// The bucket's entries.
    pub count: usize,
    pub actions: Vec<ScButton>,
    /// Every path the bucket actions send.
    pub paths: Vec<String>,
    pub rows: Vec<ScTreeRow>,
}

/// One row of a bucket's tree, in drawing order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScTreeRow {
    Folder(ScFolderRow),
    File(ScFileRow),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScFolderRow {
    pub selector: String,
    pub label: String,
    pub full_path: String,
    pub depth: usize,
    /// The files anywhere beneath it.
    pub files: usize,
    pub collapsed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScFileRow {
    pub selector: String,
    /// The status letter.
    pub status: String,
    /// The file's basename.
    pub name: String,
    /// The full path, or `<from_path> → <path>` for a rename or copy.
    pub tooltip: String,
    pub depth: usize,
    /// What its buttons and menu items send.
    pub paths: Vec<String>,
    /// The hover buttons, left to right.
    pub buttons: Vec<ScButton>,
}

/// The open discard confirm, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardConfirmView {
    pub title: String,
    pub paths: Vec<String>,
    /// The focused button's selector.
    pub focused: &'static str,
}

/// An expanded section's body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ScChanges {
    pub banner: Option<String>,
    /// `loading…`, the failure or `working tree clean`, instead of buckets.
    pub body: Option<String>,
    pub buckets: Vec<ScBucketRow>,
}

/// The changes view's state on the root view.
pub(crate) struct ChangesUi {
    /// The writes out and the banners failed ones left.
    pub(crate) writes: ScWrites,
    /// A file row's right-click menu, while open.
    pub(crate) file_menu: Option<ScFileMenu>,
    /// The discard confirm, while open.
    pub(crate) discard: Option<DiscardConfirm>,
    /// The discard confirm's keyboard focus.
    pub(crate) discard_focus: FocusHandle,
}

impl ChangesUi {
    pub(crate) fn new(discard_focus: FocusHandle) -> Self {
        Self {
            writes: ScWrites::default(),
            file_menu: None,
            discard: None,
            discard_focus,
        }
    }
}

/// The open right-click menu of a file row.
pub(crate) struct ScFileMenu {
    key: ScKey,
    bucket: Bucket,
    paths: Vec<String>,
    /// Where the right-click was, in window coordinates.
    at: Point<Pixels>,
}

/// What a bucket's tree rows are built from.
struct TreeCtx<'a> {
    model: &'a ScModel,
    key: &'a ScKey,
    id: String,
    bucket: Bucket,
    enabled: bool,
}

fn bucket_name(bucket: Bucket) -> &'static str {
    match bucket {
        Bucket::Staged => "staged",
        Bucket::Changes => "changes",
    }
}

fn button(selector: String, action: ScAction, enabled: bool) -> ScButton {
    ScButton {
        selector,
        label: action.label(),
        tooltip: action.tip(),
        action,
        danger: action.is_discard(),
        enabled,
        separated: false,
    }
}

fn bucket_actions(bucket: Bucket, id: &str, enabled: bool) -> Vec<ScButton> {
    match bucket {
        Bucket::Staged => vec![button(
            format!("sc-unstage-all-{id}"),
            ScAction::UnstageAll,
            enabled,
        )],
        Bucket::Changes => vec![
            button(
                format!("sc-discard-all-{id}"),
                ScAction::DiscardAll,
                enabled,
            ),
            button(format!("sc-stage-all-{id}"), ScAction::StageAll, enabled),
        ],
    }
}

fn file_row(ctx: &TreeCtx<'_>, change: &GitFileChange, depth: usize) -> ScFileRow {
    let path = &change.path;
    let id = &ctx.id;
    let buttons = match ctx.bucket {
        Bucket::Staged => vec![button(
            format!("sc-unstage-{id}|{path}"),
            ScAction::Unstage,
            ctx.enabled,
        )],
        Bucket::Changes => vec![
            button(
                format!("sc-discard-{id}|{path}"),
                ScAction::Discard,
                ctx.enabled,
            ),
            button(
                format!("sc-stage-{id}|{path}"),
                ScAction::Stage,
                ctx.enabled,
            ),
        ],
    };
    ScFileRow {
        selector: format!("sc-row-{}-{id}|{path}", bucket_name(ctx.bucket)),
        status: change.status.clone(),
        name: path.rsplit(['/', '\\']).next().unwrap_or(path).to_owned(),
        tooltip: change
            .from_path
            .as_ref()
            .map_or_else(|| path.clone(), |from| format!("{from} → {path}")),
        depth,
        paths: row_paths(change),
        buttons,
    }
}

fn file_count(folder: &Folder) -> usize {
    folder.files.len() + folder.folders.iter().map(file_count).sum::<usize>()
}

/// Appends `folder`'s rows at `depth`: its folders, each followed by its
/// own rows unless collapsed, then its files.
fn push_tree_rows(ctx: &TreeCtx<'_>, folder: &Folder, depth: usize, rows: &mut Vec<ScTreeRow>) {
    for child in &folder.folders {
        let collapsed = ctx
            .model
            .is_folder_collapsed(ctx.key, ctx.bucket, &child.full_path);
        rows.push(ScTreeRow::Folder(ScFolderRow {
            selector: format!(
                "sc-folder-{}-{}|{}",
                bucket_name(ctx.bucket),
                ctx.id,
                child.full_path
            ),
            label: child.label.clone(),
            full_path: child.full_path.clone(),
            depth,
            files: file_count(child),
            collapsed,
        }));
        if !collapsed {
            push_tree_rows(ctx, child, depth + 1, rows);
        }
    }
    for change in &folder.files {
        rows.push(ScTreeRow::File(file_row(ctx, change, depth)));
    }
}

fn status_color(status: &str) -> u32 {
    match status {
        "M" => MODIFIED,
        "A" => ADDED,
        "D" => DANGER,
        "R" => RENAMED,
        "?" => UNTRACKED,
        "U" => MUTED,
        _ => TEXT,
    }
}

/// The left padding of a tree row at `depth`.
fn indent(depth: usize) -> Pixels {
    let steps = u16::try_from(depth).map_or(f32::from(u16::MAX), f32::from);
    px(ROW_PADDING + INDENT_STEP * (steps + 1.0))
}

/// `▸` when folded, `▾` when open.
pub(crate) fn caret(collapsed: bool) -> Div {
    div()
        .flex_none()
        .w(px(CARET_WIDTH))
        .text_color(gpui::rgb(MUTED))
        .child(if collapsed { "▸" } else { "▾" })
}

impl RootView {
    /// Folds a daemon message into the source-control stores. An `Error`
    /// answering one of their status requests marks its section failed and
    /// raises no toast; it returns `true`, since nothing else is owed. A
    /// `GitWriteError` also raises a toast.
    pub(crate) fn on_sc_message(&mut self, msg: &DaemonMessage, cx: &mut Context<Self>) -> bool {
        let status_moved = self.sc.apply(msg);
        if self.changes.writes.apply(msg) || status_moved {
            cx.notify();
        }
        match msg {
            DaemonMessage::Error {
                message,
                request_id: Some(id),
            } if self.sc.fail_request(id, message) => {
                cx.notify();
                true
            }
            DaemonMessage::GitWriteError {
                operation, error, ..
            } => {
                let title = format!("Git {operation} failed");
                self.push_toast(ToastKind::Error, &title, Some(error.clone()), cx);
                false
            }
            _ => false,
        }
    }

    /// An expanded section's body for `key`: the banner, then the failure,
    /// `loading…`, `working tree clean` or the buckets.
    pub(crate) fn sc_changes(&self, key: &ScKey) -> ScChanges {
        let banner = self.changes.writes.banner(key).map(str::to_owned);
        let (body, buckets) = if let Some(reason) = self.sc.failure(key) {
            (Some(format!("{FAILED_PREFIX}{reason}")), Vec::new())
        } else {
            match self.sc.status(key) {
                None => (Some(LOADING_TEXT.to_owned()), Vec::new()),
                Some(status) if status.staged.is_empty() && status.changes.is_empty() => {
                    (Some(CLEAN_TEXT.to_owned()), Vec::new())
                }
                Some(status) => (None, self.sc_buckets(key, status)),
            }
        };
        ScChanges {
            banner,
            body,
            buckets,
        }
    }

    /// The non-empty buckets of `status`, Staged first.
    fn sc_buckets(&self, key: &ScKey, status: &Status) -> Vec<ScBucketRow> {
        let enabled = self.changes.writes.pending(key).is_none();
        let id = key.id();
        [
            (Bucket::Staged, "Staged Changes", &status.staged),
            (Bucket::Changes, "Changes", &status.changes),
        ]
        .into_iter()
        .filter(|(_, _, changes)| !changes.is_empty())
        .map(|(bucket, title, changes)| {
            let ctx = TreeCtx {
                model: &self.sc,
                key,
                id: id.clone(),
                bucket,
                enabled,
            };
            let mut rows = Vec::new();
            push_tree_rows(&ctx, &build_tree(changes), 0, &mut rows);
            ScBucketRow {
                bucket,
                selector: format!("sc-bucket-{}-{id}", bucket_name(bucket)),
                title,
                count: changes.len(),
                actions: bucket_actions(bucket, &id, enabled),
                paths: bucket_paths(changes),
                rows,
            }
        })
        .collect()
    }

    /// A section header click: folds or unfolds its Changes part, and saves.
    pub(crate) fn toggle_sc_changes(&mut self, key: &ScKey, cx: &mut Context<Self>) {
        let loaded = self
            .sc
            .status(key)
            .map(|_| self.sc.badge_total(std::slice::from_ref(key)));
        let collapsed = self
            .sidebar
            .source_control()
            .is_collapsed(key, Part::Changes, loaded);
        if self
            .sidebar
            .set_sc_collapsed(key, Part::Changes, !collapsed)
        {
            self.save_ui();
        }
        cx.notify();
    }

    /// Drops the banners, the file menu and the discard confirm of trees
    /// that are no longer `sections`; returns whether the menu or the
    /// confirm went, so the caller hands the keyboard back.
    pub(crate) fn drop_stale_sc_changes(&mut self, sections: &[ScKey]) -> bool {
        self.changes.writes.retain_banners(sections);
        let stale_menu = self
            .changes
            .file_menu
            .as_ref()
            .is_some_and(|menu| !sections.contains(&menu.key));
        if stale_menu {
            self.changes.file_menu = None;
        }
        let stale_confirm = self
            .changes
            .discard
            .as_ref()
            .is_some_and(|confirm| !sections.contains(confirm.tree()));
        if stale_confirm {
            self.changes.discard = None;
        }
        stale_menu || stale_confirm
    }

    /// A dialog, a modal notice or the connection overlay is up, so the
    /// changes view opens nothing.
    fn sc_changes_blocked(&self) -> bool {
        self.exit.is_some()
            || self.spawn_dialog.is_some()
            || self.shell_dialog.is_some()
            || self.appearance_editor.is_some()
            || self.delete_dialog.is_some()
            || self.run_confirm.is_some()
            || self.changes.discard.is_some()
            || self.stash.drop.is_some()
            || self.notices.has_modal()
            || self.conn.overlay().is_some()
    }

    /// Runs `action` on `paths` of the tree `key`: a stage or unstage is
    /// sent and marks the tree pending; a discard opens the confirm. A tree
    /// with a write out takes nothing.
    fn run_sc_action(
        &mut self,
        key: &ScKey,
        action: ScAction,
        paths: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() || self.changes.writes.pending(key).is_some() {
            return;
        }
        let repo_id = key.repo_id.clone();
        let worktree_path = key.worktree.clone();
        match action {
            ScAction::Stage | ScAction::StageAll => {
                self.send(ClientMessage::StageFiles {
                    repo_id,
                    paths,
                    worktree_path,
                });
                self.changes.writes.start(key.clone(), WriteOp::Stage);
            }
            ScAction::Unstage | ScAction::UnstageAll => {
                self.send(ClientMessage::UnstageFiles {
                    repo_id,
                    paths,
                    worktree_path,
                });
                self.changes.writes.start(key.clone(), WriteOp::Unstage);
            }
            ScAction::Discard | ScAction::DiscardAll => {
                self.open_discard_confirm(key.clone(), paths, window, cx);
            }
        }
        cx.notify();
    }

    /// Whether a file row's right-click menu is open.
    #[must_use]
    pub fn sc_file_menu_open(&self) -> bool {
        self.changes.file_menu.is_some()
    }

    /// The open file menu's items, top to bottom.
    #[must_use]
    pub fn sc_file_menu_items(&self) -> Option<Vec<ScButton>> {
        let menu = self.changes.file_menu.as_ref()?;
        let enabled = self.changes.writes.pending(&menu.key).is_none();
        let item = |selector: &str, label: &'static str, action: ScAction| ScButton {
            selector: selector.to_owned(),
            label,
            tooltip: None,
            action,
            danger: action.is_discard(),
            enabled,
            separated: false,
        };
        Some(match menu.bucket {
            Bucket::Staged => vec![item(
                "sc-file-menu-unstage",
                "Unstage Changes",
                ScAction::Unstage,
            )],
            Bucket::Changes => vec![
                item("sc-file-menu-stage", "Stage Changes", ScAction::Stage),
                ScButton {
                    separated: true,
                    ..item("sc-file-menu-discard", "Discard Changes", ScAction::Discard)
                },
            ],
        })
    }

    /// Opens a file row's menu at the right-click, closing every other menu
    /// and taking the keyboard so Esc reaches it.
    fn open_sc_file_menu(&mut self, menu: ScFileMenu, window: &mut Window, cx: &mut Context<Self>) {
        if self.sc_changes_blocked() {
            return;
        }
        self.close_session_menu(window, cx);
        self.close_container_menu(window, cx);
        self.close_shell_menu(window, cx);
        self.close_tab_menu(window, cx);
        self.close_sc_picker(window, cx);
        self.changes.file_menu = Some(menu);
        self.menu_focus.focus(window);
        cx.notify();
    }

    /// Closes the file menu and hands the keyboard back to the active pane.
    pub(crate) fn close_sc_file_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.changes.file_menu.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// The open file menu over a layer that keeps a click outside it from
    /// reaching what lies beneath.
    pub(crate) fn sc_file_menu_layer(&self, cx: &mut Context<Self>) -> Option<[AnyElement; 2]> {
        let menu = self.changes.file_menu.as_ref()?;
        let items = self.sc_file_menu_items()?;
        let backdrop = div().absolute().top_0().left_0().size_full().occlude();
        let mut frame = menu_frame(
            "sc-file-menu",
            &self.menu_focus,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.close_sc_file_menu(window, cx);
                cx.stop_propagation();
            }),
        );
        for item in items {
            if item.separated {
                frame = frame.child(menu_separator());
            }
            frame = frame.child(file_menu_item(&item, &menu.key, menu.paths.clone(), cx));
        }
        let panel = anchored().position(menu.at).snap_to_window().child(frame);
        Some([
            backdrop.into_any_element(),
            deferred(panel).with_priority(1).into_any_element(),
        ])
    }

    /// Opens the discard confirm for `paths` of the tree `key`.
    fn open_discard_confirm(
        &mut self,
        key: ScKey,
        paths: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() || self.sc_changes_blocked() {
            return;
        }
        self.changes.discard = Some(DiscardConfirm::new(key, paths));
        self.changes.discard_focus.focus(window);
        cx.notify();
    }

    /// The open discard confirm, as text.
    #[must_use]
    pub fn discard_confirm_view(&self) -> Option<DiscardConfirmView> {
        let confirm = self.changes.discard.as_ref()?;
        Some(DiscardConfirmView {
            title: confirm.title(),
            paths: confirm.paths().to_vec(),
            focused: confirm.focused().selector(),
        })
    }

    /// Closes the confirm; the danger button first sends the discard and
    /// marks the tree pending.
    fn press_discard_button(
        &mut self,
        button: DiscardButton,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(confirm) = self.changes.discard.take() else {
            return;
        };
        if button == DiscardButton::Discard {
            let key = confirm.tree().clone();
            self.send(ClientMessage::DiscardChanges {
                repo_id: key.repo_id.clone(),
                paths: confirm.paths().to_vec(),
                worktree_path: key.worktree.clone(),
            });
            self.changes.writes.start(key, WriteOp::Discard);
        }
        self.after_notice_closed(window, cx);
    }

    /// Cancels the discard confirm, if open, as when the connection goes.
    pub(crate) fn close_discard_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.changes.discard.is_some() {
            self.press_discard_button(DiscardButton::Cancel, window, cx);
        }
    }

    /// A key while the discard confirm is open; returns whether it was.
    pub(crate) fn on_discard_confirm_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(confirm) = &mut self.changes.discard else {
            return false;
        };
        match confirm.key(keystroke.key.as_str()) {
            Some(button) => self.press_discard_button(button, window, cx),
            None => cx.notify(),
        }
        true
    }

    /// The discard confirm over a backdrop that takes every click beneath
    /// it.
    pub(crate) fn discard_confirm_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let confirm = self.changes.discard.as_ref()?;
        let title = confirm.title();
        let buttons: Vec<AnyElement> = DiscardButton::ALL
            .into_iter()
            .map(|button| {
                let (label, danger) = match button {
                    DiscardButton::Cancel => ("Cancel".to_owned(), false),
                    DiscardButton::Discard => (title.clone(), true),
                };
                dialog_button(
                    button.selector(),
                    label,
                    danger,
                    confirm.focused() == button,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.press_discard_button(button, window, cx);
                }))
                .into_any_element()
            })
            .collect();
        let close = dialog_button("discard-confirm-close", "✕".to_owned(), false, false).on_click(
            cx.listener(|this, _: &ClickEvent, window, cx| {
                this.press_discard_button(DiscardButton::Cancel, window, cx);
            }),
        );
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(close);
        let paths = div()
            .id("discard-confirm-paths")
            .debug_selector(|| "discard-confirm-paths".to_owned())
            .flex()
            .flex_col()
            .max_h(px(PATHS_MAX_HEIGHT))
            .overflow_y_scroll()
            .p(px(6.0))
            .border_1()
            .border_color(gpui::rgb(BORDER))
            .rounded(px(4.0))
            .children(confirm.paths().iter().map(|path| div().child(path.clone())));
        let panel = modal_panel("discard-confirm-panel")
            .track_focus(&self.changes.discard_focus)
            .child(header)
            .child(div().text_color(gpui::rgb(MUTED)).child(DISCARD_BODY))
            .child(paths)
            .child(div().flex().justify_end().gap(px(6.0)).children(buttons));
        Some(backdrop("discard-confirm-dialog", panel))
    }
}

/// A file menu item: closes the menu, then runs its action on the row's
/// paths. A disabled one is dimmed and takes no click.
fn file_menu_item(
    item: &ScButton,
    key: &ScKey,
    paths: Vec<String>,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let row = menu_item(&item.selector, item.label, item.danger);
    if !item.enabled {
        return row.opacity(0.5).cursor_default();
    }
    let key = key.clone();
    let action = item.action;
    row.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.close_sc_file_menu(window, cx);
        this.run_sc_action(&key, action, paths.clone(), window, cx);
    }))
}

/// A small text button that runs its action on `paths` of the tree `key`. A
/// disabled one is dimmed and takes no click.
fn action_button(
    button: &ScButton,
    key: &ScKey,
    paths: Vec<String>,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let name = button.selector.clone();
    let base = div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex_none()
        .px(px(4.0))
        .rounded(px(3.0))
        .text_color(gpui::rgb(if button.danger { DANGER } else { TEXT }))
        .child(button.label)
        .when_some(button.tooltip, |base, tip| base.tooltip(tooltip(tip)));
    if !button.enabled {
        return base.opacity(0.5);
    }
    let key = key.clone();
    let action = button.action;
    base.cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            cx.stop_propagation();
            this.run_sc_action(&key, action, paths.clone(), window, cx);
        }))
}

/// An expanded section's body elements: the banner, the text line and the
/// buckets with their trees.
pub(crate) fn changes_body(row: &ScSectionRow, cx: &mut Context<RootView>) -> Vec<AnyElement> {
    let key = &row.key;
    let id = &row.id;
    let mut out = Vec::new();
    if let Some(text) = &row.banner {
        out.push(banner_view(key, id, text.clone(), cx).into_any_element());
    }
    if let Some(text) = &row.body {
        out.push(
            div()
                .pl(px(ROW_PADDING * 2.0))
                .pr(px(ROW_PADDING))
                .text_color(gpui::rgb(MUTED))
                .child(text.clone())
                .into_any_element(),
        );
    }
    for bucket in &row.buckets {
        out.push(bucket_header(key, bucket, cx).into_any_element());
        for tree_entry in &bucket.rows {
            out.push(tree_row(key, bucket.bucket, tree_entry, cx));
        }
    }
    out
}

fn banner_view(key: &ScKey, id: &str, text: String, cx: &mut Context<RootView>) -> Stateful<Div> {
    let name = format!("sc-banner-{id}");
    let close_name = format!("sc-banner-close-{id}");
    let key = key.clone();
    let close = div()
        .id(ElementId::Name(SharedString::from(close_name.clone())))
        .debug_selector(|| close_name)
        .flex_none()
        .px(px(4.0))
        .rounded(px(3.0))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .tooltip(tooltip(DISMISS_TIP))
        .child("✕")
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            if this.changes.writes.dismiss(&key) {
                cx.notify();
            }
        }));
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .items_start()
        .gap(px(6.0))
        .mx(px(ROW_PADDING))
        .mb(px(4.0))
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .bg(gpui::rgb(DANGER_BG))
        .text_color(gpui::rgb(DANGER))
        .child(div().flex_1().min_w(px(0.0)).child(text))
        .child(close)
}

fn bucket_header(key: &ScKey, bucket: &ScBucketRow, cx: &mut Context<RootView>) -> Stateful<Div> {
    let name = bucket.selector.clone();
    let actions: Vec<Stateful<Div>> = bucket
        .actions
        .iter()
        .map(|button| action_button(button, key, bucket.paths.clone(), cx))
        .collect();
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .items_center()
        .gap(px(4.0))
        .h(px(ROW_HEIGHT))
        .px(px(ROW_PADDING))
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::SEMIBOLD)
                .child(bucket.title),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_color(gpui::rgb(MUTED))
                .child(bucket.count.to_string()),
        )
        .children(actions)
}

fn tree_row(
    key: &ScKey,
    bucket: Bucket,
    row: &ScTreeRow,
    cx: &mut Context<RootView>,
) -> AnyElement {
    match row {
        ScTreeRow::Folder(folder) => folder_row(key, bucket, folder, cx).into_any_element(),
        ScTreeRow::File(file) => file_row_view(key, bucket, file, cx).into_any_element(),
    }
}

fn folder_row(
    key: &ScKey,
    bucket: Bucket,
    folder: &ScFolderRow,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let name = folder.selector.clone();
    let key = key.clone();
    let full_path = folder.full_path.clone();
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .items_center()
        .gap(px(4.0))
        .h(px(ROW_HEIGHT))
        .pl(indent(folder.depth))
        .pr(px(ROW_PADDING))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .child(caret(folder.collapsed))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .child(folder.label.clone()),
        )
        .child(
            div()
                .flex_none()
                .text_color(gpui::rgb(MUTED))
                .child(folder.files.to_string()),
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.sc.toggle_folder(&key, bucket, &full_path);
            cx.notify();
        }))
}

fn file_row_view(
    key: &ScKey,
    bucket: Bucket,
    file: &ScFileRow,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let name = file.selector.clone();
    let group = SharedString::from(file.selector.clone());
    let buttons: Vec<Stateful<Div>> = file
        .buttons
        .iter()
        .map(|button| action_button(button, key, file.paths.clone(), cx))
        .collect();
    let menu_key = key.clone();
    let menu_paths = file.paths.clone();
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .group(group.clone())
        .flex()
        .items_center()
        .gap(px(4.0))
        .h(px(ROW_HEIGHT))
        .pl(indent(file.depth))
        .pr(px(ROW_PADDING))
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .tooltip(tooltip(file.tooltip.clone()))
        .child(
            div()
                .flex_none()
                .w(px(STATUS_WIDTH))
                .text_color(gpui::rgb(status_color(&file.status)))
                .child(file.status.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .child(file.name.clone()),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .gap(px(2.0))
                .invisible()
                .group_hover(group, gpui::Styled::visible)
                .children(buttons),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                let menu = ScFileMenu {
                    key: menu_key.clone(),
                    bucket,
                    paths: menu_paths.clone(),
                    at: event.position,
                };
                this.open_sc_file_menu(menu, window, cx);
            }),
        )
}
