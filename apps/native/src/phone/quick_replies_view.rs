//! Draws Settings' Quick replies page ([`super::quick_replies`]' data): each
//! set's groups as rows that open a group's page, that page's Title row over
//! its replies, and the title and reply editors their rows open. A set's
//! Restore defaults asks before it brings the built-in groups back.

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, Focusable, SharedString, Stateful,
    Subscription, Window, div, prelude::*, px,
};

use super::PhoneRoot;
use super::home_view::{SIDE_PADDING, TOUCH_TARGET};
use super::hosts_view::{field_box, phone_button};
use super::quick_replies::{self, ListError, ReplyEdit, ReplyGroup, ReplySet};
use super::reorder::{self, ListId, dragged_row, slot_of};
use super::settings::{Page, Settings, SubPage};
use super::settings_view::{CHEVRON, section_label};
use crate::buttons::{ButtonSize, Layer, outlined_button};
use crate::driver_id::DriverId as _;
use crate::fonts::ui_px;
use crate::text_input::{TextInput, TextInputEvent};
use crate::ui_theme::UiTheme;

/// The heading over a group page's replies.
const REPLIES_LABEL: &str = "Replies";
/// The row that opens a new group's editor.
const ADD_GROUP_LABEL: &str = "Add group";
/// The row that opens a new reply's editor.
const ADD_REPLY_LABEL: &str = "Add reply";
/// A group's or a reply's Remove.
const REMOVE_LABEL: &str = "Remove";
/// The button that brings a set's built-in groups back.
const RESTORE_LABEL: &str = "Restore";
/// What a set's Restore row says before it asks.
const RESTORE_ROW_LABEL: &str = "Restore defaults";
/// The question a set's Restore row turns into.
const RESTORE_ASK: &str = "Replace these with the defaults?";
/// The confirm row's way out.
const CANCEL_LABEL: &str = "Cancel";
/// The editors' Save.
const SAVE_LABEL: &str = "Save";
/// What an editor says when the item it edits is gone.
const STALE_ITEM: &str = "That item no longer exists.";
/// A new group's title field says what to type.
const TITLE_PLACEHOLDER: &str = "Group title";
/// A new reply's field says what to type.
const REPLY_PLACEHOLDER: &str = "Reply text";

/// The two sets, in the order the page draws them.
const SETS: [ReplySet; 2] = [ReplySet::Agent, ReplySet::Shell];

/// One reply's place in its set, as its row draws and edits it.
#[derive(Clone, Copy)]
struct ReplyRow<'a> {
    set: ReplySet,
    group: usize,
    text: &'a str,
    index: usize,
}

/// The set's name over its groups.
fn set_label(set: ReplySet) -> &'static str {
    match set {
        ReplySet::Agent => "Agent sessions",
        ReplySet::Shell => "Shells",
    }
}

/// The tail of the set's driver ids.
fn set_key(set: ReplySet) -> &'static str {
    match set {
        ReplySet::Agent => "agent",
        ReplySet::Shell => "shell",
    }
}

/// How many replies a group holds, as its row's count reads.
fn reply_count(n: usize) -> String {
    if n == 1 {
        "1 reply".to_owned()
    } else {
        format!("{n} replies")
    }
}

/// A title or reply editor: the field, the sub-page it saves and its refusal.
pub(super) struct TextEditor {
    /// The editor sub-page this field belongs to.
    sub: SubPage,
    input: Entity<TextInput>,
    error: Option<String>,
    _keys: Subscription,
}

/// Whether `sub` still names an item of the set it shows, given each group's
/// reply count per set: a group past the set's groups is gone, and so is a
/// reply whose group or own index is gone.
pub(super) fn sub_page_stands(sub: SubPage, agent: &[usize], shell: &[usize]) -> bool {
    let counts = |set: ReplySet| match set {
        ReplySet::Agent => agent,
        ReplySet::Shell => shell,
    };
    match sub {
        SubPage::ReplyGroup { set, group } => group < counts(set).len(),
        SubPage::GroupTitle { set, group } => group.is_none_or(|group| group < counts(set).len()),
        SubPage::Reply { set, group, reply } => {
            let counts = counts(set);
            reply.map_or_else(
                || group < counts.len(),
                |reply| counts.get(group).is_some_and(|replies| reply < *replies),
            )
        }
        SubPage::KeyEditor { .. } | SubPage::KeyPicker { .. } => false,
    }
}

