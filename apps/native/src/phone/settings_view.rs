//! Draws the phone's Settings ([`super::settings`]): the header row over
//! Home and the hosts screen with its `Settings` button, and, while
//! Settings is open, its own header (Back and the title) over the category
//! list, a category's page or the sub-page opened over that page. It only
//! renders and forwards taps.

use gpui::{
    App, BackGestureEvent, ClickEvent, Context, Div, FontWeight, SharedString, Stateful, Window,
    div, prelude::*, px,
};

use super::PhoneRoot;
use super::home_view::{SIDE_PADDING, TOUCH_TARGET};
use super::hosts_view::phone_button;
use super::keys_view::key_sub_page_stands;
use super::quick_replies::ReplySet;
use super::quick_replies_view::sub_page_stands;
use super::session;
use super::settings::{BackStep, Page, Settings, SubPage};
use crate::buttons::{ButtonKind, ButtonSize, Layer, outlined_button};
use crate::driver_id::DriverId as _;
use crate::fonts::ui_px;
use crate::theme_store::ThemeEntry;
use crate::ui_theme::{UiTheme, theme};

/// The header's button that opens Settings.
const OPEN_LABEL: &str = "Settings";
/// Settings' Back button.
pub(super) const BACK_LABEL: &str = "Back";
/// What the Notifications page says while no host draws a switch.
const NO_NOTIFICATIONS_TEXT: &str = "No host offers notifications yet.";
/// The mark at a category row's right end.
pub(super) const CHEVRON: &str = "›";
/// The Appearance page's heading over the theme list.
const THEMES_LABEL: &str = "Theme";
/// The Appearance page's heading over the text-size row.
const TEXT_SIZE_LABEL: &str = "Text size";
/// The mark beside the theme the phone draws in.
pub(super) const CHECK: &str = "✓";
/// The stepper's button one size smaller.
const DOWN_LABEL: &str = "−";
/// The stepper's button one size larger.
const UP_LABEL: &str = "+";
/// The Storage page's row title.
const FETCHED_FILES_LABEL: &str = "Fetched files";
/// The Storage page's line on how long copies are kept.
const KEPT_UNTIL_TEXT: &str = "Kept until 30 days after you last open one";
/// The Storage page's button that deletes every kept copy.
const CLEAR_LABEL: &str = "Clear";

/// A header row's frame, the session page's: at least a touch target tall,
/// the side padding, a line under it.
pub(super) fn header_frame(t: &UiTheme) -> Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(12.0))
        .min_h(px(TOUCH_TARGET))
        .px(px(SIDE_PADDING))
        .py(px(6.0))
        .border_b_1()
        .border_color(gpui::rgb(t.line))
}

/// A heading over the Appearance page's two sections.
pub(super) fn section_label(label: &str, t: &UiTheme, cx: &Context<PhoneRoot>) -> Div {
    div()
        .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
        .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
        .text_color(gpui::rgb(t.muted))
        .child(label.to_owned())
}

