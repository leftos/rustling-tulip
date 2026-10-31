//! A session's summary drawn in a pane: the pinned question on top, one
//! line per stretch of work under it, each opening into its chat entries,
//! then the running-work strip and the composer. The lines' model lives in
//! [`crate::summary`]; the pinned card, the strip, the composer and the
//! entry rows are the chat pane's.

use gpui::{
    AnyElement, ClickEvent, Context, ElementId, KeyDownEvent, SharedString, div, prelude::*, px,
};
use protocol::StretchLine;

use crate::chat::PaneView;
use crate::chat_view::{
    BODY_PAD_X, LOADING_NOTE as CHAT_LOADING_NOTE, RowLook, SessionChat, card_key, chat_note,
    chat_row, follow_bottom, muted_line,
};
use crate::fonts::ui_px_at;
use crate::scrollbar::Scrollbar;
use crate::summary::{LineEntries, OpenLine, SummaryLog, open_line};
use crate::work_strip::strip_row;
use crate::{RootView, UI_TEXT_SIZE};

/// What a summary pane says while its lines are on their way.
const LOADING_NOTE: &str = "Loading the summary…";
/// What a summary pane says when there is nothing to summarize.
const EMPTY_NOTE: &str = "Nothing to summarize yet";
/// What an open line says over its entries when its first ones are no
/// longer held.
const EARLIER_NOTE: &str = "Earlier messages aren't loaded";
/// The selector prefix of the entry rows an open line draws.
const SUMMARY_ROW: &str = "summary-row";

/// The room between lines, and around the column.
const LINE_GAP: f32 = 4.0;
const BODY_PAD_Y: f32 = 12.0;
const LINE_PAD_X: f32 = 8.0;
const LINE_PAD_Y: f32 = 4.0;
const LINE_RADIUS: f32 = 6.0;
/// How far an open line's entries sit in from its text.
const ENTRIES_INDENT: f32 = 16.0;

impl RootView {
    /// Session `session_id`'s summary as pane `pane_id` draws it.
    pub(crate) fn summary_body(
        &self,
        pane_id: &str,
        session_id: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = format!("pane-summary-{pane_id}");
        let chat = self.chat.session(session_id);
        let summary = self.chat.summaries.get(session_id);
        let look = self.row_look(pane_id, session_id, cx);
        let main = match summary {
            Some(summary) if !summary.lines().is_empty() => {
                summary_column(&look, summary, chat, self.chat.scrolls.get(pane_id), cx)
            }
            Some(summary) if !summary.is_loading() => {
                chat_note(look.text_offset, EMPTY_NOTE, &look.theme)
                    .debug_selector(|| "summary-empty".to_owned())
                    .into_any_element()
            }
            _ => chat_note(look.text_offset, LOADING_NOTE, &look.theme)
                .debug_selector(|| "summary-loading".to_owned())
                .into_any_element(),
        };
        let loaded = chat.filter(|chat| !chat.is_loading());
        let cards = loaded.and_then(|chat| self.card_area(session_id, chat, cx));
        let strip = chat.and_then(|chat| chat.work().strip(self.wall_now()));
        let composer = loaded.and_then(|chat| self.composer_row(session_id, chat, cx));
        let keys_for = session_id.to_owned();
        div()
            .debug_selector(|| name)
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .size_full()
            .capture_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if let Some(key) = card_key(&event.keystroke)
                    && this.chat_key(&keys_for, key, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .children(cards.map(|cards| div().flex_none().pt(px(BODY_PAD_Y)).child(cards)))
            .child(main)
            .children(
                strip.map(|strip| strip_row(look.text_offset, &strip, &look.mono, &look.theme)),
            )
            .children(composer)
            .into_any_element()
    }

    /// A click on line `line_id` of session `session_id`'s summary opens or
    /// closes it.
    pub fn toggle_summary_line(&mut self, session_id: &str, line_id: &str, cx: &mut Context<Self>) {
        if let Some(summary) = self.chat.summaries.get_mut(session_id) {
            summary.toggle_open(line_id);
            cx.notify();
        }
    }

    /// New lines or entries reached session `session_id`'s summary: each
    /// summary pane on it that sat at its bottom before them follows them
    /// there, and one scrolled up stays put. Opening or closing a line moves
    /// no pane.
    pub(crate) fn follow_summary_panes(&self, session_id: &str) {
        for (pane_id, scroll) in &self.chat.scrolls {
            let on_session =
                self.panes.get(pane_id).and_then(|slot| slot.session()) == Some(session_id);
            if on_session && self.pane_view(pane_id) == PaneView::Summary {
                follow_bottom(scroll.handle());
            }
        }
    }

    /// Session `session_id`'s summary lines in the daemon's order; none
    /// while no pane shows its summary.
    #[must_use]
    pub fn summary_lines(&self, session_id: &str) -> Vec<StretchLine> {
        self.chat
            .summaries
            .get(session_id)
            .map(|summary| summary.lines().to_vec())
            .unwrap_or_default()
    }

    /// Whether line `line_id` of session `session_id`'s summary is open.
    #[must_use]
    pub fn summary_line_open(&self, session_id: &str, line_id: &str) -> bool {
        self.chat
            .summaries
            .get(session_id)
            .is_some_and(|summary| summary.is_open(line_id))
    }
}

/// The summary's lines in a scrolling column, each open one followed by its
/// entries.
fn summary_column(
    look: &RowLook<'_>,
    summary: &SummaryLog,
    chat: Option<&SessionChat>,
    scroll: Option<&Scrollbar>,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let mut rows: Vec<AnyElement> = Vec::with_capacity(summary.lines().len());
    for line in summary.lines() {
        rows.push(line_row(look, line, cx));
        if summary.is_open(&line.id) {
            rows.push(open_line_block(look, chat, line, cx));
        }
    }
    let column = div()
        .id(ElementId::Name(SharedString::from(format!(
            "summary-scroll-{}",
            look.pane_id
        ))))
        .size_full()
        .overflow_y_scroll()
        .px(px(BODY_PAD_X))
        .py(px(BODY_PAD_Y))
        .flex()
        .flex_col()
        .gap(px(LINE_GAP))
        .text_size(ui_px_at(look.text_offset, UI_TEXT_SIZE))
        .text_color(gpui::rgb(look.theme.text))
        .children(rows);
    let column = match scroll {
        Some(scroll) => column.track_scroll(scroll.handle()),
        None => column,
    };
    div()
        .relative()
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .child(column)
        .children(scroll.map(Scrollbar::overlay))
        .into_any_element()
}

/// One summary line, tagged `summary-line-<pane>-<line id>`; a click opens
/// or closes it.
fn line_row(look: &RowLook<'_>, line: &StretchLine, cx: &mut Context<RootView>) -> AnyElement {
    let name = format!("summary-line-{}-{}", look.pane_id, line.id);
    let session_id = look.session_id.to_owned();
    let line_id = line.id.clone();
    let hover = look.theme.hover;
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex_none()
        .px(px(LINE_PAD_X))
        .py(px(LINE_PAD_Y))
        .rounded(px(LINE_RADIUS))
        .cursor_pointer()
        .hover(move |style| style.bg(gpui::rgb(hover)))
        .child(line.text.clone())
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.toggle_summary_line(&session_id, &line_id, cx);
        }))
        .into_any_element()
}