/// A button tagged `selector`, drawn dimmed and taking no tap while `enabled`
/// is false.
fn dimmable_button(
    selector: &str,
    enabled: bool,
    label: &'static str,
    t: &UiTheme,
    cx: &Context<PhoneRoot>,
) -> Stateful<Div> {
    outlined_button(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        selector,
        ButtonSize::Large,
        enabled,
        Layer::Base,
        t,
    )
    .min_h(px(TOUCH_TARGET))
    .child(label)
}

impl PhoneRoot {
    /// Opens Settings on its Quick replies page, as the shot example does.
    pub fn open_settings_replies(&mut self, cx: &mut Context<Self>) {
        self.open_settings_on(Page::QuickReplies, cx);
    }

    /// Opens Settings on `set`'s group `group`, as a row's tap and the shot
    /// example do.
    pub fn open_settings_reply_group(
        &mut self,
        set: ReplySet,
        group: usize,
        cx: &mut Context<Self>,
    ) {
        self.forget_pending_open();
        self.settings_scroll_on_open();
        let mut settings = Settings::default();
        settings.open(Page::QuickReplies);
        settings.push(SubPage::ReplyGroup { set, group });
        self.settings = Some(settings);
        self.keep_restore_confirm();
        cx.notify();
    }

    /// Keeps the Restore confirm, and a drag that can still draw, only while
    /// the Quick replies page tops Settings: leaving the page, opening
    /// anything over it or closing Settings drops both.
    pub(super) fn keep_restore_confirm(&mut self) {
        let on_page = self.settings.as_ref().is_some_and(|settings| {
            settings.top().is_none() && settings.page == Some(Page::QuickReplies)
        });
        if !on_page {
            self.restore_confirm = None;
            self.reorder_drag = None;
        }
    }

    /// Drops the editor once its sub-page left the stack or Settings closed.
    pub(super) fn keep_reply_editor(&mut self) {
        let Some(editor) = self.reply_editor.as_ref() else {
            return;
        };
        let open = self
            .settings
            .as_ref()
            .is_some_and(|settings| settings.stack.contains(&editor.sub));
        if !open {
            self.reply_editor = None;
        }
    }

    /// How many replies each group of `set` holds, as the views draw it.
    pub(super) fn reply_counts(&self, set: ReplySet, cx: &App) -> Vec<usize> {
        self.saved_groups(set, cx)
            .iter()
            .map(|group| group.replies.len())
            .collect()
    }

    /// Drops the reply sub-pages whose item is gone, and the editor of one
    /// that left.
    pub(super) fn retain_reply_sub_pages(&mut self, cx: &App) {
        let agent = self.reply_counts(ReplySet::Agent, cx);
        let shell = self.reply_counts(ReplySet::Shell, cx);
        if let Some(settings) = self.settings.as_mut() {
            settings.retain_valid(|sub| sub_page_stands(sub, &agent, &shell));
        }
        self.keep_reply_editor();
    }

    /// The set's groups as the page draws them: the saved ones, else the
    /// built-in defaults.
    pub(super) fn saved_groups(&self, set: ReplySet, cx: &App) -> Vec<ReplyGroup> {
        let saved = self.saved_replies(set, cx);
        quick_replies::groups(saved.as_deref(), set)
    }

    /// Opens the editor of `set`'s group `group`, or of a new group while
    /// `group` is `None`.
    fn open_group_title_editor(
        &mut self,
        set: ReplySet,
        group: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let groups = self.saved_groups(set, cx);
        let title = group
            .and_then(|group| groups.get(group))
            .map(|group| group.title.clone())
            .unwrap_or_default();
        let sub = SubPage::GroupTitle { set, group };
        self.reply_editor = Some(Self::make_editor(sub, title, TITLE_PLACEHOLDER, window, cx));
        self.push_editor(sub, cx);
    }

    /// Opens the editor of `set`'s `group`'s reply `reply`, or of a new reply
    /// while `reply` is `None`.
    fn open_reply_editor(
        &mut self,
        set: ReplySet,
        group: usize,
        reply: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let groups = self.saved_groups(set, cx);
        let text = reply
            .and_then(|reply| groups.get(group)?.replies.get(reply).cloned())
            .unwrap_or_default();
        let sub = SubPage::Reply { set, group, reply };
        self.reply_editor = Some(Self::make_editor(sub, text, REPLY_PLACEHOLDER, window, cx));
        self.push_editor(sub, cx);
    }