impl PhoneRoot {
    /// Opens Settings on its category list, as the header's button does.
    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        tracing::debug!("phone: opening Settings");
        self.forget_pending_open();
        self.storage_error = None;
        self.settings = Some(Settings::default());
        self.keep_reply_editor();
        self.keep_key_editor();
        self.keep_restore_confirm();
        self.keep_keys_restore_confirm();
        cx.notify();
    }

    /// Opens `page` over Settings' list, as its row's tap does.
    fn open_settings_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.forget_pending_open();
        self.storage_error = None;
        if let Some(settings) = self.settings.as_mut() {
            settings.open(page);
        }
        self.keep_reply_editor();
        self.keep_key_editor();
        self.keep_restore_confirm();
        self.keep_keys_restore_confirm();
        cx.notify();
    }

    /// Opens Settings on `page`, as the phone does by itself when the hosts
    /// screen's add form would otherwise leave the screen.
    pub(super) fn open_settings_on(&mut self, page: Page, cx: &mut Context<Self>) {
        let mut settings = Settings::default();
        settings.open(page);
        self.settings = Some(settings);
        self.storage_error = None;
        self.keep_reply_editor();
        self.keep_key_editor();
        self.keep_restore_confirm();
        self.keep_keys_restore_confirm();
        cx.notify();
    }

    /// Opens Settings on its Appearance page, as the shot example does.
    pub fn open_settings_appearance(&mut self, cx: &mut Context<Self>) {
        self.open_settings_on(Page::Appearance, cx);
    }

    /// One Back step in Settings: the innermost sub-page pops, then a page
    /// goes back to the list, then the list closes Settings. A sub-page whose
    /// item is gone closes with the step.
    pub(super) fn settings_back(&mut self, cx: &mut Context<Self>) {
        self.forget_pending_open();
        let step = self.settings.as_mut().map(Settings::back);
        if step == Some(BackStep::ToList) {
            self.storage_error = None;
        }
        self.retain_sub_pages(cx);
        if step == Some(BackStep::Close) {
            self.settings = None;
        }
        self.keep_restore_confirm();
        self.keep_keys_restore_confirm();
        cx.notify();
    }

    /// Drops the sub-pages whose item is gone, the editors of ones that left,
    /// and the picker of a press that left.
    pub(super) fn retain_sub_pages(&mut self, cx: &App) {
        let agent = self.reply_counts(ReplySet::Agent, cx);
        let shell = self.reply_counts(ReplySet::Shell, cx);
        let keys = self.key_count(cx);
        let steps = self
            .key_editor
            .as_ref()
            .map(super::keys_view::KeyEditor::step_count);
        if let Some(settings) = self.settings.as_mut() {
            settings.retain_valid(|sub| match sub {
                SubPage::KeyEditor { .. } | SubPage::KeyPicker { .. } => {
                    key_sub_page_stands(sub, keys, steps)
                }
                _ => sub_page_stands(sub, &agent, &shell),
            });
        }
        self.keep_reply_editor();
        self.keep_key_editor();
    }

    /// A back gesture over the window while the viewer, a session or
    /// Settings shows: one step of the screen shown once it commits.
    pub(super) fn screen_back_gesture(
        &mut self,
        event: &BackGestureEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !session::back_on(event.phase) {
            return;
        }
        // Settings is the page on top in either layout, over whatever two
        // panes left open beside or under it.
        if self.settings.is_some() {
            self.settings_back(cx);
        } else if self.viewer.is_some() {
            self.viewer_back(cx);
        } else if self.shown.is_some() {
            self.session_back(cx);
        }
    }

    /// The header over Home and the hosts screen: the `Settings` button at
    /// its right end, and over Home (`sort`) the sort button before it.
    pub(super) fn home_header_row(sort: bool, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let open = phone_button("phone-settings-open", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_none()
            .child(OPEN_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.open_settings(cx)));
        header_frame(t)
            .justify_end()
            .when(sort, |header| header.child(Self::sort_button(cx)))
            .child(open)
    }

    /// While Settings is open: its header over the category list, the page it
    /// is on, or the sub-page opened over that page.
    pub(super) fn settings_page(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<Div> {
        let settings = self.settings.as_ref()?;
        let t = theme(cx);
        let body = match settings.top() {
            Some(SubPage::ReplyGroup { set, group }) => self.reply_group_page(set, group, &t, cx),
            Some(SubPage::GroupTitle { .. } | SubPage::Reply { .. }) => {
                self.reply_editor_page(&t, cx)
            }
            Some(SubPage::KeyEditor { .. } | SubPage::KeyPicker { .. }) => {
                self.key_editor_page(&t, cx)
            }
            None => match settings.page {
                None => Self::category_list(&t, cx),
                Some(Page::Notifications) => self.notifications_page(&t, cx),
                Some(Page::Hosts) => self.hosts_content(&t, cx),
                Some(Page::Appearance) => self.appearance_page(&t, cx),
                Some(Page::QuickReplies) => self.quick_replies_page(&t, cx),
                Some(Page::Keys) => self.keys_page(window, &t, cx),
                Some(Page::Storage) => self.storage_page(&t, cx),
            },
        };
        Some(
            div()
                .driver_id(|| "phone-settings".to_owned())
                .size_full()
                .flex()
                .flex_col()
                .child(Self::settings_header_row(settings.title(), &t, cx))
                .child(
                    div()
                        .id("phone-settings-body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(body),
                ),
        )
    }

    /// Settings' header: Back, then the title.
    fn settings_header_row(title: &'static str, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let back = phone_button("phone-settings-back", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_none()
            .child(BACK_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.settings_back(cx)));
        header_frame(t).child(back).child(
            div()
                .driver_id(|| "phone-settings-title".to_owned())
                .flex_1()
                .min_w_0()
                .truncate()
                .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                .child(title),
        )
    }

    /// The category list: one row per category, in [`Page::ALL`]'s order.
    fn category_list(t: &UiTheme, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .children(Page::ALL.map(|page| Self::category_row(page, t, cx)))
    }

    /// `page`'s row: its name and a chevron, a touch target tall; a tap
    /// opens its page.
    fn category_row(page: Page, t: &UiTheme, cx: &mut Context<Self>) -> Stateful<Div> {
        let id = format!("phone-settings-row-{}", page.key());
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
                    .truncate()
                    .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                    .child(page.label()),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(t.muted))
                    .child(CHEVRON),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.open_settings_page(page, cx);
            }))
    }

    /// The Notifications page: each open host's switch and hint as Home's
    /// footer drew them, or a line saying no host offers one.
    fn notifications_page(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let lines = self.push_lines(t, cx);
        let empty = lines.is_empty().then(|| {
            div()
                .driver_id(|| "phone-settings-notifications-empty".to_owned())
                .text_color(gpui::rgb(t.muted))
                .child(NO_NOTIFICATIONS_TEXT)
        });
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(SIDE_PADDING))
            .children(lines)
            .children(empty)
    }

    /// The Appearance page: every theme that loads, the one the phone draws
    /// in marked, over the text-size row.
    fn appearance_page(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let entries = self.root.read(cx).theme_entries();
        let current = Self::current_theme_id(&entries, self.saved_theme_id(cx).as_deref());
        let rows: Vec<Stateful<Div>> = entries
            .into_iter()
            .filter(|entry| entry.error.is_none())
            .map(|entry| Self::theme_row(entry, &current, t, cx))
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(SIDE_PADDING))
            .child(section_label(THEMES_LABEL, t, cx))
            .children(rows)
            .child(self.text_size_section(t, cx))
    }

    /// The theme `native-ui.json` names, when it names one.
    fn saved_theme_id(&self, cx: &Context<Self>) -> Option<String> {
        self.root.read(cx).sidebar.ui_state().theme.clone()
    }

    /// The id of the theme the boot draws in, as the desktop's theme list
    /// resolves it: the saved selection when a theme that loads answers to
    /// it, else the default's.
    fn current_theme_id(entries: &[ThemeEntry], saved: Option<&str>) -> String {
        let loads = |id: &str| {
            entries
                .iter()
                .any(|entry| entry.error.is_none() && entry.id == id)
        };
        match saved {
            Some(id) if loads(id) => id.to_owned(),
            _ => crate::theme_store::DEFAULT_ID.to_owned(),
        }
    }

    /// One theme's row: the check when it is the one in use, then its name, a
    /// touch target tall; a tap switches to it.
    fn theme_row(
        entry: ThemeEntry,
        current: &str,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let chosen = entry.id == current;
        let id = format!("phone-theme-{}", entry.id);
        let mark = div()
            .flex_none()
            .w(px(18.0))
            .text_size(ui_px(cx, crate::UI_TEXT_SIZE));
        let mark = if chosen {
            mark.driver_id(|| format!("phone-theme-check-{}", entry.id))
                .child(CHECK)
        } else {
            mark
        };
        let theme_id = entry.id;
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
            .child(mark)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                    .child(entry.name),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.pick_theme(&theme_id, cx);
            }))
    }

    /// Switches to the theme `id` names and saves it; one that does not load
    /// changes nothing.
    fn pick_theme(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Err(err) = self.root.update(cx, |root, cx| root.select_theme(id, cx)) {
            tracing::warn!(theme = %id, "phone: selecting a theme failed: {err}");
            return;
        }
        cx.notify();
    }

    /// The text-size row: the size's name over `−`, the current size and
    /// `+`.
    fn text_size_section(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let offset = self.text_size_offset(cx);
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(section_label(TEXT_SIZE_LABEL, t, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(self.text_size_button(-1, t, cx))
                    .child(
                        div()
                            .driver_id(|| "phone-text-size-value".to_owned())
                            .flex_1()
                            .flex()
                            .justify_center()
                            .child(crate::appearance_view::ui_text_size_label(offset)),
                    )
                    .child(self.text_size_button(1, t, cx)),
            )
    }

    /// The step button for `delta`: it moves the size when that stays in
    /// range, and is dimmed and takes no tap at its end of the range.
    fn text_size_button(&self, delta: i8, t: &UiTheme, cx: &mut Context<Self>) -> Stateful<Div> {
        let offset = self.text_size_offset(cx);
        let next = Self::text_size_step(offset, delta);
        let (selector, label) = if delta < 0 {
            ("phone-text-size-down", DOWN_LABEL)
        } else {
            ("phone-text-size-up", UP_LABEL)
        };
        let button = outlined_button(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            selector,
            ButtonSize::Large,
            next.is_some(),
            Layer::Base,
            t,
        )
        .min_h(px(TOUCH_TARGET))
        .flex_none()
        .child(label);
        if next.is_some() {
            button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.step_text_size(delta, cx);
            }))
        } else {
            button
        }
    }

    /// The size `delta` from `current`, or `None` past an end of the range.
    fn text_size_step(current: i8, delta: i8) -> Option<i8> {
        let next = current.checked_add(delta)?;
        crate::fonts::UI_TEXT_SIZE_OFFSETS
            .contains(&next)
            .then_some(next)
    }

    /// The pixels every UI text size is offset by, as saved.
    fn text_size_offset(&self, cx: &Context<Self>) -> i8 {
        self.root.read(cx).sidebar.ui_state().ui_text_size
    }

    /// The kept copies' size as the Storage page shows it.
    #[must_use]
    pub fn kept_size_text(&self) -> String {
        crate::spawn_form::human_size(self.fetched.total_size())
    }

    /// The Storage page's error line after a Clear that left copies behind,
    /// until the page is left.
    #[must_use]
    pub fn storage_error(&self) -> Option<&str> {
        self.storage_error.as_deref()
    }

    /// The Storage page: the kept copies' size and how long they are kept,
    /// over Clear and, after a Clear that failed, why.
    fn storage_page(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let row = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .min_h(px(TOUCH_TARGET))
            .py(px(6.0))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                            .child(FETCHED_FILES_LABEL),
                    )
                    .child(
                        div()
                            .driver_id(|| "phone-settings-storage-size".to_owned())
                            .flex_none()
                            .child(self.kept_size_text()),
                    ),
            )
            .child(div().text_color(gpui::rgb(t.muted)).child(KEPT_UNTIL_TEXT));
        let clear = phone_button("phone-settings-storage-clear", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_none()
            .child(CLEAR_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.clear_fetched(cx)));
        let error = self.storage_error.clone().map(|err| {
            div()
                .driver_id(|| "phone-settings-storage-error".to_owned())
                .text_color(gpui::rgb(t.muted))
                .child(format!("Some copies could not be deleted: {err}"))
        });
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(SIDE_PADDING))
            .child(row)
            .child(div().flex().child(clear))
            .children(error)
    }

    /// Deletes every kept copy and writes the index; a copy left behind
    /// shows why under Clear.
    fn clear_fetched(&mut self, cx: &mut Context<Self>) {
        let result = match self.root.read(cx).remote_files_root() {
            Ok(root) => {
                let root = root.to_path_buf();
                self.fetched.clear(&root).map_err(|err| err.to_string())
            }
            Err(err) => Err(err.to_owned()),
        };
        self.write_fetched();
        self.storage_error = result.err();
        if let Some(err) = &self.storage_error {
            tracing::warn!(error = %err, "phone: clearing the fetched copies");
        }
        cx.notify();
    }

    /// Steps the text size one `delta`; at an end of the range nothing
    /// changes.
    fn step_text_size(&mut self, delta: i8, cx: &mut Context<Self>) {
        let offset = self.text_size_offset(cx);
        if let Some(next) = Self::text_size_step(offset, delta) {
            self.root
                .update(cx, |root, cx| root.set_ui_text_size(next, cx));
        }
    }
}