/// What an open line shows under it: its chat entries, or, while the chat
/// is on its way, the "Loading the chat…" note (tagged
/// `summary-line-loading-<pane>-<line id>`).
fn open_line_block(
    look: &RowLook<'_>,
    chat: Option<&SessionChat>,
    line: &StretchLine,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let body = chat.map(|chat| (chat, open_line(chat.log(), chat.is_loading(), line)));
    let rows = match body {
        Some((chat, OpenLine::Entries(slice))) => entry_rows(look, chat, line, slice, cx),
        Some((_, OpenLine::Loading)) | None => {
            let name = format!("summary-line-loading-{}-{}", look.pane_id, line.id);
            vec![note_row(look, CHAT_LOADING_NOTE, name)]
        }
    };
    div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(LINE_GAP * 2.0))
        .pl(px(ENTRIES_INDENT))
        .pb(px(LINE_GAP))
        .text_color(gpui::rgb(look.theme.text_2))
        .children(rows)
        .into_any_element()
}

/// An open line's chat entries, behind the "Earlier messages aren't
/// loaded" note (tagged `summary-earlier-<pane>-<line id>`) when its first
/// entries are no longer held.
fn entry_rows(
    look: &RowLook<'_>,
    chat: &SessionChat,
    line: &StretchLine,
    slice: LineEntries<'_>,
    cx: &mut Context<RootView>,
) -> Vec<AnyElement> {
    let note = slice.earlier_missing.then(|| {
        let name = format!("summary-earlier-{}-{}", look.pane_id, line.id);
        note_row(look, EARLIER_NOTE, name)
    });
    note.into_iter()
        .chain(
            slice
                .entries
                .into_iter()
                .map(|entry| chat_row(look, chat, entry, SUMMARY_ROW, cx)),
        )
        .collect()
}

/// A muted note among an open line's rows, tagged `name`.
fn note_row(look: &RowLook<'_>, text: &str, name: String) -> AnyElement {
    muted_line(look.text_offset, text.to_owned(), &look.theme)
        .debug_selector(|| name)
        .into_any_element()
}
