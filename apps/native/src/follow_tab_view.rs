//! Follow tabs: asking This PC's daemon for one, activating the tab it
//! names, keeping each tab's follow of its file through the session's host
//! daemon while the tab is in the layout, and the tab's body: a header
//! naming the file and the file's lines as the follow sends them.

use std::collections::HashSet;
use std::time::Instant;

use crate::RootView;
use crate::chat_tail::{Tail, TailShows};
use crate::chat_view::file_name;
use crate::connection::Connection;
use crate::diff_tab::PENDING_ACTIVATE_TTL;
use crate::driver_id::DriverId as _;
use crate::fonts::{self, DEFAULT_FAMILY, ui_px_at};
use crate::grid_view::muted_note;
use crate::tabs::host_of_wire;
use crate::target::HostKey;
use crate::ui_theme::{UiTheme, theme};
use crate::{UI_TEXT_SIZE, new_request_id};
use gpui::{
    AnyElement, Context, Div, ElementId, ScrollEvent, SharedString, Window, div, prelude::*, px,
};
use protocol::{
    ClientMessage, DaemonMessage, FEATURE_FILE_TAIL, FollowSource, TabContent, TabEntry,
};

/// What a follow tab of a source this build does not know says.
const NEWER_APP: &str = "This tab needs a newer app.";

/// What a follow tab says when its host's daemon cannot follow files.
pub const CANNOT_FOLLOW: &str = "This host can't follow files.";

/// What a follow tab says once its host no longer lists its session.
pub const SESSION_ENDED: &str = "The session has ended.";

/// A follow tab's line height, as a multiple of its text size.
const LINE_RATIO: f32 = 1.4;

/// A follow tab's padding, in pixels, across and down.
const PAD_X: f32 = 12.0;
const PAD_Y: f32 = 6.0;

/// The `OpenFollowTab`s out, and the tab one of them waits to activate.
#[derive(Debug, Default)]
pub(crate) struct FollowOpens {
    next_open: u64,
    /// The `OpenFollowTab` ids the daemon has not answered.
    sent: HashSet<String>,
    /// A tab the daemon named before its `TabUpdated` arrived, and when.
    pending: Option<(String, Instant)>,
}

impl FollowOpens {
    /// A fresh `OpenFollowTab` id, recorded as sent.
    fn open_id(&mut self) -> String {
        self.next_open += 1;
        let id = format!("follow-open-{}", self.next_open);
        self.sent.insert(id.clone());
        id
    }

    /// `FollowTabOpened`: the tab to activate now, when this client asked
    /// for it and the tab list already holds it (`known`). A tab the list
    /// does not hold yet waits for its `TabUpdated`; another client's open
    /// is ignored.
    fn opened(&mut self, id: &str, tab_id: &str, known: bool, now: Instant) -> Option<String> {
        if !self.sent.remove(id) {
            return None;
        }
        if known {
            self.pending = None;
            return Some(tab_id.to_owned());
        }
        self.pending = Some((tab_id.to_owned(), now));
        None
    }

    /// The waiting tab, once `known` says the tab list holds it; one that
    /// waited [`PENDING_ACTIVATE_TTL`] is dropped instead.
    fn take_ready(&mut self, known: impl Fn(&str) -> bool, now: Instant) -> Option<String> {
        let (tab_id, at) = self.pending.as_ref()?;
        if now.saturating_duration_since(*at) >= PENDING_ACTIVATE_TTL {
            self.pending = None;
            return None;
        }
        if !known(tab_id) {
            return None;
        }
        self.pending.take().map(|(tab_id, _)| tab_id)
    }

    /// Whether `id` is an `OpenFollowTab` of this client, which is
    /// forgotten.
    fn failed(&mut self, id: &str) -> bool {
        self.sent.remove(id)
    }

    /// A new connection: no answer to an old request will come.
    fn reset(&mut self) {
        self.sent.clear();
        self.pending = None;
    }
}

/// The file a follow tab follows: the tab, its session's host, the session
/// and the path.
struct Followed<'a> {
    tab_id: &'a str,
    host: HostKey,
    session_id: &'a str,
    path: &'a str,
}

/// The file follow tab `tab` follows; none for another kind of tab or a
/// source this build does not know.
fn followed(tab: &TabEntry) -> Option<Followed<'_>> {
    match &tab.content {
        TabContent::Follow {
            source: FollowSource::File { session_id, path },
            host,
        } => Some(Followed {
            tab_id: &tab.id,
            host: host_of_wire(host.as_deref()),
            session_id,
            path,
        }),
        _ => None,
    }
}

