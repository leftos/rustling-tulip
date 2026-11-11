//! The What's new window: the changelog of an update, ready or installed,
//! rendered as Markdown over a backdrop, with Close, and on a changelog that
//! could not load the error and a link to the releases page. Which notes
//! it shows and when it opens are [`crate::update_view`]'s.

use futures::future::BoxFuture;
use gpui::{
    AnyElement, ClickEvent, Context, FontWeight, Keystroke, Task, Window, div, prelude::*, px,
};

use crate::RootView;
use crate::driver_id::DriverId as _;
use crate::notice_view::modal_panel;
use crate::open::OpenJob;
use crate::open_view::Then;
use crate::scrollbar::{Scrollbar, Thumb};
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::target::HostKey;
use crate::ui_theme::theme;
use crate::viewer_md_view::{LinkRoute, MdDoc, Painter, prepare_text, render_row};

/// Where every release's notes are, for a changelog that could not load.
const RELEASES_URL: &str = "https://github.com/leftos/rustling-tulip/releases";
const LOADING_NOTE: &str = "Loading the changelog…";
const EMPTY_NOTE: &str = "No release notes for this version.";
const FAILED_NOTE: &str = "Couldn't load the changelog";
/// The tallest the notes grow before they scroll.
const BODY_MAX_HEIGHT: f32 = 420.0;

/// The notes the window shows.
enum Notes {
    /// The changelog is on its way from the feed.
    Loading,
    /// The changelog as Markdown, and its rows.
    Shown { source: String, doc: MdDoc },
    /// The fetch failed with this error.
    Failed(String),
}

impl Notes {
    fn shown(source: &str) -> Self {
        Self::Shown {
            source: source.to_owned(),
            doc: prepare_text(source),
        }
    }
}

/// The open window: the version it names and its notes.
pub(crate) struct WhatsNew {
    version: String,
    notes: Notes,
    /// The changelog fetch under way; closing the window drops it.
    _fetch: Option<Task<()>>,
}

impl RootView {
    /// Whether the What's new window is open.
    #[must_use]
    pub fn whats_new_open(&self) -> bool {
        self.whats_new.is_some()
    }

    /// The window's title, `What's new in {version}`, while it is open.
    #[must_use]
    pub fn whats_new_title(&self) -> Option<String> {
        let dialog = self.whats_new.as_ref()?;
        Some(format!("What's new in {}", dialog.version))
    }

    /// Whether the window holds the keyboard in `window`.
    #[must_use]
    pub fn whats_new_has_keyboard(&self, window: &Window) -> bool {
        self.whats_new.is_some() && self.whats_new_focus.is_focused(window)
    }

    /// What the window shows: the changelog's Markdown, or the loading,
    /// empty or failure line.
    #[must_use]
    pub fn whats_new_text(&self) -> Option<String> {
        let dialog = self.whats_new.as_ref()?;
        Some(match &dialog.notes {
            Notes::Loading => LOADING_NOTE.to_owned(),
            Notes::Shown { source, .. } if source.trim().is_empty() => EMPTY_NOTE.to_owned(),
            Notes::Shown { source, .. } => source.clone(),
            Notes::Failed(error) => format!("{FAILED_NOTE}: {error}"),
        })
    }

    /// The What's new body's scrollbar thumb as the last frame laid it out,
    /// or `None` while the notes fit.
    #[must_use]
    pub fn whats_new_scroll_thumb(&self) -> Option<Thumb> {
        self.whats_new_scroll.thumb()
    }

    /// Opens the window on `notes`, the changelog up to `version`, already
    /// in hand.
    pub(crate) fn open_whats_new(
        &mut self,
        version: String,
        notes: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.whats_new = Some(WhatsNew {
            version,
            notes: Notes::shown(notes),
            _fetch: None,
        });
        self.whats_new_scroll = Scrollbar::default();
        self.whats_new_focus.focus(window);
        cx.notify();
    }

