//! Draws the phone's session page ([`super::session`]): a header with
//! Back, the session's label and status word and, at its right end, the
//! Terminal toggle, over the Chat or Terminal page's body. The page's root
//! takes the back gesture. It only renders and forwards taps.

use gpui::{
    AnyElement, App, BackGestureEvent, ClickEvent, Context, Div, FontWeight, MouseUpEvent, Pixels,
    Point, Stateful, Window, div, prelude::*, px,
};

use super::PhoneRoot;
use super::composer::{self, Dock};
use super::home::{self, Tone};
use super::home_view::{SIDE_PADDING, STATUS_TEXT_SIZE, TOUCH_TARGET, tone_color};
use super::hosts_view::phone_button;
use super::long_press::{long_press_drag_area, touch_drag_area};
use super::session::{self, BackStep, Header, Page, SessionPage};
use crate::buttons::{ButtonKind, ButtonSize, Layer, button, button_look, field_frame};
use crate::chat::ComposerLine;
use crate::chat_view::{HandleAreas, HandleEnd, chat_available, composer_line_disables};
use crate::driver_id::DriverId as _;
use crate::fonts::ui_px;
use crate::keyboard;
use crate::target::HostKey;
use crate::ui_theme::{UiTheme, theme};

/// The session page's Back button.
const BACK_LABEL: &str = "Back";

/// The composer's Send button's label.
const SEND_LABEL: &str = "Send";
/// The composer's Paste button's label.
const PASTE_LABEL: &str = "Paste";
/// The Quick dock's button's label.
const QUICK_LABEL: &str = "Quick";
/// The Keys tray's button's label.
const KEYS_LABEL: &str = "Keys";
/// The Hide keyboard button's label.
const HIDE_LABEL: &str = "Hide keyboard";
/// The room between the composer's rows, and between its buttons.
const COMPOSER_GAP: f32 = 8.0;

/// One of the composer row's buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerButton {
    /// Paste the clipboard's text into the field.
    Paste,
    /// Open the Quick replies' dock.
    Quick,
    /// Open the Keys tray's dock.
    Keys,
    /// Put the on-screen keyboard away.
    Hide,
}

impl ComposerButton {
    /// The button's driver id.
    fn id(self) -> &'static str {
        match self {
            Self::Paste => "phone-composer-paste",
            Self::Quick => "phone-composer-quick",
            Self::Keys => "phone-composer-keys",
            Self::Hide => "phone-composer-hide",
        }
    }

    /// The label drawn on the button.
    fn label(self) -> &'static str {
        match self {
            Self::Paste => PASTE_LABEL,
            Self::Quick => QUICK_LABEL,
            Self::Keys => KEYS_LABEL,
            Self::Hide => HIDE_LABEL,
        }
    }
}

/// The composer row's buttons in the order it draws them: Paste, Quick and
/// Keys, and Hide keyboard while the on-screen keyboard is up.
fn buttons_shown(keyboard_up: bool) -> Vec<ComposerButton> {
    let mut buttons = vec![
        ComposerButton::Paste,
        ComposerButton::Quick,
        ComposerButton::Keys,
    ];
    if keyboard_up {
        buttons.push(ComposerButton::Hide);
    }
    buttons
}

impl PhoneRoot {
    /// Whether `host`'s session `session_id` mirrors its chat now.
    pub(super) fn chat_mirrored(&self, host: &HostKey, session_id: &str, cx: &App) -> bool {
        let sidebar = &self.root.read(cx).sidebar;
        sidebar
            .session_on(host, session_id)
            .is_some_and(|session| chat_available(session, sidebar.agents(host)))
    }

    /// The shown session's header, while a session is shown.
    pub(super) fn session_header(&self, cx: &App) -> Option<Header> {
        let (host, session_id) = self.shown.as_ref()?;
        let ended = self
            .root
            .read(cx)
            .sidebar
            .session_on(host, session_id)
            .is_some_and(home::ended);
        let mirrored = self.chat_mirrored(host, session_id, cx);
        let home = self.home(cx);
        Some(Header::new(
            session_id,
            home.row_of(host, session_id),
            ended,
            mirrored,
        ))
    }

