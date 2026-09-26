//! The Stashes part of a source-control section: its header with the count,
//! the push row, the stash rows with their pop, apply and drop buttons, and
//! the confirm a drop opens. The state lives in [`crate::stashes`]; this
//! renders it and forwards the clicks.

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, FocusHandle, FontWeight,
    Keystroke, SharedString, Stateful, Subscription, Window, div, prelude::*, px,
};
use protocol::{ClientMessage, DaemonMessage, GitStash};
use std::collections::{HashMap, HashSet};

use crate::changes_view::{LOADING_TEXT, caret};
use crate::notice_view::modal_panel;
use crate::notices::ToastKind;
use crate::session_menu::{backdrop, dialog_button};
use crate::source_control::{Part, ScKey};
use crate::source_control_view::ScSectionRow;
use crate::stashes::{DropButton, DropConfirm, StashModel, StashOp, stash_date};
use crate::text_input::{TextInput, TextInputEvent};
use crate::{BORDER, DANGER, HOVER_BG, MUTED, RootView, TEXT, tooltip};

/// The part's title.
pub const STASHES_TITLE: &str = "Stashes";
/// The push input's placeholder.
pub const STASH_PLACEHOLDER: &str = "Message (optional)";
/// An expanded part whose repo has no stashes.
pub const NO_STASHES_TEXT: &str = "no stashes";
/// What the part says before the reason its list read failed.
const FAILED_PREFIX: &str = "couldn't load stashes: ";
/// The toast when a stash action is dropped because the list moved.
pub const STASH_LIST_CHANGED: &str = "Stash list changed";
const STASH_LABEL: &str = "Stash";
const STASHING_LABEL: &str = "Stashing…";
const ROW_PADDING: f32 = 8.0;
const ROW_HEIGHT: f32 = 20.0;

/// What a stash row's button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StashAction {
    Pop,
    Apply,
    Drop,
}

impl StashAction {
    const ALL: [Self; 3] = [Self::Pop, Self::Apply, Self::Drop];

    /// The button's label, which its selector also carries.
    fn label(self) -> &'static str {
        match self {
            Self::Pop => "pop",
            Self::Apply => "apply",
            Self::Drop => "drop",
        }
    }
}

/// A stash row's text button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashButton {
    pub selector: String,
    pub label: &'static str,
    pub action: StashAction,
    /// Drawn in the danger colour.
    pub danger: bool,
    /// `false` while the repo has a stash write out.
    pub enabled: bool,
}

/// One stash of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScStashRow {
    pub selector: String,
    /// `stash@{N}`.
    pub id: String,
    pub subject: String,
    /// The date as the daemon sent it; with the id and subject it names the
    /// stash a button acts on.
    pub created_at: String,
    /// The full subject, a newline, and the date as `YYYY-MM-DD HH:MM`.
    pub tooltip: String,
    /// `pop`, `apply`, `drop`, left to right.
    pub buttons: Vec<StashButton>,
}

/// The push row: the message input and the button that sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScStashPush {
    pub input_selector: String,
    pub selector: String,
    /// `Stash`, or `Stashing…` while a push is out.
    pub label: &'static str,
    /// `false` while the repo has a stash write out.
    pub enabled: bool,
}

/// A section's Stashes part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScStashes {
    /// The header's selector.
    pub selector: String,
    pub collapsed: bool,
    /// The repo's stash count, once its list has arrived.
    pub count: Option<usize>,
    /// The push row, while expanded.
    pub push: Option<ScStashPush>,
    /// `loading…`, `couldn't load stashes: <reason>` or `no stashes`,
    /// instead of rows, while expanded.
    pub body: Option<String>,
    /// The stashes, newest first, while expanded.
    pub rows: Vec<ScStashRow>,
}

/// The open drop confirm, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashDropView {
    pub title: String,
    pub body: String,
    /// The focused button's selector.
    pub focused: &'static str,
}

