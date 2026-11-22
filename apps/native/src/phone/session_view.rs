//! Draws the phone's session page ([`super::session`]): a header with
//! Back, the session's label and status word and, at its right end, the
//! Terminal toggle, over the Chat or Terminal page's body. The page's root
//! takes the back gesture. It only renders and forwards taps.

use std::rc::Rc;

use gpui::{
    AnyElement, App, BackGestureEvent, ClickEvent, Context, Div, FontWeight, MouseUpEvent, Pixels,
    Point, ScrollEvent, Stateful, TouchPhase, Window, canvas, div, prelude::*, px,
};

use super::composer::Dock;
use super::home::{self, Tone};
use super::home_view::{
    SIDE_PADDING, STATUS_TEXT_SIZE, TITLE_CHIP_GAP, TOUCH_TARGET, title_chip, tone_color,
};
use super::hosts_view::phone_button;
use super::key_grid::{self, GridKey, KeyAction, KeyModes};
use super::long_press::{long_press_drag_area, touch_drag_area};
use super::pasteboard::{self, PasteSpot};
use super::photos::{self, PHOTOS_LABEL};
use super::prompt_sheet;
use super::quick_replies::{self, ReplySet};
use super::session::{self, BackStep, Header, Page, SessionPage};
use super::{PhoneLayout, PhoneRoot};
use crate::RootView;
use crate::buttons::{ButtonKind, ButtonSize, Layer, button, button_look, field_frame};
use crate::chat::ComposerLine;
use crate::chat_view::{HandleAreas, HandleEnd, chat_available, composer_line_disables};
use crate::dashboard_rows::ContextLabel;
use crate::driver_id::DriverId as _;
use crate::fonts::ui_px;
use crate::keyboard;
use crate::linear_card::OPEN_IN_LINEAR;
use crate::linear_card_view::card_sheet_body;
use crate::target::HostKey;
use crate::term_input;
use crate::text_input::SystemPaste;
use crate::ui_theme::{UiTheme, theme};
use protocol::{FEATURE_UPLOAD_FILE, ShiftEnter};

/// The session page's Back button.
const BACK_LABEL: &str = "Back";

/// The composer's Send button's label.
const SEND_LABEL: &str = "Send";
/// The composer's Paste button's label.
const PASTE_LABEL: &str = "Paste";
/// The Quick dock's button's label.
const QUICK_LABEL: &str = "Quick";

/// Whether session `session_id`'s chat host takes uploads
/// (`FEATURE_UPLOAD_FILE`), so the composer offers Photos.
pub(super) fn takes_uploads(root: &RootView, session_id: &str) -> bool {
    root.loaded_chat_host(session_id)
        .and_then(|host| root.conn_of(&host))
        .is_some_and(|conn| conn.supports(FEATURE_UPLOAD_FILE))
}
/// Whether the system paste control takes the Paste buttons' and keys'
/// taps: on iOS 16 and later ([`super::paste_control_ios`]).
#[cfg(target_os = "ios")]
pub(super) fn control_pastes() -> bool {
    super::paste_control_ios::supported()
}

/// Off iOS the Paste button's own tap pastes.
#[cfg(not(target_os = "ios"))]
pub(super) const fn control_pastes() -> bool {
    false
}