    /// The editor over `sub`: its field holding `text` and focused, which
    /// saves on Enter.
    fn make_editor(
        sub: SubPage,
        text: String,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TextEditor {
        let input = cx.new(|cx| TextInput::new(text, placeholder, cx));
        input.read(cx).focus_handle(cx).focus(window, cx);
        let keys = cx.subscribe(&input, |this, _, event: &TextInputEvent, cx| {
            if *event == TextInputEvent::Submit {
                this.save_reply_editor(cx);
            }
        });
        TextEditor {
            sub,
            input,
            error: None,
            _keys: keys,
        }
    }

    /// Opens `sub` over the page.
    fn push_editor(&mut self, sub: SubPage, cx: &mut Context<Self>) {
        if let Some(settings) = self.settings.as_mut() {
            settings.push(sub);
        }
        self.keep_restore_confirm();
        cx.notify();
    }

    /// Saves the open editor's field: a refused title or reply keeps the
    /// editor open with its message, a saved one pops it.
    fn save_reply_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.reply_editor.as_ref() else {
            return;
        };
        let sub = editor.sub;
        let text = editor.input.read(cx).text().to_owned();
        let set = match sub {
            SubPage::GroupTitle { set, .. } | SubPage::Reply { set, .. } => set,
            SubPage::ReplyGroup { .. } | SubPage::KeyEditor { .. } | SubPage::KeyPicker { .. } => {
                return;
            }
        };
        let groups = self.saved_groups(set, cx);
        let Some(outcome) = Self::checked_edit(sub, &groups, &text) else {
            return;
        };
        match outcome {
            Err(error) => self.set_editor_error(&error.message(), cx),
            Ok(edit) => {
                if self.write_replies(set, edit, cx) {
                    self.close_saved_editor(cx);
                } else {
                    self.set_editor_error(STALE_ITEM, cx);
                }
            }
        }
    }

    /// Pops the editor a save just came from, dropping its draft.
    fn close_saved_editor(&mut self, cx: &mut Context<Self>) {
        if let Some(settings) = self.settings.as_mut() {
            settings.back();
        }
        self.reply_editor = None;
        self.retain_reply_sub_pages(cx);
        cx.notify();
    }

    /// Says what stopped the open editor from saving, keeping it open with
    /// its typed text.
    fn set_editor_error(&mut self, message: &str, cx: &mut Context<Self>) {
        if let Some(editor) = self.reply_editor.as_mut() {
            editor.error = Some(message.to_owned());
        }
        cx.notify();
    }

    /// The edit an editor sub-page makes of `typed`, or why it cannot.
    fn checked_edit(
        sub: SubPage,
        groups: &[ReplyGroup],
        typed: &str,
    ) -> Option<Result<ReplyEdit, ListError>> {
        match sub {
            SubPage::GroupTitle { group, .. } => Some(
                quick_replies::check_title(groups, typed, group).map(|title| match group {
                    Some(group) => ReplyEdit::RenameGroup { group, title },
                    None => ReplyEdit::AddGroup { title },
                }),
            ),
            SubPage::Reply { group, reply, .. } => {
                Some(quick_replies::check_reply(typed).map(|text| match reply {
                    Some(reply) => ReplyEdit::SetReply { group, reply, text },
                    None => ReplyEdit::AddReply { group, text },
                }))
            }
            SubPage::ReplyGroup { .. } | SubPage::KeyEditor { .. } | SubPage::KeyPicker { .. } => {
                None
            }
        }
    }

    /// Writes `set`'s groups with `edit` made, the whole set each time;
    /// false, writing nothing, while an index names nothing.
    fn write_replies(&mut self, set: ReplySet, edit: ReplyEdit, cx: &mut Context<Self>) -> bool {
        let groups = self.saved_groups(set, cx);
        let Some(next) = quick_replies::apply(&groups, edit) else {
            return false;
        };
        self.root
            .update(cx, |root, cx| root.set_phone_replies(set, Some(next), cx));
        true
    }

    /// Drops `set`'s group `group`.
    fn remove_group(&mut self, set: ReplySet, group: usize, cx: &mut Context<Self>) {
        self.write_replies(set, ReplyEdit::RemoveGroup { group }, cx);
        self.retain_reply_sub_pages(cx);
        cx.notify();
    }

    /// Drops `set`'s `group`'s reply `reply`.
    fn remove_reply(&mut self, set: ReplySet, group: usize, reply: usize, cx: &mut Context<Self>) {
        self.write_replies(set, ReplyEdit::RemoveReply { group, reply }, cx);
        self.retain_reply_sub_pages(cx);
        cx.notify();
    }

    /// A tap on a set's Restore defaults: the confirm row asks.
    fn ask_restore(&mut self, set: ReplySet, cx: &mut Context<Self>) {
        self.restore_confirm = Some(set);
        cx.notify();
    }