/// A section's push input and what listens to it.
struct StashInput {
    input: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

/// The stash view's state on the root view.
pub(crate) struct StashUi {
    /// The lists, requests, failures and writes out.
    pub(crate) model: StashModel,
    /// Each section's push input, by tree.
    inputs: HashMap<ScKey, StashInput>,
    /// The drop confirm, while open.
    pub(crate) drop: Option<DropConfirm>,
    /// The drop confirm's keyboard focus.
    drop_focus: FocusHandle,
}

impl StashUi {
    pub(crate) fn new(drop_focus: FocusHandle) -> Self {
        Self {
            model: StashModel::default(),
            inputs: HashMap::new(),
            drop: None,
            drop_focus,
        }
    }
}

impl RootView {
    /// Folds a daemon message into the stash store. An `Error` answering one
    /// of its list requests marks the repo failed and raises no toast; it
    /// returns `true`, since nothing else is owed. A list that no longer
    /// holds the stash the drop confirm names closes the confirm unsent.
    pub(crate) fn on_stash_message(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.stash.model.apply(msg) {
            cx.notify();
        }
        if let DaemonMessage::Stashes { repo_id, .. } = msg {
            let shifted = self.stash.drop.as_ref().is_some_and(|confirm| {
                confirm.tree().repo_id == *repo_id
                    && !self.stash.model.holds(repo_id, confirm.stash())
            });
            if shifted {
                self.stash.drop = None;
                self.stash_list_changed(cx);
                self.after_notice_closed(window, cx);
            }
        }
        match msg {
            DaemonMessage::Error {
                message,
                request_id: Some(id),
            } if self.stash.model.fail_request(id, message) => {
                cx.notify();
                true
            }
            _ => false,
        }
    }

    /// The Stashes part of the section `key`.
    pub(crate) fn sc_stashes(&self, key: &ScKey) -> ScStashes {
        let id = key.id();
        let repo = key.repo_id.as_str();
        let model = &self.stash.model;
        let list = model.list(repo);
        let collapsed = self
            .sidebar
            .source_control()
            .is_collapsed(key, Part::Stashes, None);
        let mut part = ScStashes {
            selector: format!("sc-stashes-{id}"),
            collapsed,
            count: list.map(<[_]>::len),
            push: None,
            body: None,
            rows: Vec::new(),
        };
        if collapsed {
            return part;
        }
        let pending = model.pending(repo);
        let enabled = pending.is_none();
        part.push = Some(ScStashPush {
            input_selector: format!("sc-stash-input-{id}"),
            selector: format!("sc-stash-push-{id}"),
            label: if pending == Some(StashOp::Push) {
                STASHING_LABEL
            } else {
                STASH_LABEL
            },
            enabled,
        });
        if let Some(reason) = model.failure(repo) {
            part.body = Some(format!("{FAILED_PREFIX}{reason}"));
            return part;
        }
        match list {
            None => part.body = Some(LOADING_TEXT.to_owned()),
            Some([]) => part.body = Some(NO_STASHES_TEXT.to_owned()),
            Some(stashes) => {
                part.rows = stashes
                    .iter()
                    .map(|stash| ScStashRow {
                        selector: format!("sc-stash-row-{id}|{}", stash.id),
                        id: stash.id.clone(),
                        subject: stash.subject.clone(),
                        created_at: stash.created_at.clone(),
                        tooltip: format!("{}\n{}", stash.subject, stash_date(&stash.created_at)),
                        buttons: StashAction::ALL
                            .into_iter()
                            .map(|action| StashButton {
                                selector: format!("sc-stash-{}-{id}|{}", action.label(), stash.id),
                                label: action.label(),
                                action,
                                danger: action == StashAction::Drop,
                                enabled,
                            })
                            .collect(),
                    })
                    .collect();
            }
        }
        part
    }

    /// Asks for the list of every repo of `sections` that has none, no
    /// request out and no failed request, once per repo, from the first of
    /// its trees.
    pub(crate) fn seed_stashes(&mut self, sections: &[ScKey]) {
        for key in self.stash.model.wanted_missing(sections) {
            self.request_stashes(key);
        }
    }

    /// Asks again for the list of every repo of `sections`, a failed one
    /// included, and forgets the repo's pending write.
    pub(crate) fn refresh_stashes(&mut self, sections: &[ScKey]) {
        let mut seen = HashSet::new();
        for key in sections {
            if seen.insert(key.repo_id.as_str()) {
                self.stash.model.clear_pending(&key.repo_id);
                self.request_stashes(key.clone());
            }
        }
    }