/// The Paste button at `spot`, recording where it is drawn each frame and
/// the clip it is drawn in, for the system paste control to sit over.
#[cfg(target_os = "ios")]
pub(super) fn record_paste_bounds(button: Stateful<Div>, spot: PasteSpot) -> Stateful<Div> {
    button.child(
        gpui::canvas(
            move |bounds, window, _| {
                let clip = window.content_mask().bounds;
                super::paste_control_ios::record_button(spot, bounds, clip);
            },
            |_, (), _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full(),
    )
}

/// Off iOS there is no paste control to place.
#[cfg(not(target_os = "ios"))]
pub(super) fn record_paste_bounds(button: Stateful<Div>, _: PasteSpot) -> Stateful<Div> {
    button
}

/// The composer field's system paste hook: where the control takes the
/// Paste taps (`control_pastes`), the edit bar's Paste tap reads no
/// clipboard and, on iOS, touch editing tells the control layer where its
/// Paste item and the areas it draws over the window are.
#[cfg(target_os = "ios")]
fn edit_bar_paste(control_pastes: bool) -> SystemPaste {
    use super::paste_control_ios::{record_edit_bar_paste, record_touch_cover};
    SystemPaste {
        takes_tap: control_pastes,
        paste_drawn: control_pastes.then_some(record_edit_bar_paste as fn(gpui::Bounds<Pixels>)),
        cover_drawn: control_pastes.then_some(record_touch_cover as fn(gpui::Bounds<Pixels>)),
    }
}

/// Off iOS no control lies over the bar; specs set `control_pastes` to
/// stand in for iOS 16 and later, so the bar's Paste tap reads nothing.
#[cfg(not(target_os = "ios"))]
fn edit_bar_paste(control_pastes: bool) -> SystemPaste {
    SystemPaste {
        takes_tap: control_pastes,
        paste_drawn: None,
        cover_drawn: None,
    }
}

/// What the Quick dock says while its set has no groups.
const QUICK_EMPTY_TEXT: &str = "No quick replies. Add them in Settings › Quick replies.";
/// The Quick dock's line while its set has no groups.
const QUICK_EMPTY_ID: &str = "phone-quick-empty";
/// The Keys tray's button's label.
const KEYS_LABEL: &str = "Keys";
/// The Hide keyboard button's label, short enough for its fifth of a
/// phone's composer row.
const HIDE_LABEL: &str = "Hide";
/// The room between the composer's rows, and between its buttons.
const COMPOSER_GAP: f32 = 8.0;
/// What the composer's note row asks once the draft's Clear is tapped.
const DRAFT_CLEAR_ASK: &str = "Clear the terminal's draft?";

/// One of the composer row's buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerButton {
    /// Paste the clipboard's text into the field, or upload its image.
    Paste,
    /// Pick a photo to upload.
    Photos,
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
            Self::Photos => "phone-composer-photos",
            Self::Quick => "phone-composer-quick",
            Self::Keys => "phone-composer-keys",
            Self::Hide => "phone-composer-hide",
        }
    }

    /// The label drawn on the button.
    fn label(self) -> &'static str {
        match self {
            Self::Paste => PASTE_LABEL,
            Self::Photos => PHOTOS_LABEL,
            Self::Quick => QUICK_LABEL,
            Self::Keys => KEYS_LABEL,
            Self::Hide => HIDE_LABEL,
        }
    }
}

/// The composer row's buttons in the order it draws them: Paste, Photos
/// while the session's host takes uploads (`photos`), Quick and Keys, and
/// Hide keyboard while the on-screen keyboard is up.
fn buttons_shown(keyboard_up: bool, photos: bool) -> Vec<ComposerButton> {
    let mut buttons = vec![ComposerButton::Paste];
    if photos {
        buttons.push(ComposerButton::Photos);
    }
    buttons.extend([ComposerButton::Quick, ComposerButton::Keys]);
    if keyboard_up {
        buttons.push(ComposerButton::Hide);
    }
    buttons
}