    /// The page the shown session draws while its chat is mirrored or not
    /// ([`SessionPage::shown`]).
    pub(super) fn shown_page(&self, chat_mirrored: bool) -> Page {
        self.page
            .unwrap_or_else(|| SessionPage::opened(chat_mirrored))
            .shown(chat_mirrored)
    }

    /// Shows the session Home lists as `session_id`, as a tap on its row
    /// does; returns whether Home lists it.
    pub fn open_session_on(&mut self, session_id: &str, cx: &mut Context<Self>) -> bool {
        let host = self
            .home(cx)
            .rows()
            .find(|row| row.session_id == session_id)
            .map(|row| row.host.clone());
        let Some(host) = host else {
            return false;
        };
        self.open_session(host, session_id, cx);
        true
    }

    /// The shown session's label as its header draws it.
    #[must_use]
    pub fn session_label(&self, cx: &App) -> Option<String> {
        self.session_header(cx).map(|header| header.label)
    }

    /// The shown session's status word as its header draws it.
    #[must_use]
    pub fn session_status(&self, cx: &App) -> Option<&'static str> {
        self.session_header(cx)?.status.map(|(word, _)| word)
    }

    /// The Terminal toggle's label, while it shows.
    #[must_use]
    pub fn session_toggle(&self, cx: &App) -> Option<&'static str> {
        let header = self.session_header(cx)?;
        header
            .toggle
            .then(|| self.shown_page(header.chat_mirrored).toggle_label())
    }

    /// One back step on the session page: the prompt sheet goes down while
    /// it is up, else the composer's open dock closes, else the page steps
    /// back, from the Terminal page to the Chat page while the toggle shows,
    /// else back to Home.
    pub(super) fn session_back(&mut self, cx: &mut Context<Self>) {
        let (mirrored, toggle) = self.session_header(cx).map_or((false, false), |header| {
            (header.chat_mirrored, header.toggle)
        });
        let sheet_up = self.prompt_sheet_up(cx);
        let drawn = self
            .shown
            .as_ref()
            .is_some_and(|(_, id)| self.dock_drawn(id, self.shown_page(mirrored), cx));
        let step = match self.page.as_mut() {
            Some(page) => {
                let over = session::Over {
                    sheet_up,
                    dock_drawn: drawn,
                };
                session::back_over_sheet(over, page, mirrored, toggle)
            }
            None => BackStep::Close,
        };
        match step {
            BackStep::DismissSheet => self.put_prompt_sheet_away(cx),
            BackStep::CloseDock => {
                self.composer_dock = Dock::None;
                cx.notify();
            }
            BackStep::ToChat => cx.notify(),
            BackStep::Close => self.close_session(cx),
        }
    }

    /// A back gesture over the window while a session is shown: a back step
    /// once it commits.
    pub(super) fn session_back_gesture(
        &mut self,
        event: &BackGestureEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if session::back_on(event.phase) {
            self.session_back(cx);
        }
    }

    /// The toggle's tap: the other page, with the composer's dock closed
    /// since the page it draws on leaves the screen.
    fn toggle_page(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.page.as_mut() {
            page.toggle();
            self.composer_dock = Dock::None;
            // Leaving the Chat page, or coming back to it, ends its selection.
            if let Some((_, id)) = self.shown.clone() {
                self.root
                    .update(cx, |root, cx| root.clear_phone_chat_selection(&id, cx));
            }
            cx.notify();
        }
    }

    /// A release at window point `at` in session `session_id`'s chat body
    /// ends its selection once the release's own handlers (a button, a
    /// link, a stretch, a card, a handle's replayed tap) have acted on it as
    /// it was; a release inside a selection handle's hit area keeps it.
    fn chat_released(
        &self,
        session_id: &str,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.root.read(cx).phone_chat_release_on_handle(at) {
            return;
        }
        let root = self.root.downgrade();
        let id = session_id.to_owned();
        window.defer(cx, move |_, cx| {
            // Fails only when the app's root is gone.
            root.update(cx, |root, cx| root.clear_phone_chat_selection(&id, cx))
                .ok();
        });
    }

    /// A release anywhere in the shown session's chat body: while the
    /// on-screen keyboard is up it goes away, whatever child handled the
    /// release, since this runs in the capture phase before any of them. A
    /// pan never reaches here: it is delivered as scroll events with no
    /// mouse release, so a drag that scrolls the chat leaves the keyboard.
    fn chat_tapped(keyboard_up: bool, window: &mut Window) {
        if keyboard_up {
            keyboard::lower(window);
        }
    }

    /// The area over session `session_id`'s chat body that takes a long
    /// press: the hold selects the word under the finger, the held finger's
    /// moves widen the selection, and its lift ends the drag.
    fn chat_hold(&self, session_id: &str) -> impl IntoElement + use<> {
        let (press_root, move_root) = (self.root.downgrade(), self.root.downgrade());
        let end_root = self.root.downgrade();
        let (press_id, move_id) = (session_id.to_owned(), session_id.to_owned());
        let end_id = session_id.to_owned();
        // Each update fails only when the app's root is gone.
        long_press_drag_area(
            move |at, _, cx| {
                press_root
                    .update(cx, |root, cx| root.phone_select_start(&press_id, at, cx))
                    .ok();
            },
            move |at, _, cx| {
                move_root
                    .update(cx, |root, cx| root.phone_select_widen(&move_id, at, cx))
                    .ok();
            },
            move |_, cx| {
                end_root
                    .update(cx, |root, _| root.phone_select_hold_end(&end_id))
                    .ok();
            },
        )
    }

    /// The drag areas of the two selection handles over session
    /// `session_id`'s chat body: a drag on one moves its end of the
    /// selection.
    fn chat_handles(&self, session_id: &str) -> HandleAreas {
        HandleAreas {
            start: self.chat_handle(session_id, HandleEnd::Start),
            end: self.chat_handle(session_id, HandleEnd::End),
        }
    }

    /// The drag area of selection handle `end` over session `session_id`'s
    /// chat body.
    fn chat_handle(&self, session_id: &str, end: HandleEnd) -> AnyElement {
        let roots = [
            self.root.downgrade(),
            self.root.downgrade(),
            self.root.downgrade(),
        ];
        let [start_root, move_root, end_root] = roots;
        let (start_id, move_id, end_id) = (
            session_id.to_owned(),
            session_id.to_owned(),
            session_id.to_owned(),
        );
        // Each update fails only when the app's root is gone.
        touch_drag_area(
            end.key(),
            move |grab, _, cx| {
                start_root
                    .update(cx, |root, cx| {
                        root.phone_handle_start(&start_id, end, grab, cx);
                    })
                    .ok();
            },
            move |at, _, cx| {
                move_root
                    .update(cx, |root, cx| root.phone_handle_move(&move_id, at, cx))
                    .ok();
            },
            move |_, cx| {
                end_root
                    .update(cx, |root, cx| root.phone_handle_end(&end_id, cx))
                    .ok();
            },
        )
        .into_any_element()
    }

    /// While a session is shown: its page, the header over the body of the
    /// page it is on, with whether the on-screen keyboard is up for the
    /// composer's buttons and the chat's tap.
    /// Off the Chat page, and with no session shown, the chat's texts from
    /// its last frame are forgotten.
    pub(super) fn session_page(&self, keyboard_up: bool, cx: &mut Context<Self>) -> Option<Div> {
        let Some(header) = self.session_header(cx) else {
            self.root.read(cx).forget_phone_chat_text();
            return None;
        };
        let page = self.shown_page(header.chat_mirrored);
        let t = theme(cx);
        // `Show prompt` sits between the page's body and the composer.
        let mut show_prompt = self.show_prompt_button(cx);
        let body = match page {
            Page::Chat => {
                let id = self.shown.as_ref().map(|(_, id)| id.clone())?;
                let tapped_id = id.clone();
                let on_tap = cx.listener(move |this, event: &MouseUpEvent, window, cx| {
                    Self::chat_tapped(keyboard_up, window);
                    this.chat_released(&tapped_id, event.position, window, cx);
                });
                let hold = self.chat_hold(&id);
                let handles = self.chat_handles(&id);
                let chat = self.root.update(cx, |root, cx| {
                    root.phone_chat_body(&id, on_tap, hold, handles, cx)
                });
                let composer = self.composer(&id, keyboard_up, cx);
                div()
                    .driver_id(|| "phone-session-chat".to_owned())
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(chat)
                    .children(show_prompt.take())
                    .children(composer)
            }
            Page::Terminal => {
                self.root.read(cx).forget_phone_chat_text();
                let id = self.shown.as_ref().map(|(_, id)| id.clone())?;
                self.terminal_page(&id, keyboard_up, cx)
            }
        };
        Some(
            div()
                .driver_id(|| "phone-session".to_owned())
                .size_full()
                .flex()
                .flex_col()
                .child(Self::session_header_row(&header, page, &t, cx))
                .child(body)
                .children(show_prompt),
        )
    }

    /// The shown session's composer under its chat: the Quick or Keys dock
    /// while one is open, the note saying why it cannot send, the field with
    /// Send, and the buttons under them. None before the chat has loaded.
    fn composer(&self, session_id: &str, keyboard_up: bool, cx: &mut Context<Self>) -> Option<Div> {
        let (input, line, chip, enabled, error) = {
            let root = self.root.read(cx);
            (
                root.phone_composer_input(session_id)?,
                root.chat_composer_line(session_id),
                root.chat_suggestion_chip(session_id)
                    .map(|chip| (chip.text.to_owned(), chip.actions)),
                root.phone_composer_send_enabled(session_id, cx),
                root.chat_composer_error(session_id),
            )
        };
        let chip_live = !composer_line_disables(line.as_ref());
        let note = line.map(|line| Self::composer_note_row(session_id, &line, cx));
        let chip = chip
            .map(|(text, actions)| Self::suggestion_chip(session_id, text, actions, chip_live, cx));
        let dock = if self.dock_drawn(session_id, Page::Chat, cx) {
            self.dock(session_id, cx)
        } else {
            None
        };
        let t = theme(cx);
        let id = session_id.to_owned();
        let field = field_frame(false, Layer::Base, &t)
            .driver_id(|| "phone-composer-field".to_owned())
            .flex_1()
            .min_w(px(0.0))
            .py(px(6.0))
            .child(input);
        let send = button(
            "phone-composer-send",
            button_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                ButtonKind::Primary,
                ButtonSize::Large,
                enabled,
                Layer::Base,
                &t,
            ),
        )
        .min_h(px(TOUCH_TARGET))
        .flex_none()
        .child(SEND_LABEL)
        .when(enabled, |send| {
            send.on_click(cx.listener({
                let id = id.clone();
                move |this, _: &ClickEvent, _, cx| {
                    this.root.update(cx, |root, cx| root.send_composer(&id, cx));
                }
            }))
        });
        Some(
            div()
                .driver_id(|| "phone-composer".to_owned())
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(COMPOSER_GAP))
                .px(px(SIDE_PADDING))
                .py(px(COMPOSER_GAP))
                .border_t_1()
                .border_color(gpui::rgb(t.line))
                .children(dock)
                .children(note)
                .children(chip)
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap(px(COMPOSER_GAP))
                        .child(field)
                        .child(send),
                )
                .children(error.map(|reason| {
                    div()
                        .driver_id(|| "phone-composer-error".to_owned())
                        .text_size(ui_px(cx, STATUS_TEXT_SIZE))
                        .text_color(gpui::rgb(t.danger))
                        .child(reason)
                }))
                .child(Self::composer_buttons(keyboard_up, cx)),
        )
    }

    /// The composer's note row (`phone-composer-note`): `line`'s text, and
    /// for a draft the host can pull or send, Pull (`phone-draft-pull`) and
    /// Send (`phone-draft-send`) after it.
    fn composer_note_row(session_id: &str, line: &ComposerLine, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        let note = div()
            .driver_id(|| "phone-composer-note".to_owned())
            .text_size(ui_px(cx, STATUS_TEXT_SIZE))
            .text_color(gpui::rgb(t.muted))
            .child(line.text());
        if !line.has_draft_actions() {
            return note;
        }
        let (pull_id, send_id) = (session_id.to_owned(), session_id.to_owned());
        let pull = Self::composer_action("phone-draft-pull", "Pull", true, cx).on_click(
            cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.root
                    .update(cx, |root, cx| root.pull_draft(&pull_id, cx));
            }),
        );
        let send = Self::composer_action("phone-draft-send", "Send", true, cx).on_click(
            cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.root
                    .update(cx, |root, cx| root.send_draft(&send_id, cx));
            }),
        );
        div()
            .flex()
            .items_center()
            .gap(px(COMPOSER_GAP))
            .child(note.flex_1().min_w(px(0.0)))
            .child(pull)
            .child(send)
    }

    /// The agent's suggested next prompt above the composer's field
    /// (`phone-suggestion`): `text`, wrapping at the phone's width, and
    /// with its `actions`, Send (`phone-suggestion-send`) and Pull
    /// (`phone-suggestion-pull`) under it, live while `live`.
    fn suggestion_chip(
        session_id: &str,
        text: String,
        actions: bool,
        live: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = theme(cx);
        let chip = field_frame(false, Layer::Base, &t)
            .driver_id(|| "phone-suggestion".to_owned())
            .flex()
            .flex_col()
            .gap(px(COMPOSER_GAP))
            .py(px(6.0))
            .child(
                div()
                    .w_full()
                    .text_size(ui_px(cx, STATUS_TEXT_SIZE))
                    .text_color(gpui::rgb(t.muted))
                    .child(text.clone()),
            );
        if !actions {
            return chip;
        }
        let (send_id, pull_id) = (session_id.to_owned(), session_id.to_owned());
        let (send_text, pull_text) = (text.clone(), text);
        let send =
            Self::composer_action("phone-suggestion-send", "Send", live, cx).when(live, |send| {
                send.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.root.update(cx, |root, cx| {
                        root.send_suggestion(&send_id, &send_text, cx);
                    });
                }))
            });
        let pull =
            Self::composer_action("phone-suggestion-pull", "Pull", live, cx).when(live, |pull| {
                pull.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.root.update(cx, |root, cx| {
                        root.pull_suggestion(&pull_id, &pull_text, cx);
                    });
                }))
            });
        chip.child(
            div()
                .w_full()
                .flex()
                .gap(px(COMPOSER_GAP))
                .child(send.flex_1())
                .child(pull.flex_1()),
        )
    }

    /// An outlined composer action tagged `selector` reading `label`, a
    /// touch target tall, drawn dimmed unless `enabled`.
    fn composer_action(
        selector: &str,
        label: &'static str,
        enabled: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        button(
            selector,
            button_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                ButtonKind::Outlined,
                ButtonSize::Large,
                enabled,
                Layer::Base,
                &theme(cx),
            ),
        )
        .min_h(px(TOUCH_TARGET))
        .flex_none()
        .child(label)
    }

    /// Whether the composer's dock draws now: one is open, the shown page is
    /// the Chat page, and the session's chat has a composer to draw it over.
    /// Both the draw and the back step ask this, so a dock that is set but
    /// off screen never swallows a Back.
    fn dock_drawn(&self, session_id: &str, page: Page, cx: &App) -> bool {
        self.composer_dock != Dock::None
            && page == Page::Chat
            && self
                .root
                .read(cx)
                .phone_composer_input(session_id)
                .is_some()
    }

    /// The buttons under the composer's field, in the order
    /// [`buttons_shown`] gives them: Paste, Quick and Keys, and Hide
    /// keyboard while the on-screen keyboard is up.
    fn composer_buttons(keyboard_up: bool, cx: &mut Context<Self>) -> Div {
        let mut row = div().flex().gap(px(COMPOSER_GAP));
        for button in buttons_shown(keyboard_up) {
            let drawn = phone_button(button.id(), ButtonKind::Outlined, cx)
                .min_h(px(TOUCH_TARGET))
                .flex_1()
                .child(button.label());
            let drawn = match button {
                ComposerButton::Paste => {
                    drawn.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.paste_into_composer(window, cx);
                    }))
                }
                ComposerButton::Quick => drawn.on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| this.tap_composer_quick(cx)),
                ),
                ComposerButton::Keys => drawn.on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| this.tap_composer_keys(cx)),
                ),
                ComposerButton::Hide => {
                    drawn.on_click(cx.listener(|_, _: &ClickEvent, window, cx| {
                        keyboard::lower(window);
                        cx.notify();
                    }))
                }
            };
            row = row.child(drawn);
        }
        row
    }

    /// Paste's tap: the clipboard's text goes into the composer's field at
    /// its caret. A clipboard with no text changes nothing.
    fn paste_into_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, session_id)) = self.shown.as_ref() else {
            return;
        };
        let session_id = session_id.clone();
        let Some(input) = self.root.read(cx).phone_composer_input(&session_id) else {
            return;
        };
        input.update(cx, |input, cx| input.paste_clipboard(window, cx));
    }

    /// The composer's open dock above its row, while one is.
    fn dock(&self, session_id: &str, cx: &mut Context<Self>) -> Option<Div> {
        let t = theme(cx);
        match self.composer_dock {
            Dock::None => None,
            Dock::Quick => Some(self.quick_dock(session_id, QuickTarget::Composer, &t, cx)),
            Dock::Keys => Some(Self::keys_dock(session_id, cx)),
        }
    }

    /// The Quick dock for `target`: each of the shown session's reply
    /// groups under its title, every reply a tap that types it and Return.
    pub(super) fn quick_dock(
        &self,
        session_id: &str,
        target: QuickTarget,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Div {
        let id = session_id.to_owned();
        let mut dock = div()
            .driver_id(move || target.dock_id().to_owned())
            .flex()
            .flex_col()
            .gap(px(COMPOSER_GAP));
        for group in composer::quick_groups(self.shown_has_agent(cx)) {
            let mut row = div().flex().flex_wrap().gap(px(COMPOSER_GAP));
            for (index, text) in group.items.iter().enumerate() {
                let bytes = composer::quick_bytes(text);
                row = row.child(
                    phone_button(
                        &format!("{}-{}-{index}", target.reply_prefix(), group.title),
                        ButtonKind::Outlined,
                        cx,
                    )
                    .min_h(px(TOUCH_TARGET))
                    .child(*text)
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _: &ClickEvent, _, cx| {
                            this.quick_reply(target, &id, &bytes, cx);
                        }
                    })),
                );
            }
            dock = dock.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(ui_px(cx, STATUS_TEXT_SIZE))
                            .text_color(gpui::rgb(t.muted))
                            .child(group.title),
                    )
                    .child(row),
            );
        }
        dock
    }

    /// A Quick reply's tap: `bytes` typed into session `session_id` from the
    /// composer, or through the terminal on the Terminal page.
    fn quick_reply(
        &mut self,
        target: QuickTarget,
        session_id: &str,
        bytes: &[u8],
        cx: &mut Context<Self>,
    ) {
        match target {
            QuickTarget::Composer => self
                .root
                .update(cx, |root, _| root.send_phone_input(session_id, bytes)),
            QuickTarget::Terminal => self.quick_into_terminal(bytes, cx),
        }
    }

    /// The Keys tray: every key of the strip, each a tap that types its
    /// bytes.
    fn keys_dock(session_id: &str, cx: &mut Context<Self>) -> Div {
        let id = session_id.to_owned();
        let mut dock = div()
            .driver_id(|| "phone-dock-keys".to_owned())
            .flex()
            .flex_wrap()
            .gap(px(COMPOSER_GAP));
        for (index, key) in composer::KEY_TRAY.iter().enumerate() {
            let bytes = key.bytes;
            dock = dock.child(
                phone_button(&format!("phone-key-{index}"), ButtonKind::Outlined, cx)
                    .min_h(px(TOUCH_TARGET))
                    .child(key.label)
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _: &ClickEvent, _, cx| {
                            this.root
                                .update(cx, |root, _| root.send_phone_input(&id, bytes));
                        }
                    })),
            );
        }
        dock
    }

    /// The header: Back, the label over the status word, and the toggle
    /// while it shows.
    fn session_header_row(header: &Header, page: Page, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let back = phone_button("phone-session-back", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_none()
            .child(BACK_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.session_back(cx)));
        let toggle = header.toggle.then(|| {
            phone_button("phone-session-toggle", ButtonKind::Outlined, cx)
                .min_h(px(TOUCH_TARGET))
                .flex_none()
                .child(page.toggle_label())
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_page(cx)))
        });
        div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .min_h(px(TOUCH_TARGET))
            .px(px(SIDE_PADDING))
            .py(px(6.0))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .child(back)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .driver_id(|| "phone-session-label".to_owned())
                            .truncate()
                            .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                            .child(header.label.clone()),
                    )
                    .children(
                        header
                            .status
                            .map(|(word, tone)| status_line(word, tone, t, cx)),
                    ),
            )
            .children(toggle)
    }
}