    /// Sends a list request for `key`'s repo under a fresh request id, so a
    /// failure comes back to its part.
    fn request_stashes(&mut self, key: ScKey) {
        let request_id = self.stash.model.request(&key.repo_id);
        self.send(ClientMessage::ListStashes {
            repo_id: key.repo_id,
            worktree_path: key.worktree,
            request_id: Some(request_id),
        });
    }

    /// Drops the push inputs and the drop confirm of trees that are no
    /// longer `sections`; returns whether the confirm went, so the caller
    /// hands the keyboard back.
    pub(crate) fn drop_stale_stash_ui(&mut self, sections: &[ScKey]) -> bool {
        self.stash.inputs.retain(|key, _| sections.contains(key));
        let stale = self
            .stash
            .drop
            .as_ref()
            .is_some_and(|confirm| !sections.contains(confirm.tree()));
        if stale {
            self.stash.drop = None;
        }
        stale
    }

    /// Gives every section of `sections` that has none a push input.
    pub(crate) fn make_stash_inputs(
        &mut self,
        sections: &[ScKey],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for section in sections {
            if self.stash.inputs.contains_key(section) {
                continue;
            }
            let input = cx.new(|cx| TextInput::new("", STASH_PLACEHOLDER, cx));
            let key = section.clone();
            let keys = cx.subscribe_in(
                &input,
                window,
                move |this, _, event: &TextInputEvent, window, cx| match event {
                    TextInputEvent::Submit => this.push_stash(&key, cx),
                    TextInputEvent::Cancel => this.focus_active_pane(window, cx),
                },
            );
            self.stash.inputs.insert(
                section.clone(),
                StashInput {
                    input,
                    _subscriptions: vec![keys],
                },
            );
        }
    }

    /// The push input of the section `key`, once made.
    pub(crate) fn stash_input(&self, key: &ScKey) -> Option<Entity<TextInput>> {
        self.stash.inputs.get(key).map(|entry| entry.input.clone())
    }

    /// The text in the push input of the section `key`.
    #[must_use]
    pub fn sc_stash_input(&self, key: &ScKey, cx: &App) -> Option<String> {
        self.stash
            .inputs
            .get(key)
            .map(|entry| entry.input.read(cx).text().to_owned())
    }

    /// The Stashes header: folds or unfolds the part, and saves.
    fn toggle_sc_stashes(&mut self, key: &ScKey, cx: &mut Context<Self>) {
        let collapsed = self
            .sidebar
            .source_control()
            .is_collapsed(key, Part::Stashes, None);
        if self
            .sidebar
            .set_sc_collapsed(key, Part::Stashes, !collapsed)
        {
            self.save_ui();
        }
        cx.notify();
    }

    /// Enter in the push input or its button: sends the trimmed message for
    /// the tree `key`, marks the repo pending and clears the input. A repo
    /// with a stash write out takes nothing.
    fn push_stash(&mut self, key: &ScKey, cx: &mut Context<Self>) {
        if self.stash.model.pending(&key.repo_id).is_some() {
            return;
        }
        let Some(input) = self.stash_input(key) else {
            return;
        };
        let message = input.read(cx).text().trim().to_owned();
        self.send(ClientMessage::StashPush {
            repo_id: key.repo_id.clone(),
            message,
            worktree_path: key.worktree.clone(),
        });
        self.stash.model.start(&key.repo_id, StashOp::Push);
        input.update(cx, |input, cx| input.set_text("", cx));
        cx.notify();
    }