impl PhoneRoot {
    /// Drops the draft Clear's question once its session is no longer shown
    /// or its composer no longer offers Clear.
    pub(super) fn keep_draft_clear_confirm(&mut self, cx: &App) {
        let Some(asking) = self.draft_clear_asking.as_deref() else {
            return;
        };
        let stands = self.shown.as_ref().is_some_and(|(_, id)| id == asking)
            && self
                .root
                .read(cx)
                .chat_composer_line(asking)
                .is_some_and(|line| line.offers_clear());
        if !stands {
            self.draft_clear_asking = None;
        }
    }

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
        let root = self.root.read(cx);
        let ended = root
            .sidebar
            .session_on(host, session_id)
            .is_some_and(home::ended);
        let context = root.context_label_of(session_id);
        let mirrored = self.chat_mirrored(host, session_id, cx);
        let home = self.home(cx);
        let header = Header::new(session_id, home.row_of(host, session_id), ended, mirrored)
            .with_context(context);
        let issue = root.title_chips().chip(host, &header.label);
        Some(header.with_issue(issue))
    }

    /// The identifier of the shown session's title chip, while its header
    /// draws one.
    #[must_use]
    pub fn session_issue_identifier(&self, cx: &App) -> Option<String> {
        self.session_header(cx)
            .and_then(|header| header.issue)
            .map(|chip| chip.identifier)
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

    /// A tap on the Terminal toggle: the shown session's other page; on an
    /// iPhone on its side the Terminal page fills the screen.
    pub fn tap_session_toggle(&mut self, cx: &mut Context<Self>) {
        self.toggle_page(cx);
    }

    /// One back step on the session page: the Linear card sheet goes down
    /// while it is up, else the prompt sheet while it is up, else the
    /// composer's open dock closes, else the page steps
    /// back, from the Terminal page to the Chat page while the toggle shows,
    /// else back to Home: in two panes, the selection clears to the empty
    /// session pane.
    pub(super) fn session_back(&mut self, cx: &mut Context<Self>) {
        let (mirrored, toggle) = self.session_header(cx).map_or((false, false), |header| {
            (header.chat_mirrored, header.toggle)
        });
        let card_up = self.root.read(cx).phone_linear_card().is_some();
        let sheet_up = self.prompt_sheet_up(cx);
        // The Terminal page's Quick dock closes on Back as the composer's
        // dock does; `dock_drawn` stays the composer's, which it draws from.
        let drawn = self.shown.as_ref().is_some_and(|(_, id)| {
            let page = self.shown_page(mirrored);
            self.dock_drawn(id, page, cx) || (page == Page::Terminal && self.terminal_quick)
        });
        let step = match self.page.as_mut() {
            Some(page) => {
                let over = session::Over {
                    card_up,
                    sheet_up,
                    dock_drawn: drawn,
                };
                session::back_over_sheet(over, page, mirrored, toggle)
            }
            None => BackStep::Close,
        };
        match step {
            BackStep::DismissCard => self.dismiss_card(cx),
            BackStep::DismissSheet => self.put_prompt_sheet_away(cx),
            BackStep::CloseDock => {
                self.composer_dock = Dock::None;
                self.terminal_quick = false;
                cx.notify();
            }
            BackStep::ToChat => {
                // The user's own page wins over a turn upright's return.
                self.landscape_return = None;
                cx.notify();
            }
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
            // The user's own page wins over a turn upright's return.
            self.landscape_return = None;
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
            |_| true,
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
            move |_, _, cx| {
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
    /// composer's buttons and the chat's tap. A terminal filling the screen
    /// draws no header: its key bar carries Back and the status.
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
        let full = self.layout == PhoneLayout::TerminalFull && page == Page::Terminal;
        let header_row = (!full)
            .then(|| Self::session_header_row(&header, page, !self.layout.two_pane(), &t, cx));
        Some(
            div()
                .driver_id(|| "phone-session".to_owned())
                .size_full()
                .flex()
                .flex_col()
                .children(header_row)
                .children(self.path_note().map(str::to_owned).map(|note| {
                    div()
                        .driver_id(|| "phone-path-note".to_owned())
                        .px(px(16.0))
                        .py(px(6.0))
                        .text_color(gpui::rgb(t.muted))
                        .child(note)
                }))
                .relative()
                .child(body)
                .children(show_prompt)
                .children(self.card_sheet_layer(px(0.0), cx)),
        )
    }

    /// Takes the Linear card sheet down.
    fn dismiss_card(&mut self, cx: &mut Context<Self>) {
        self.root.update(cx, RootView::dismiss_linear_card);
        cx.notify();
    }

    /// A back gesture on the card sheet: the card goes down; with a session
    /// page under it, the page's own back step runs first.
    fn card_back_gesture(
        &mut self,
        event: &BackGestureEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !session::back_on(event.phase) {
            return;
        }
        if self.shown.is_some() {
            self.session_back(cx);
        } else {
            self.dismiss_card(cx);
        }
    }

    /// While the phone's Linear card is up ([`RootView::phone_linear_card`]):
    /// a scrim over the page, whose tap takes the card down, and the card as
    /// a bottom sheet over it, its panel kept `bottom` clear of the window's
    /// edge: a grab handle, the card's content and Open in Linear. A drag
    /// down on it takes it down.
    pub(super) fn card_sheet_layer(&self, bottom: Pixels, cx: &mut Context<Self>) -> Option<Div> {
        let card = self.root.read(cx).phone_linear_card()?;
        let t = theme(cx);
        let scrim = div()
            .id("phone-linear-scrim")
            .driver_id(|| "phone-linear-scrim".to_owned())
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(gpui::rgba(t.scrim))
            .occlude()
            // The scrim hides the page's root from the gesture, so it takes
            // the back gesture itself, and carries on a drag the sheet
            // began once the finger runs past the sheet's top.
            .on_back_gesture(cx.listener(Self::card_back_gesture))
            .on_scroll(cx.listener(Self::card_scrim_scrolled))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.dismiss_card(cx)));
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .flex_col()
                .justify_end()
                .child(scrim)
                .child(self.card_sheet_panel(&card, bottom, &t, cx)),
        )
    }

    /// The card sheet itself, drawn its drag's travel below its place, with
    /// `bottom` kept under its content.
    fn card_sheet_panel(
        &self,
        card: &crate::linear_card::Card,
        bottom: Pixels,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let (offset, height) = {
            let drag = self.root.read(cx).phone_card_drag_ref();
            (drag.offset(), Rc::clone(&drag.height))
        };
        let measure = canvas(
            move |bounds, _, _| height.set(f32::from(bounds.size.height)),
            |_, (), _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        let handle = div().flex_none().flex().justify_center().py(px(6.0)).child(
            div()
                .driver_id(|| "phone-linear-handle".to_owned())
                .w(px(36.0))
                .h(px(5.0))
                .rounded_full()
                .bg(gpui::rgb(t.muted)),
        );
        let open = phone_button("phone-linear-open", ButtonKind::Primary, cx)
            .w_full()
            .min_h(px(TOUCH_TARGET))
            .child(OPEN_IN_LINEAR)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.root
                    .update(cx, |root, cx| root.open_phone_linear_card(window, cx));
                cx.notify();
            }));
        div()
            .id("phone-linear-sheet")
            .driver_id(|| "phone-linear-sheet".to_owned())
            .occlude()
            .relative()
            .top(px(offset))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .pt(px(4.0))
            .pb(bottom + px(16.0))
            .rounded_t(px(14.0))
            .border_t_1()
            .border_color(gpui::rgb(t.line))
            .bg(gpui::rgb(t.raised))
            .on_back_gesture(cx.listener(Self::card_back_gesture))
            .on_scroll(cx.listener(Self::card_sheet_scrolled))
            .child(measure)
            .child(handle)
            .child(div().px(px(SIDE_PADDING)).child(card_sheet_body(card, cx)))
            .child(div().px(px(SIDE_PADDING)).child(open))
    }

    /// A pan over the card sheet: its downward travel feeds the drag, and
    /// a lift that commits ([`prompt_sheet::commits`]) takes the card
    /// down; one that does not springs it back.
    fn card_sheet_scrolled(
        &mut self,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dy = f32::from(event.delta.pixel_delta(window.line_height()).y);
        let at = cx.background_executor().now();
        let committed = self.root.update(cx, |root, _| {
            let drag = root.phone_card_drag();
            match event.touch_phase {
                TouchPhase::Started => drag.started(dy, at),
                TouchPhase::Moved => drag.moved(dy, at),
                TouchPhase::Ended => {
                    let height = drag.height.get();
                    return drag.ended(dy, at).is_some_and(|(travel, speed)| {
                        prompt_sheet::commits(travel, speed, height)
                    });
                }
                TouchPhase::Cancelled => drag.cancelled(),
            }
            false
        });
        if committed {
            // The rest of the pan, its momentum included, never reaches the
            // terminal under the sheet.
            self.prompt_sheet.swallow_rest_of_pan();
            self.dismiss_card(cx);
        }
        cx.notify();
    }

    /// A pan over the card sheet's scrim: it carries on the sheet's drag in
    /// progress, but never starts one.
    fn card_scrim_scrolled(
        &mut self,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.touch_phase != TouchPhase::Started {
            self.card_sheet_scrolled(event, window, cx);
        }
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
        let hook = edit_bar_paste(self.control_pastes);
        input.update(cx, |input, _| input.set_system_paste(hook));
        let photos = takes_uploads(self.root.read(cx), session_id);
        let chip_live = !composer_line_disables(line.as_ref());
        let note = line.map(|line| self.composer_note_row(session_id, &line, cx));
        let chip = chip
            .map(|(text, actions)| Self::suggestion_chip(session_id, text, actions, chip_live, cx));
        let dock = if self.dock_drawn(session_id, Page::Chat, cx) {
            self.dock(session_id, cx)
        } else {
            None
        };
        let strip = self.completion_strip(session_id, cx);
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
                .children(strip)
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
                .child(Self::composer_buttons(keyboard_up, photos, cx)),
        )
    }

    /// The composer's note row (`phone-composer-note`): `line`'s text, and
    /// for a draft the host can pull or send, Pull (`phone-draft-pull`) and
    /// Send (`phone-draft-send`) after it, then Clear (`phone-draft-clear`)
    /// where the host offers it. Once Clear is tapped the row asks first
    /// ([`Self::draft_clear_confirm_row`]).
    fn composer_note_row(
        &self,
        session_id: &str,
        line: &ComposerLine,
        cx: &mut Context<Self>,
    ) -> Div {
        if line.offers_clear() && self.draft_clear_asking.as_deref() == Some(session_id) {
            return Self::draft_clear_confirm_row(session_id, cx);
        }
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
        let clear_id = session_id.to_owned();
        let clear = line.offers_clear().then(|| {
            Self::composer_action("phone-draft-clear", "Clear", true, cx).on_click(cx.listener(
                move |this, _: &ClickEvent, _, cx| {
                    this.draft_clear_asking = Some(clear_id.clone());
                    cx.notify();
                },
            ))
        });
        div()
            .flex()
            .items_center()
            .gap(px(COMPOSER_GAP))
            .child(note.flex_1().min_w(px(0.0)))
            .child(pull)
            .child(send)
            .children(clear)
    }

    /// The question the draft's Clear turns the note row into
    /// (`phone-draft-clear-ask`), with its Clear
    /// (`phone-draft-clear-confirm`) and Cancel (`phone-draft-clear-cancel`).
    fn draft_clear_confirm_row(session_id: &str, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        let id = session_id.to_owned();
        let confirm = Self::composer_action("phone-draft-clear-confirm", "Clear", true, cx)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.draft_clear_asking = None;
                this.root.update(cx, |root, cx| root.clear_draft(&id, cx));
                cx.notify();
            }));
        let cancel = Self::composer_action("phone-draft-clear-cancel", "Cancel", true, cx)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.draft_clear_asking = None;
                cx.notify();
            }));
        let ask = div()
            .driver_id(|| "phone-draft-clear-ask".to_owned())
            .flex_1()
            .min_w(px(0.0))
            .text_size(ui_px(cx, STATUS_TEXT_SIZE))
            .text_color(gpui::rgb(t.text))
            .child(DRAFT_CLEAR_ASK);
        div()
            .flex()
            .items_center()
            .gap(px(COMPOSER_GAP))
            .child(ask)
            .child(confirm)
            .child(cancel)
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

    /// The completion strip above the composer's field
    /// (`phone-completion-strip`): the session's matching commands and files
    /// as one scrolling row of chips (`phone-completion-{i}`), each a tap
    /// that inserts its pick and leaves the keyboard as it is. None while
    /// nothing matches.
    fn completion_strip(&self, session_id: &str, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let chips = self.root.read(cx).chat_completion_chips(session_id)?;
        let id = session_id.to_owned();
        let mut strip = div()
            .id("phone-completion-strip")
            .driver_id(|| "phone-completion-strip".to_owned())
            .flex()
            .gap(px(COMPOSER_GAP))
            .min_w_0()
            .overflow_x_scroll();
        for (index, chip) in chips.into_iter().enumerate() {
            strip = strip.child(
                phone_button(
                    &format!("phone-completion-{index}"),
                    ButtonKind::Outlined,
                    cx,
                )
                .min_h(px(TOUCH_TARGET))
                .flex_none()
                .whitespace_nowrap()
                .child(chip)
                .on_click(cx.listener({
                    let id = id.clone();
                    move |this, _: &ClickEvent, _, cx| {
                        this.root
                            .update(cx, |root, cx| root.tap_completion(&id, index, cx));
                    }
                })),
            );
        }
        Some(strip)
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
    /// [`buttons_shown`] gives them: Paste, Photos while the host takes
    /// uploads, Quick and Keys, and Hide keyboard while the on-screen
    /// keyboard is up. Each takes an equal share of the row, and a label
    /// wider than its share truncates, so the last button stays inside.
    fn composer_buttons(keyboard_up: bool, photos: bool, cx: &mut Context<Self>) -> Div {
        let mut row = div()
            .driver_id(|| "phone-composer-buttons".to_owned())
            .flex()
            .gap(px(COMPOSER_GAP));
        for button in buttons_shown(keyboard_up, photos) {
            let drawn = phone_button(button.id(), ButtonKind::Outlined, cx)
                .min_h(px(TOUCH_TARGET))
                .flex_1()
                .min_w_0()
                .child(div().truncate().child(button.label()));
            let drawn = match button {
                ComposerButton::Paste => record_paste_bounds(
                    drawn.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        // On iOS 16 and later the system's paste control
                        // over the button takes the tap.
                        if !this.control_pastes {
                            this.paste_into_composer(window, cx);
                        }
                    })),
                    PasteSpot::Composer,
                ),
                ComposerButton::Photos => {
                    drawn.on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.pick_photo(cx)))
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

    /// Paste's tap: a pasteboard holding an image and no text uploads the
    /// image to the session's host when that host takes uploads, whose path
    /// then goes in at the caret, as the desktop's image paste does; the
    /// image is never read for a host that cannot take it. Else the
    /// clipboard's text goes into
    /// the composer's field at its caret. The probes raise no "Allow Paste"
    /// prompt; only the image's read does, on this tap. A clipboard with
    /// neither changes nothing.
    fn paste_into_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, session_id)) = self.shown.as_ref() else {
            return;
        };
        let session_id = session_id.clone();
        let Some(input) = self.root.read(cx).phone_composer_input(&session_id) else {
            return;
        };
        if self.pasteboard.has_image()
            && !self.pasteboard.has_text()
            && takes_uploads(self.root.read(cx), &session_id)
        {
            let Some((bytes, format)) = self.pasteboard.image() else {
                tracing::info!(session = %session_id, "phone: the pasteboard's image could not be read");
                return;
            };
            let name = pasteboard::pasted_name(format);
            self.root.update(cx, |root, cx| {
                root.on_chat_paste_image(&session_id, &name, &bytes, cx);
            });
            return;
        }
        input.update(cx, |input, cx| input.paste_clipboard(window, cx));
    }

    /// The last element of `PhoneRoot`'s frame on iOS: a zero-size canvas
    /// whose prepaint, after the page's own, places a system paste control
    /// over each Paste button or key drawn in this frame, and hides the
    /// control of each one not drawn, or every one when a sheet `covered`
    /// the page.
    #[cfg(target_os = "ios")]
    pub(super) fn paste_frame_end(
        this: &Self,
        covered: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !this.control_pastes {
            return None;
        }
        let session = this.shown.as_ref().map(|(_, id)| id.clone());
        let uploads = session
            .as_deref()
            .is_some_and(|id| takes_uploads(this.root.read(cx), id));
        let root = cx.entity().downgrade();
        let t = theme(cx);
        let frame = super::paste_control_ios::PasteFrame {
            covered,
            session,
            terminal: this.terminal_session(),
            uploads,
            colors: super::paste_control_ios::PasteColors {
                text: t.text,
                ground: t.ground,
            },
            // The edit bar's items are outlined buttons in the theme's
            // text on the bar's popover fill.
            popover: super::paste_control_ios::PasteColors {
                text: t.text,
                ground: t.popover,
            },
        };
        let end = gpui::canvas(
            move |_, _, cx| {
                super::paste_control_ios::frame_end(&frame, root, cx);
            },
            |_, (), _, _| {},
        )
        .absolute();
        Some(end.into_any_element())
    }

    /// Off iOS there is no paste control, so the frame ends with nothing.
    #[cfg(not(target_os = "ios"))]
    pub(super) fn paste_frame_end(_: &Self, _: bool, _: &mut Context<Self>) -> Option<AnyElement> {
        None
    }

    /// Photos' tap: the platform's picker opens for one image, which uploads
    /// to the shown session's host as a pasted image does; a cancelled pick
    /// sends nothing.
    fn pick_photo(&mut self, cx: &mut Context<Self>) {
        let Some((_, session_id)) = self.shown.clone() else {
            return;
        };
        let picked = self.photos.pick();
        cx.spawn(async move |this, cx| {
            let Some((bytes, format)) = picked.await else {
                tracing::info!(session = %session_id, "phone: no photo picked");
                return;
            };
            let name = photos::picked_name(chrono::Local::now().naive_local(), format);
            let sent = this.update(cx, |this, cx| {
                this.root.update(cx, |root, cx| {
                    root.on_chat_paste_image(&session_id, &name, &bytes, cx);
                });
            });
            if sent.is_err() {
                tracing::debug!("phone: a photo was picked after the phone's root was gone");
            }
        })
        .detach();
    }

    /// The composer's open dock above its row, while one is.
    fn dock(&self, session_id: &str, cx: &mut Context<Self>) -> Option<Div> {
        let t = theme(cx);
        match self.composer_dock {
            Dock::None => None,
            Dock::Quick => Some(self.quick_dock(session_id, QuickTarget::Composer, &t, cx)),
            Dock::Keys => Some(self.keys_dock(cx)),
        }
    }

    /// The Quick dock for `target`: each of the shown session's reply
    /// groups under its title, every reply a tap that sends it as a chat
    /// message from the composer, or types it and Return through the
    /// terminal. A composer reply is greyed and takes no tap while the
    /// composer could not send it now (no connection, a disabling note, a
    /// send in flight or a card up); the terminal's always take a tap. A
    /// group with no replies draws no section, and a set with no reply at all
    /// says where replies are added.
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
        let set = ReplySet::of(self.shown_has_agent(cx));
        let saved = self.saved_replies(set, cx);
        let groups: Vec<_> = quick_replies::groups(saved.as_deref(), set)
            .into_iter()
            .filter(|group| !group.replies.is_empty())
            .collect();
        if groups.is_empty() {
            dock = dock.child(
                div()
                    .driver_id(|| QUICK_EMPTY_ID.to_owned())
                    .text_size(ui_px(cx, STATUS_TEXT_SIZE))
                    .text_color(gpui::rgb(t.muted))
                    .child(QUICK_EMPTY_TEXT),
            );
        }
        for group in groups {
            let mut row = div().flex().flex_wrap().gap(px(COMPOSER_GAP));
            for (index, text) in group.replies.iter().enumerate() {
                let live = match target {
                    QuickTarget::Composer => self.root.read(cx).chat_send_enabled(session_id, text),
                    QuickTarget::Terminal => true,
                };
                row = row.child(
                    phone_button(
                        &format!("{}-{}-{index}", target.reply_prefix(), group.title),
                        ButtonKind::Outlined,
                        cx,
                    )
                    .min_h(px(TOUCH_TARGET))
                    .child(text.clone())
                    .when(!live, |button| {
                        button.opacity(crate::spawn_view::DISABLED_OPACITY)
                    })
                    .when(live, |button| {
                        button.on_click(cx.listener({
                            let id = id.clone();
                            let reply = text.clone();
                            move |this, _: &ClickEvent, _, cx| {
                                this.quick_reply(target, &id, &reply, cx);
                            }
                        }))
                    }),
                );
            }
            let section = format!("{}-group-{}", target.reply_prefix(), group.title);
            dock = dock.child(
                div()
                    .driver_id(move || section)
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

    /// A Quick reply's tap: `text` sent as session `session_id`'s chat
    /// message from the composer, or typed with Return through the terminal
    /// on the Terminal page.
    fn quick_reply(
        &mut self,
        target: QuickTarget,
        session_id: &str,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        match target {
            QuickTarget::Composer => {
                self.root.update(cx, |root, cx| {
                    root.send_text(session_id, text, cx);
                });
            }
            QuickTarget::Terminal => {
                self.quick_into_terminal(&quick_replies::quick_bytes(text), cx);
            }
        }
    }

    /// The Keys dock: the grid's keys as wrapping rows, each a tap that runs
    /// its action, Ctrl lit while it is armed.
    fn keys_dock(&self, cx: &mut Context<Self>) -> Div {
        let armed = self.key_bar.borrow().armed();
        let mut dock = div()
            .driver_id(|| "phone-dock-keys".to_owned())
            .flex()
            .flex_wrap()
            .gap(px(COMPOSER_GAP));
        let saved = self.root.read(cx).sidebar.ui_state().phone_keys.clone();
        for (index, key) in key_grid::resolved(saved.as_deref()).iter().enumerate() {
            let kind = if armed && key.action == KeyAction::Ctrl {
                ButtonKind::Primary
            } else {
                ButtonKind::Outlined
            };
            let dimmed = key_grid::is_inert(key);
            let button = phone_button(&format!("phone-key-{index}"), kind, cx)
                .min_h(px(TOUCH_TARGET))
                .child(key.label.clone())
                .when(dimmed, |button| {
                    button.opacity(crate::spawn_view::DISABLED_OPACITY)
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.press_grid_key(index, KeySurface::Dock, window, cx);
                }));
            // On iOS 16 and later the system's paste control sits over the
            // dock's Paste key.
            let button = if key.action == KeyAction::Paste {
                record_paste_bounds(button, PasteSpot::Dock)
            } else {
                button
            };
            dock = dock.child(button);
        }
        dock
    }

    /// A tap on the grid's key at `index`, from the Keys dock or the
    /// Terminal page's key bar: Ctrl arms the sticky Ctrl, Paste pastes into
    /// the composer's field or the terminal, Quick opens the Quick dock or
    /// the Terminal page's replies, and a send key types its bytes into the
    /// surface. The dock holds no terminal, so its arrows send the plain
    /// cursor keys.
    pub fn press_grid_key(
        &mut self,
        index: usize,
        surface: KeySurface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let saved = self.root.read(cx).sidebar.ui_state().phone_keys.clone();
        let keys = key_grid::resolved(saved.as_deref());
        let Some(key) = keys.get(index) else {
            return;
        };
        if key_grid::is_inert(key) {
            return;
        }
        match &key.action {
            KeyAction::Ctrl => self.key_bar.borrow_mut().toggle_ctrl(),
            KeyAction::Paste => match surface {
                // On iOS 16 and later the system's paste control over the
                // dock's or the key bar's Paste takes the tap.
                KeySurface::Dock | KeySurface::Terminal if self.control_pastes => {}
                KeySurface::Dock => self.paste_into_composer(window, cx),
                KeySurface::Terminal => {
                    let pane = self.terminal_pane(cx);
                    Self::paste_into_terminal(pane, self.terminal_session(), cx);
                }
            },
            KeyAction::Quick => match surface {
                KeySurface::Dock => self.composer_dock = Dock::Quick,
                KeySurface::Terminal => self.terminal_quick = !self.terminal_quick,
            },
            KeyAction::Send { .. } => self.press_send_key(key, surface, cx),
            KeyAction::Unknown => {}
        }
        cx.notify();
    }

    /// A tap on a send key: its bytes, armed Ctrl joined to a key of one
    /// press, typed into the terminal on the Terminal page, which reads the
    /// terminal's modes, or sent to the shown session from the dock.
    fn press_send_key(&mut self, key: &GridKey, surface: KeySurface, cx: &mut Context<Self>) {
        let KeyAction::Send { steps } = &key.action else {
            return;
        };
        let sticky = self.key_bar.borrow_mut().take_ctrl_for(key);
        let pane = self
            .terminal_pane(cx)
            .filter(|_| surface == KeySurface::Terminal);
        let shift_enter = match &pane {
            Some(pane) => pane.read(cx).shift_enter(),
            None => self.dock_shift_enter(cx),
        };
        let modes = KeyModes {
            app_cursor: pane.as_ref().is_some_and(|pane| pane.read(cx).app_cursor()),
            shift_enter,
        };
        let Some(bytes) = key_grid::key_bytes(steps, sticky, modes) else {
            return;
        };
        match pane {
            Some(pane) => pane.update(cx, |pane, cx| pane.type_bytes(&bytes, cx)),
            None => {
                if let Some((_, session_id)) = self.shown.clone() {
                    self.root
                        .update(cx, |root, _| root.send_phone_input(&session_id, &bytes));
                }
            }
        }
    }

    /// What Shift+Enter sends from the Keys dock: the shown session's own
    /// choice, as the terminal pane reads it, or a shell's `\` + CR when no
    /// session shows.
    fn dock_shift_enter(&self, cx: &App) -> ShiftEnter {
        let root = self.root.read(cx);
        let Some((host, id)) = self.shown.as_ref() else {
            return ShiftEnter::BackslashCr;
        };
        root.sidebar
            .session(id)
            .map_or(ShiftEnter::BackslashCr, |session| {
                term_input::SessionContext::of(session, root.sidebar.agents(host)).shift_enter
            })
    }

    /// The header: Back while `back` (one pane at a time; Home's list stays
    /// beside the page in two panes), the label over the status word, and
    /// the toggle while it shows.
    fn session_header_row(
        header: &Header,
        page: Page,
        back: bool,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Div {
        let back = back.then(|| {
            phone_button("phone-session-back", ButtonKind::Outlined, cx)
                .min_h(px(TOUCH_TARGET))
                .flex_none()
                .child(BACK_LABEL)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.session_back(cx)))
        });
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
            .children(back)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(TITLE_CHIP_GAP))
                            .min_w_0()
                            .children(
                                header.issue.as_ref().map(|chip| {
                                    title_chip("phone-session-title-chip", chip, t, cx)
                                }),
                            )
                            .child(
                                div()
                                    .driver_id(|| "phone-session-label".to_owned())
                                    .truncate()
                                    .min_w_0()
                                    .flex_1()
                                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                                    .child(header.title_text()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .children(
                                header
                                    .status
                                    .map(|(word, tone)| status_line(word, tone, t, cx)),
                            )
                            .children(
                                header
                                    .context
                                    .as_ref()
                                    .map(|label| context_line(label, t, cx)),
                            ),
                    ),
            )
            .children(toggle)
    }
}