/// Where a follow tab's host stands: whether its connection is open,
/// whether its daemon follows files, and whether it lists the session.
#[derive(Debug, Clone, Copy)]
struct HostState {
    open: bool,
    can_tail: bool,
    listed: bool,
}

impl HostState {
    /// Whether the tab may ask the host to follow its file now.
    fn ready(self) -> bool {
        self.open && self.can_tail && self.listed
    }
}

/// What a follow tab's body shows.
#[derive(Debug, PartialEq, Eq)]
enum FollowBody<'a> {
    /// The host's daemon does not follow files.
    CannotFollow,
    /// The host no longer lists the session.
    Ended,
    /// No data yet.
    Waiting,
    /// The daemon's refusal.
    Refused(&'a str),
    /// The file's lines, oldest first.
    Lines(Vec<&'a str>),
}

/// What a follow tab shows, from where its host stands and the follow it
/// reads.
fn follow_body(state: HostState, tail: Option<&Tail>) -> FollowBody<'_> {
    if state.open && !state.can_tail {
        return FollowBody::CannotFollow;
    }
    if state.open && !state.listed {
        return FollowBody::Ended;
    }
    match tail.map(Tail::shows) {
        None | Some(TailShows::Nothing) => FollowBody::Waiting,
        Some(TailShows::Error(message)) => FollowBody::Refused(message),
        Some(TailShows::Lines(lines)) => FollowBody::Lines(lines),
    }
}

/// The text a body shows in place of lines, for the file at `path`; none
/// while it shows lines.
fn body_note(body: &FollowBody<'_>, path: &str) -> Option<String> {
    match body {
        FollowBody::CannotFollow => Some(CANNOT_FOLLOW.to_owned()),
        FollowBody::Ended => Some(SESSION_ENDED.to_owned()),
        FollowBody::Waiting => Some(format!("Waiting for {}\u{2026}", file_name(path))),
        FollowBody::Refused(message) => Some((*message).to_owned()),
        FollowBody::Lines(_) => None,
    }
}

impl RootView {
    /// Asks This PC's daemon to open, or focus, a follow tab of `path` in
    /// session `session_id` of `host`. The layout stays here, so the request
    /// goes here; it names the host the session is on, whose daemon the
    /// follow reads the file through. The path is sent as given: the
    /// session's daemon resolves it.
    pub fn open_follow_tab(&mut self, host: &HostKey, session_id: &str, path: &str) {
        let id = self.follow_opens.open_id();
        self.send_to(
            &HostKey::Local,
            ClientMessage::OpenFollowTab {
                id,
                source: FollowSource::File {
                    session_id: session_id.to_owned(),
                    path: path.to_owned(),
                },
                host: host.wire().map(str::to_owned),
            },
        );
    }

    /// The text `tab_id`'s body shows in place of its file's lines (a
    /// note, or the daemon's refusal), when it is a follow tab showing
    /// one.
    #[must_use]
    pub fn follow_tab_note(&self, tab_id: &str) -> Option<String> {
        let tab = self.tabs.tab(tab_id)?;
        let Some(followed) = followed(tab) else {
            return matches!(tab.content, TabContent::Follow { .. }).then(|| NEWER_APP.to_owned());
        };
        body_note(&self.follow_body_of(&followed), followed.path)
    }

    /// The lines follow tab `tab_id` draws; none while it shows a note.
    #[must_use]
    pub fn follow_tab_lines(&self, tab_id: &str) -> Option<Vec<String>> {
        let followed = followed(self.tabs.tab(tab_id)?)?;
        match self.follow_body_of(&followed) {
            FollowBody::Lines(lines) => Some(lines.into_iter().map(str::to_owned).collect()),
            _ => None,
        }
    }

    /// Whether follow tab `tab_id` follows its file's bottom; none while
    /// it reads no follow.
    #[must_use]
    pub fn follow_tab_following(&self, tab_id: &str) -> Option<bool> {
        self.chat
            .tails
            .tab(tab_id)
            .map(|tail| tail.tab_following(tab_id))
    }