/// Where a Quick dock's replies are typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QuickTarget {
    /// Into the session from the composer, around the terminal.
    Composer,
    /// Through the Terminal page's terminal.
    Terminal,
}

impl QuickTarget {
    /// The dock's driver id.
    fn dock_id(self) -> &'static str {
        match self {
            Self::Composer => "phone-dock-quick",
            Self::Terminal => "phone-terminal-quick",
        }
    }

    /// The start of each reply's driver id, before its group and index.
    fn reply_prefix(self) -> &'static str {
        match self {
            Self::Composer => "phone-quick",
            Self::Terminal => "phone-terminal-quick",
        }
    }
}

/// The status word in its tone's colour, as Home's row draws it.
fn status_line(word: &'static str, tone: Tone, t: &UiTheme, cx: &App) -> Div {
    div()
        .driver_id(|| "phone-session-status".to_owned())
        .text_size(ui_px(cx, STATUS_TEXT_SIZE))
        .text_color(gpui::rgb(tone_color(tone, t)))
        .child(word)
}

#[cfg(test)]
mod tests {
    use super::{ComposerButton, buttons_shown};

    #[test]
    fn the_composer_shows_hide_keyboard_only_while_the_keyboard_is_up() {
        let ids = |keyboard_up: bool| -> Vec<&'static str> {
            buttons_shown(keyboard_up)
                .into_iter()
                .map(ComposerButton::id)
                .collect()
        };
        assert_eq!(
            ids(false),
            [
                "phone-composer-paste",
                "phone-composer-quick",
                "phone-composer-keys"
            ]
        );
        assert_eq!(
            ids(true),
            [
                "phone-composer-paste",
                "phone-composer-quick",
                "phone-composer-keys",
                "phone-composer-hide"
            ]
        );
    }
}