/// Where a tap on a key of the grid happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySurface {
    /// The Keys dock over the composer.
    Dock,
    /// The Terminal page's key bar.
    Terminal,
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
pub(super) fn status_line(word: &'static str, tone: Tone, t: &UiTheme, cx: &App) -> Div {
    div()
        .driver_id(|| "phone-session-status".to_owned())
        .text_size(ui_px(cx, STATUS_TEXT_SIZE))
        .text_color(gpui::rgb(tone_color(tone, t)))
        .child(word)
}

/// The session's context label beside its status word, tagged
/// `phone-session-context`, its text `phone-session-context={text}`, and
/// `phone-session-context-hot` past 80%.
fn context_line(label: &ContextLabel, t: &UiTheme, cx: &App) -> Div {
    let text = label.text.clone();
    let tag = format!("phone-session-context={text}");
    div()
        .driver_id(|| "phone-session-context".to_owned())
        .flex_none()
        .text_size(ui_px(cx, STATUS_TEXT_SIZE))
        .text_color(gpui::rgb(label.colour(t)))
        .child(
            div()
                .when(label.hot, |hot| {
                    hot.driver_id(|| "phone-session-context-hot".to_owned())
                })
                .child(div().driver_id(move || tag).child(text)),
        )
}

#[cfg(test)]
mod tests {
    use super::{ComposerButton, buttons_shown};

    #[test]
    fn the_composer_shows_hide_keyboard_only_while_the_keyboard_is_up() {
        let ids = |keyboard_up: bool| -> Vec<&'static str> {
            buttons_shown(keyboard_up, false)
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
        assert_eq!(ComposerButton::Hide.label(), "Hide");
    }

    #[test]
    fn the_composer_shows_photos_after_paste_only_when_the_host_takes_uploads() {
        let shown: Vec<ComposerButton> = buttons_shown(false, true);
        assert_eq!(
            shown.iter().map(|button| button.id()).collect::<Vec<_>>(),
            [
                "phone-composer-paste",
                "phone-composer-photos",
                "phone-composer-quick",
                "phone-composer-keys"
            ]
        );
        assert_eq!(ComposerButton::Photos.label(), "Photos");
    }
}