    /// A stash row's button: pop and apply are sent and mark the repo
    /// pending; drop opens the confirm. A repo with a stash write out, or a
    /// stash no longer listed, takes nothing.
    fn run_stash_action(
        &mut self,
        key: &ScKey,
        action: StashAction,
        stash: GitStash,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let repo_id = key.repo_id.clone();
        if self.stash.model.pending(&repo_id).is_some() {
            return;
        }
        if !self.stash.model.holds(&repo_id, &stash) {
            self.stash_list_changed(cx);
            return;
        }
        let worktree_path = key.worktree.clone();
        match action {
            StashAction::Pop => {
                self.send(ClientMessage::StashPop {
                    repo_id: repo_id.clone(),
                    stash_id: stash.id,
                    worktree_path,
                });
                self.stash.model.start(&repo_id, StashOp::Pop);
            }
            StashAction::Apply => {
                self.send(ClientMessage::StashApply {
                    repo_id: repo_id.clone(),
                    stash_id: stash.id,
                    worktree_path,
                });
                self.stash.model.start(&repo_id, StashOp::Apply);
            }
            StashAction::Drop => {
                if self.stash_drop_blocked() {
                    return;
                }
                self.stash.drop = Some(DropConfirm::new(key.clone(), stash));
                self.stash.drop_focus.focus(window);
            }
        }
        cx.notify();
    }

    /// A dialog, a modal notice or the connection overlay is up, so no drop
    /// confirm opens.
    fn stash_drop_blocked(&self) -> bool {
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

    /// The open drop confirm, as text.
    #[must_use]
    pub fn stash_drop_confirm(&self) -> Option<StashDropView> {
        let confirm = self.stash.drop.as_ref()?;
        Some(StashDropView {
            title: confirm.title(),
            body: confirm.body(),
            focused: confirm.focused().selector(),
        })
    }

    /// Closes the confirm; the danger button first sends the drop and marks
    /// the repo pending.
    fn press_drop_button(
        &mut self,
        button: DropButton,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(confirm) = self.stash.drop.take() else {
            return;
        };
        let key = confirm.tree();
        if button == DropButton::Drop && !self.stash.model.holds(&key.repo_id, confirm.stash()) {
            self.stash_list_changed(cx);
        } else if button == DropButton::Drop {
            self.send(ClientMessage::StashDrop {
                repo_id: key.repo_id.clone(),
                stash_id: confirm.stash().id.clone(),
                worktree_path: key.worktree.clone(),
            });
            self.stash.model.start(&key.repo_id, StashOp::Drop);
        }
        self.after_notice_closed(window, cx);
    }

    /// Tells the user a stash action was not sent because the stash it
    /// named is no longer where it was in the list.
    fn stash_list_changed(&mut self, cx: &mut Context<Self>) {
        self.push_toast(ToastKind::Info, STASH_LIST_CHANGED, None, cx);
    }

    /// Cancels the drop confirm, if open, as when the connection goes.
    pub(crate) fn close_stash_drop_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stash.drop.is_some() {
            self.press_drop_button(DropButton::Cancel, window, cx);
        }
    }

    /// A key while the drop confirm is open; returns whether it was.
    pub(crate) fn on_stash_drop_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(confirm) = &mut self.stash.drop else {
            return false;
        };
        match confirm.key(keystroke.key.as_str()) {
            Some(button) => self.press_drop_button(button, window, cx),
            None => cx.notify(),
        }
        true
    }

    /// The drop confirm over a backdrop that takes every click beneath it.
    pub(crate) fn stash_drop_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let confirm = self.stash.drop.as_ref()?;
        let buttons: Vec<AnyElement> = DropButton::ALL
            .into_iter()
            .map(|button| {
                let (label, danger) = match button {
                    DropButton::Cancel => ("Cancel", false),
                    DropButton::Drop => ("Drop stash", true),
                };
                dialog_button(
                    button.selector(),
                    label.to_owned(),
                    danger,
                    confirm.focused() == button,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.press_drop_button(button, window, cx);
                }))
                .into_any_element()
            })
            .collect();
        let close = dialog_button("stash-drop-confirm-close", "✕".to_owned(), false, false)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.press_drop_button(DropButton::Cancel, window, cx);
            }));
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(confirm.title()),
            )
            .child(close);
        let panel = modal_panel("stash-drop-confirm-panel")
            .track_focus(&self.stash.drop_focus)
            .child(header)
            .child(div().text_color(gpui::rgb(MUTED)).child(confirm.body()))
            .child(div().flex().justify_end().gap(px(6.0)).children(buttons));
        Some(backdrop("stash-drop-confirm-dialog", panel))
    }
}