    /// Every follow tab in the layout, with the file it follows.
    fn follow_tabs(&self) -> Vec<Followed<'_>> {
        self.tabs.tabs().iter().filter_map(followed).collect()
    }

    /// Where `host` stands for a follow of session `session_id`.
    fn follow_host_state(&self, host: &HostKey, session_id: &str) -> HostState {
        let conn = self.conn_of(host);
        HostState {
            open: conn.is_some_and(Connection::is_open),
            can_tail: conn.is_some_and(|conn| conn.supports(FEATURE_FILE_TAIL)),
            listed: self.sidebar.session_on(host, session_id).is_some(),
        }
    }

    fn follow_body_of(&self, followed: &Followed<'_>) -> FollowBody<'_> {
        let state = self.follow_host_state(&followed.host, followed.session_id);
        follow_body(state, self.chat.tails.tab(followed.tab_id))
    }

    /// The host and session of every follow tab that may ask for its file
    /// now: its host connected, following files and listing the session.
    pub(crate) fn follow_tab_sessions_ready(&self) -> HashSet<(HostKey, String)> {
        self.follow_tabs()
            .into_iter()
            .filter(|tab| self.follow_host_state(&tab.host, tab.session_id).ready())
            .map(|tab| (tab.host, tab.session_id.to_owned()))
            .collect()
    }

    /// Keeps each follow tab's follow while the tab is in the layout: a tab
    /// that left it stops reading (the follow's last reader sends the
    /// `UntailFile`), and a tab whose host is ready and which reads nothing
    /// yet starts (the follow's first reader sends the `TailFile` to the
    /// session's host). A tab whose host does not follow files, or no
    /// longer lists its session, asks for nothing.
    pub(crate) fn sync_follow_tabs(&mut self) {
        let wanted: Vec<(String, HostKey, String, String, bool)> = self
            .follow_tabs()
            .into_iter()
            .map(|tab| {
                let ready = self.follow_host_state(&tab.host, tab.session_id).ready();
                let (session_id, path) = (tab.session_id.to_owned(), tab.path.to_owned());
                (tab.tab_id.to_owned(), tab.host, session_id, path, ready)
            })
            .collect();
        let in_layout: HashSet<&str> = wanted.iter().map(|(tab_id, ..)| tab_id.as_str()).collect();
        let untails = self
            .chat
            .tails
            .tab_ids()
            .into_iter()
            .filter(|tab_id| !in_layout.contains(tab_id.as_str()))
            .filter_map(|tab_id| self.chat.tails.close_tab(&tab_id))
            .collect();
        self.send_untails(untails);
        self.follow_scrolls
            .retain(|tab_id, _| in_layout.contains(tab_id.as_str()));
        for (tab_id, host, session_id, path, ready) in &wanted {
            self.follow_scrolls.entry(tab_id.clone()).or_default();
            if !ready || self.chat.tails.tab(tab_id).is_some() {
                continue;
            }
            let ask =
                self.chat
                    .tails
                    .open_tab(host.clone(), session_id, path, tab_id, new_request_id());
            if let Some(ask) = ask {
                self.send_to(host, ask);
            }
        }
    }

    /// A follow tab's body (tagged `follow-tab-<id>`): a header naming the
    /// file, its full path and its host when that is not This PC; under it
    /// the file's lines, a note while there are none, or the daemon's
    /// refusal in the danger colour.
    pub(crate) fn follow_tab_element(
        &self,
        tab_id: &str,
        source: &FollowSource,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let offset = fonts::ui_text_size(cx);
        let selector = format!("follow-tab-{tab_id}");
        let FollowSource::File { session_id, path } = source else {
            return muted_note(offset, NEWER_APP, &t)
                .driver_id(move || selector)
                .into_any_element();
        };
        let host = self
            .tabs
            .tab(tab_id)
            .and_then(followed)
            .map_or(HostKey::Local, |tab| tab.host);
        let followed = Followed {
            tab_id,
            host,
            session_id,
            path,
        };
        let body = self.follow_body_of(&followed);
        let content = match body {
            FollowBody::Lines(lines) => self.follow_lines(tab_id, lines, cx),
            FollowBody::Refused(message) => {
                let name = format!("follow-tab-refused-{tab_id}");
                div()
                    .driver_id(move || name)
                    .flex_1()
                    .px(px(PAD_X))
                    .py(px(PAD_Y))
                    .text_size(ui_px_at(offset, UI_TEXT_SIZE))
                    .text_color(gpui::rgb(t.danger))
                    .child(message.to_owned())
                    .into_any_element()
            }
            other => muted_note(offset, body_note(&other, path).unwrap_or_default(), &t)
                .into_any_element(),
        };
        div()
            .driver_id(move || selector)
            .flex()
            .flex_col()
            .size_full()
            .child(self.follow_header(&followed, offset, &t))
            .child(content)
            .into_any_element()
    }

    /// A follow tab's header row: the file's name, its full path in the
    /// muted colour, and its host's name when that is not This PC.
    fn follow_header(&self, followed: &Followed<'_>, offset: i8, t: &UiTheme) -> Div {
        let host = (followed.host != HostKey::Local).then(|| self.host_name(&followed.host));
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.0))
            .px(px(PAD_X))
            .py(px(PAD_Y))
            .border_b_1()
            .border_color(gpui::rgb(t.line_strong))
            .text_size(ui_px_at(offset, UI_TEXT_SIZE))
            .child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(t.text))
                    .child(file_name(followed.path).to_owned()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(gpui::rgb(t.muted))
                    .child(followed.path.to_owned()),
            )
            .children(host.map(|name| {
                div()
                    .flex_none()
                    .text_color(gpui::rgb(t.text_2))
                    .child(name)
            }))
    }

    /// A follow tab's lines (tagged `follow-tab-lines-<id>`) in the
    /// terminal font, scrolling, following the bottom while scrolled to it
    /// and holding still while scrolled up.
    fn follow_lines(&self, tab_id: &str, lines: Vec<&str>, cx: &mut Context<Self>) -> AnyElement {
        let t = theme(cx);
        let size = ui_px_at(fonts::ui_text_size(cx), UI_TEXT_SIZE);
        let line = (size / px(1.0) * LINE_RATIO).round();
        let family = self.sidebar.ui_state().terminal_font.family.as_deref();
        let name = format!("follow-tab-lines-{tab_id}");
        let column = div()
            .id(ElementId::Name(SharedString::from(name.clone())))
            .driver_id(move || name)
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .px(px(PAD_X))
            .py(px(PAD_Y))
            .bg(gpui::rgb(t.sunken))
            .font_family(SharedString::from(
                family.unwrap_or(DEFAULT_FAMILY).to_owned(),
            ))
            .font_weight(fonts::mono_weight())
            .text_size(size)
            .line_height(px(line))
            .text_color(gpui::rgb(t.text_2))
            .children(lines.into_iter().map(|text| div().child(text.to_owned())));
        let Some(handle) = self.follow_scrolls.get(tab_id).cloned() else {
            return column.into_any_element();
        };
        if self.follow_tab_following(tab_id).unwrap_or(false) {
            handle.scroll_to_bottom();
        }
        let tab_id = tab_id.to_owned();
        column
            .track_scroll(&handle)
            .on_scroll(cx.listener(move |_, _: &ScrollEvent, _, cx| {
                let (tab_id, handle) = (tab_id.clone(), handle.clone());
                // Read once the wheel has moved the lines.
                let root = cx.weak_entity();
                cx.defer(move |cx| {
                    let scrolled = -handle.offset().y / px(1.0);
                    let max_scroll = handle.max_offset().y / px(1.0);
                    let read = root.update(cx, |this, cx| {
                        this.chat
                            .tails
                            .on_tab_scrolled(&tab_id, scrolled, max_scroll);
                        cx.notify();
                    });
                    if let Err(err) = read {
                        tracing::debug!("a follow tab scroll's root view has gone: {err:#}");
                    }
                });
            }))
            .into_any_element()
    }

    /// Folds a follow tab's open answer in; returns whether nothing else is
    /// owed it. A follow tab lives in This PC's layout, so only This PC's
    /// answer names one; the tab it names is activated now when it is
    /// listed, else once its `TabUpdated` lists it.
    pub(crate) fn on_follow_message(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if *host != HostKey::Local {
            return false;
        }
        match msg {
            DaemonMessage::Welcome { .. } => {
                self.follow_opens.reset();
                false
            }
            DaemonMessage::FollowTabOpened { id, tab_id } => {
                let known = self.tabs.tab(tab_id).is_some();
                if let Some(tab_id) = self.follow_opens.opened(id, tab_id, known, (self.now)()) {
                    // Re-opening the active tab focuses it again too.
                    self.last_active_tab = None;
                    self.tabs.activate(&tab_id);
                    self.after_tabs_change(window, cx);
                }
                true
            }
            DaemonMessage::Error {
                message,
                request_id: Some(id),
                ..
            } if self.follow_opens.failed(id) => {
                tracing::warn!("the daemon could not open a follow tab: {message}");
                true
            }
            _ => false,
        }
    }

    /// Activates the tab an `OpenFollowTab` named before the tab list held
    /// it, once it does.
    pub(crate) fn activate_pending_follow_tab(&mut self) {
        let now = (self.now)();
        let tabs = &self.tabs;
        let ready = self
            .follow_opens
            .take_ready(|tab_id| tabs.tab(tab_id).is_some(), now);
        if let Some(tab_id) = ready {
            self.tabs.activate(&tab_id);
        }
    }
}