    /// A tap on the confirm row's Cancel: nothing changes.
    fn cancel_restore(&mut self, cx: &mut Context<Self>) {
        self.restore_confirm = None;
        cx.notify();
    }

    /// A tap on the confirm row's Restore: the set goes back to its built-in
    /// groups.
    fn restore_defaults(&mut self, set: ReplySet, cx: &mut Context<Self>) {
        self.restore_confirm = None;
        self.root
            .update(cx, |root, cx| root.set_phone_replies(set, None, cx));
        self.retain_reply_sub_pages(cx);
        cx.notify();
    }

    /// The Quick replies page: each set's groups under its name, then its
    /// Add group and its Restore defaults, with the page's drag area over
    /// the lists.
    pub(super) fn quick_replies_page(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        self.forget_list_boxes();
        div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(SIDE_PADDING))
            .children(SETS.map(|set| self.reply_set_section(set, t, cx)))
            .child(reorder::reorder_area(&self.list_boxes, cx))
    }

    /// One set: its name, a row per group, Add group and Restore defaults.
    fn reply_set_section(&self, set: ReplySet, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let groups = self.saved_groups(set, cx);
        let drag = self
            .reorder_drag
            .filter(|drag| drag.list == ListId::ReplyGroups(set));
        let rows: Vec<AnyElement> = groups
            .iter()
            .enumerate()
            .flat_map(|(index, group)| {
                dragged_row(
                    Self::group_row(set, group, index, t, cx),
                    drag.is_some_and(|drag| drag.from == index),
                    slot_of(drag, index),
                    t,
                )
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .driver_id(|| format!("phone-replies-{}", set_key(set)))
            .child(section_label(set_label(set), t, cx))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .driver_id(|| format!("phone-replies-{}-groups", set_key(set)))
                    .children(rows)
                    .child(reorder::list_box_canvas(
                        &self.list_boxes,
                        ListId::ReplyGroups(set),
                        groups.len(),
                    )),
            )
            .child(Self::add_group_row(set, cx))
            .child(self.restore_row(set, t, cx))
    }

    /// The row that opens a new group's editor.
    fn add_group_row(set: ReplySet, cx: &mut Context<Self>) -> Stateful<Div> {
        phone_button(
            &format!("phone-replies-{}-add-group", set_key(set)),
            crate::buttons::ButtonKind::Outlined,
            cx,
        )
        .min_h(px(TOUCH_TARGET))
        .child(ADD_GROUP_LABEL)
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.open_group_title_editor(set, None, window, cx);
        }))
    }

    /// A set's Restore defaults: the button, or the row that asks first. It
    /// is dimmed and takes no tap while the built-in groups already show.
    fn restore_row(&self, set: ReplySet, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        if self.restore_confirm == Some(set) {
            return Self::restore_confirm_row(set, t, cx);
        }
        let saved = self.saved_replies(set, cx).is_some();
        let selector = format!("phone-replies-{}-restore", set_key(set));
        let button = dimmable_button(&selector, saved, RESTORE_ROW_LABEL, t, cx);
        let button = if saved {
            button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.ask_restore(set, cx);
            }))
        } else {
            button
        };
        div().flex().items_center().child(button)
    }

    /// The question a set's Restore row turns into, with its Restore and
    /// Cancel.
    fn restore_confirm_row(set: ReplySet, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let key = set_key(set);
        let confirm = dimmable_button(
            &format!("phone-replies-{key}-restore-confirm"),
            true,
            RESTORE_LABEL,
            t,
            cx,
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.restore_defaults(set, cx);
        }));
        let cancel = dimmable_button(
            &format!("phone-replies-{key}-restore-cancel"),
            true,
            CANCEL_LABEL,
            t,
            cx,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel_restore(cx)));
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(div().flex_1().min_w_0().child(RESTORE_ASK))
            .child(confirm)
            .child(cancel)
    }

    /// One group's row: its title over its reply count, a Remove, a chevron,
    /// a touch target tall; a tap opens the group's page.
    fn group_row(
        set: ReplySet,
        group: &ReplyGroup,
        index: usize,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = format!("phone-replies-{}-group-{index}", set_key(set));
        let remove = format!("{id}-remove");
        div()
            .id(SharedString::from(id.clone()))
            .driver_id(|| id)
            .flex()
            .items_center()
            .gap(px(10.0))
            .min_h(px(TOUCH_TARGET))
            .px(px(SIDE_PADDING))
            .py(px(6.0))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .truncate()
                            .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                            .child(group.title.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                            .text_color(gpui::rgb(t.muted))
                            .child(reply_count(group.replies.len())),
                    ),
            )
            .child(
                phone_button(&remove, crate::buttons::ButtonKind::Outlined, cx)
                    .min_h(px(TOUCH_TARGET))
                    .child(REMOVE_LABEL)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.remove_group(set, index, cx);
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(t.muted))
                    .child(CHEVRON),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.open_settings_reply_group(set, index, cx);
            }))
    }

    /// One group's page: its Title row over its replies, then Add reply,
    /// with the page's drag area over the replies.
    pub(super) fn reply_group_page(
        &self,
        set: ReplySet,
        group: usize,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Div {
        self.forget_list_boxes();
        let groups = self.saved_groups(set, cx);
        let Some(entry) = groups.get(group) else {
            return div();
        };
        let drag = self
            .reorder_drag
            .filter(|drag| drag.list == ListId::Replies);
        let rows: Vec<AnyElement> = entry
            .replies
            .iter()
            .enumerate()
            .flat_map(|(index, text)| {
                dragged_row(
                    Self::reply_row(
                        &ReplyRow {
                            set,
                            group,
                            text,
                            index,
                        },
                        t,
                        cx,
                    ),
                    drag.is_some_and(|drag| drag.from == index),
                    slot_of(drag, index),
                    t,
                )
            })
            .collect();
        div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(SIDE_PADDING))
            .child(Self::group_title_row(set, entry, group, t, cx))
            .child(section_label(REPLIES_LABEL, t, cx))
            .child(
                div()
                    .relative()
                    .driver_id(|| "phone-replies-replies".to_owned())
                    .flex()
                    .flex_col()
                    .children(rows)
                    .child(reorder::list_box_canvas(
                        &self.list_boxes,
                        ListId::Replies,
                        entry.replies.len(),
                    )),
            )
            .child(
                phone_button(
                    "phone-replies-add-reply",
                    crate::buttons::ButtonKind::Outlined,
                    cx,
                )
                .min_h(px(TOUCH_TARGET))
                .child(ADD_REPLY_LABEL)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.open_reply_editor(set, group, None, window, cx);
                })),
            )
            .child(reorder::reorder_area(&self.list_boxes, cx))
    }

    /// The group page's Title row: the group's title and a chevron; a tap
    /// opens its editor.
    fn group_title_row(
        set: ReplySet,
        group: &ReplyGroup,
        index: usize,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(SharedString::from("phone-replies-group-title"))
            .driver_id(|| "phone-replies-group-title".to_owned())
            .flex()
            .items_center()
            .gap(px(10.0))
            .min_h(px(TOUCH_TARGET))
            .py(px(6.0))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                    .child(group.title.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(t.muted))
                    .child(CHEVRON),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_group_title_editor(set, Some(index), window, cx);
            }))
    }

    /// One reply's row: its text, truncated, a Remove and a chevron; a tap
    /// opens its editor.
    fn reply_row(row: &ReplyRow<'_>, t: &UiTheme, cx: &mut Context<Self>) -> Stateful<Div> {
        let ReplyRow {
            set,
            group,
            text,
            index,
        } = *row;
        let id = format!("phone-replies-reply-{index}");
        let remove = format!("{id}-remove");
        div()
            .id(SharedString::from(id.clone()))
            .driver_id(|| id)
            .flex()
            .items_center()
            .gap(px(10.0))
            .min_h(px(TOUCH_TARGET))
            .py(px(6.0))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                    .child(text.to_owned()),
            )
            .child(
                phone_button(&remove, crate::buttons::ButtonKind::Outlined, cx)
                    .min_h(px(TOUCH_TARGET))
                    .child(REMOVE_LABEL)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.remove_reply(set, group, index, cx);
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(t.muted))
                    .child(CHEVRON),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_reply_editor(set, group, Some(index), window, cx);
            }))
    }

    /// The open title or reply editor: its field, its refusal and Save, at
    /// the top of its sub-page.
    pub(super) fn reply_editor_page(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let Some(editor) = self.reply_editor.as_ref() else {
            return div();
        };
        let page = div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(SIDE_PADDING))
            .child(field_box("phone-edit-field", &editor.input, t, cx));
        let page = match editor.error.as_ref() {
            Some(error) => page.child(
                div()
                    .driver_id(|| "phone-edit-error".to_owned())
                    .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                    .text_color(gpui::rgb(t.danger_text))
                    .child(error.clone()),
            ),
            None => page,
        };
        page.child(
            dimmable_button("phone-edit-save", true, SAVE_LABEL, t, cx)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_reply_editor(cx))),
        )
    }
}