/// A section's Stashes elements: the header, then, while expanded, the push
/// row and the text line or the stash rows. Nothing when the section shows
/// no Stashes part.
pub(crate) fn stashes_part(
    row: &ScSectionRow,
    input: Option<Entity<TextInput>>,
    cx: &mut Context<RootView>,
) -> Vec<AnyElement> {
    let Some(part) = &row.stashes else {
        return Vec::new();
    };
    let key = &row.key;
    let mut out = vec![stashes_header(key, part, cx).into_any_element()];
    if let Some(push) = &part.push {
        out.push(push_row(key, push, input, cx).into_any_element());
    }
    if let Some(text) = &part.body {
        out.push(
            div()
                .pl(px(ROW_PADDING * 2.0))
                .pr(px(ROW_PADDING))
                .text_color(gpui::rgb(MUTED))
                .child(text.clone())
                .into_any_element(),
        );
    }
    for stash in &part.rows {
        out.push(stash_row(key, stash, cx).into_any_element());
    }
    out
}

fn stashes_header(key: &ScKey, part: &ScStashes, cx: &mut Context<RootView>) -> Stateful<Div> {
    let name = part.selector.clone();
    let key = key.clone();
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .items_center()
        .gap(px(4.0))
        .h(px(ROW_HEIGHT))
        .px(px(ROW_PADDING))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .child(caret(part.collapsed))
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::SEMIBOLD)
                .child(STASHES_TITLE),
        )
        .when_some(part.count, |header, count| {
            header.child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(MUTED))
                    .child(count.to_string()),
            )
        })
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.toggle_sc_stashes(&key, cx);
        }))
}

fn push_row(
    key: &ScKey,
    push: &ScStashPush,
    input: Option<Entity<TextInput>>,
    cx: &mut Context<RootView>,
) -> Div {
    let input_name = push.input_selector.clone();
    let field = div()
        .debug_selector(|| input_name)
        .flex_1()
        .min_w(px(0.0))
        .px(px(6.0))
        .py(px(2.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .when(!push.enabled, |field| field.opacity(0.5))
        .children(input);
    let name = push.selector.clone();
    let button = div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex_none()
        .px(px(6.0))
        .rounded(px(3.0))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .child(push.label);
    let button = if push.enabled {
        let key = key.clone();
        button
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.push_stash(&key, cx);
            }))
    } else {
        button.opacity(0.5)
    };
    div()
        .flex()
        .items_center()
        .gap(px(4.0))
        .pl(px(ROW_PADDING * 2.0))
        .pr(px(ROW_PADDING))
        .py(px(2.0))
        .child(field)
        .child(button)
}

fn stash_row(key: &ScKey, stash: &ScStashRow, cx: &mut Context<RootView>) -> Stateful<Div> {
    let name = stash.selector.clone();
    let shown = GitStash {
        id: stash.id.clone(),
        subject: stash.subject.clone(),
        created_at: stash.created_at.clone(),
    };
    let buttons: Vec<Stateful<Div>> = stash
        .buttons
        .iter()
        .map(|button| stash_button(key, &shown, button, cx))
        .collect();
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .items_center()
        .gap(px(4.0))
        .h(px(ROW_HEIGHT))
        .pl(px(ROW_PADDING * 2.0))
        .pr(px(ROW_PADDING))
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .tooltip(tooltip(stash.tooltip.clone()))
        .child(
            div()
                .flex_none()
                .text_color(gpui::rgb(MUTED))
                .child(stash.id.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .child(stash.subject.clone()),
        )
        .child(div().flex().flex_none().gap(px(2.0)).children(buttons))
}

/// A stash row's text button, acting on `stash` as the row showed it. A
/// disabled one is dimmed and takes no click.
fn stash_button(
    key: &ScKey,
    stash: &GitStash,
    button: &StashButton,
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
        .child(button.label);
    if !button.enabled {
        return base.opacity(0.5);
    }
    let key = key.clone();
    let stash = stash.clone();
    let action = button.action;
    base.cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            cx.stop_propagation();
            this.run_stash_action(&key, action, stash.clone(), window, cx);
        }))
}