    /// Opens the window loading the changelog up to `version`, which `fetch`
    /// brings; it shows the changelog, or the failure, once `fetch` ends.
    pub(crate) fn open_whats_new_fetching(
        &mut self,
        version: String,
        fetch: BoxFuture<'static, anyhow::Result<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = cx.spawn(async move |this, cx| {
            let answer = fetch.await;
            // Fails only when the view is gone, and the window with it.
            this.update(cx, |root, cx| root.on_whats_new_fetched(answer, cx))
                .ok();
        });
        self.whats_new = Some(WhatsNew {
            version,
            notes: Notes::Loading,
            _fetch: Some(task),
        });
        self.whats_new_scroll = Scrollbar::default();
        self.whats_new_focus.focus(window);
        cx.notify();
    }

    fn on_whats_new_fetched(&mut self, answer: anyhow::Result<String>, cx: &mut Context<Self>) {
        let Some(dialog) = self.whats_new.as_mut() else {
            return;
        };
        dialog.notes = match answer {
            Ok(text) => Notes::shown(&text),
            Err(err) => {
                tracing::warn!("update: the changelog did not load: {err:#}");
                Notes::Failed(format!("{err:#}"))
            }
        };
        cx.notify();
    }

    /// Closes the window and hands the keyboard back to what is under it:
    /// Settings' tab list when Settings is open, else as a closed notice
    /// does.
    pub(crate) fn close_whats_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.whats_new.take().is_none() {
            return;
        }
        if self.settings_open() {
            self.settings_tabs_focus.focus(window);
            cx.notify();
        } else {
            self.after_notice_closed(window, cx);
        }
    }

    /// A key while the window is open: Esc closes it, and every key is the
    /// window's. Returns whether the window was open.
    pub(crate) fn on_whats_new_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.whats_new.is_none() {
            return false;
        }
        if keystroke.key == "escape" {
            self.close_whats_new(window, cx);
        }
        true
    }

    /// The window over a backdrop that takes every click beneath it.
    pub(crate) fn whats_new_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.whats_new.as_ref()?;
        let t = theme(cx);
        let offset = crate::fonts::ui_text_size(cx);
        let shift = crate::fonts::ui_weight_shift(cx);
        let muted = |text: &str| {
            div()
                .text_color(gpui::rgb(t.muted))
                .child(text.to_owned())
                .into_any_element()
        };
        let body = match &dialog.notes {
            Notes::Loading => muted(LOADING_NOTE),
            Notes::Shown { source, .. } if source.trim().is_empty() => muted(EMPTY_NOTE),
            Notes::Shown { doc, .. } => {
                let font = &self.sidebar.ui_state().terminal_font;
                let painter = Painter::new(
                    offset,
                    "whats-new".to_owned(),
                    LinkRoute::WebOnly,
                    (font.family.as_deref(), font.size),
                    &t,
                );
                div()
                    .flex()
                    .flex_col()
                    .children(
                        (0..doc.row_count()).map(|ix| render_row(doc, &painter, ix, 0.0, 0.0)),
                    )
                    .into_any_element()
            }
            Notes::Failed(error) => div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .driver_id(|| "whats-new-error".to_owned())
                        .text_color(gpui::rgb(t.danger))
                        .child(FAILED_NOTE),
                )
                .child(muted(error))
                .into_any_element(),
        };
        let button = |selector: &str, label: &str| {
            dialog_button(
                selector,
                label.to_owned(),
                dialog_look(offset, shift, false, true, &t),
                false,
            )
        };
        let releases = matches!(dialog.notes, Notes::Failed(_)).then(|| {
            button("whats-new-releases", "Open the releases page").on_click(cx.listener(
                |this, _: &ClickEvent, window, cx| {
                    this.dispatch_open(
                        &HostKey::Local,
                        OpenJob::Url(RELEASES_URL.to_owned()),
                        Then::Nothing,
                        window,
                        cx,
                    );
                },
            ))
        });
        let close = button("whats-new-close", "Close").on_click(cx.listener(
            |this, _: &ClickEvent, window, cx| {
                this.close_whats_new(window, cx);
            },
        ));
        let panel = modal_panel(offset, "whats-new-panel", &t)
            .track_focus(&self.whats_new_focus)
            .child(
                div()
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(format!("What's new in {}", dialog.version)),
            )
            .child(
                div()
                    .relative()
                    .child(
                        div()
                            .id("whats-new-body")
                            .driver_id(|| "whats-new-body".to_owned())
                            .max_h(px(BODY_MAX_HEIGHT))
                            .overflow_y_scroll()
                            .track_scroll(self.whats_new_scroll.handle())
                            .child(body),
                    )
                    .child(self.whats_new_scroll.overlay()),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(6.0))
                    .children(releases)
                    .child(close),
            );
        Some(backdrop("whats-new", panel, &t))
    }
}
