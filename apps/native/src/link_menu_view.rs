//! The link menu in the window: a Ctrl+Shift+click on a path opens it at the
//! click, its rows open the path in the default app, in the viewer, in a
//! follow tab or in its folder, and Up, Down, Enter, Esc and a click outside
//! drive it.

use std::sync::Arc;

use gpui::{
    AnyElement, Context, Div, Keystroke, MouseButton, MouseDownEvent, ScrollEvent, Stateful,
    Window, WindowId, anchored, deferred, div, prelude::*,
};
use protocol::FEATURE_FILE_TAIL;

use crate::link_menu::{self, LinkMenu, RowPick};
use crate::open;
use crate::open_view::LinkSource;
use crate::overlay_owner::Overlay;
use crate::remote_mode::Capabilities;
use crate::session_menu::{menu_frame, menu_item};
use crate::target::HostKey;
use crate::ui_theme::theme;
use crate::{RootView, fonts};

/// The link menu's selector.
const LINK_MENU: &str = "link-menu";

impl RootView {
    /// Whether the link menu is open, in any window.
    #[must_use]
    pub fn link_menu_open(&self) -> bool {
        self.link_menu.is_some()
    }

    /// The text of the open link menu's row tagged `id` (`link-menu-default`,
    /// `link-menu-viewer`, `link-menu-follow` or `link-menu-folder`); `None`
    /// while the menu is closed, for a row it does not offer, or for any
    /// other id.
    #[must_use]
    pub fn link_menu_row_label(&self, id: &str) -> Option<&'static str> {
        let menu = self.link_menu.as_ref()?;
        menu.rows()
            .into_iter()
            .find(|row| row.id == id)
            .map(|row| row.label)
    }

    /// Opens `menu` in the window the click was in, its first row
    /// highlighted and the keyboard its own; every other menu closes.
    pub(crate) fn open_link_menu(
        &mut self,
        menu: LinkMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_session_menu(window, cx);
        self.close_tab_menu(window, cx);
        self.close_more_menu(window, cx);
        self.close_empty_pane_menu(window, cx);
        self.close_pane_overflow_menu(window, cx);
        self.close_shell_menu(window, cx);
        self.overlays
            .opened(Overlay::LinkMenu, window.window_handle().window_id());
        self.link_menu = Some(menu);
        self.menu_focus.focus(window, cx);
        cx.notify();
    }

    /// Closes the link menu and hands the keyboard back to the terminal of
    /// the window it was open in.
    pub(crate) fn close_link_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.take_link_menu(window, cx);
    }

    /// Closes the link menu as [`Self::close_link_menu`] does and returns
    /// it, `None` when it was not open.
    fn take_link_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<LinkMenu> {
        let menu = self.link_menu.take()?;
        let owner = self.overlays.owner(Overlay::LinkMenu);
        self.hand_back_focus(owner, window, cx);
        cx.notify();
        Some(menu)
    }

    /// Closes the menu and opens its link as row `index` asks.
    fn pick_link_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.take_link_menu(window, cx) else {
            return;
        };
        let Some(row) = menu.rows().get(index).copied() else {
            return;
        };
        let action = match row.pick {
            RowPick::Open(action) => action,
            RowPick::Follow => {
                self.follow_link(menu, window, cx);
                return;
            }
        };
        let LinkMenu {
            link,
            base_dirs,
            session,
            host,
            ..
        } = menu;
        let source = LinkSource {
            host: &host,
            session: session.as_deref(),
            action,
        };
        self.open_link(&link, base_dirs, &source, window, cx);
    }

    /// Whether a link menu on a pane of `host` showing `session` offers
    /// "Follow in tab": the pane has a session, on a host whose daemon
    /// follows files.
    pub(crate) fn link_menu_follows(&self, host: &HostKey, session: Option<&str>) -> bool {
        session.is_some()
            && self
                .conn_of(host)
                .is_some_and(|conn| conn.supports(FEATURE_FILE_TAIL))
    }

    /// Whether a link in a pane of `host` names a file of This PC.
    fn link_is_here(&self, host: &HostKey) -> bool {
        match self.conn_of(host) {
            Some(conn) => Capabilities::for_target(conn.target()).opens_local_files(),
            None => *host == HostKey::Local,
        }
    }

    /// Opens a follow tab of `menu`'s link, through its session's host,
    /// never by a fetch. On This PC it follows the first of the link's
    /// readings that exists, checked on a background thread, as built (a
    /// session folder joined with the link's path) and never canonicalized,
    /// so a folder reached through a link or a mapped drive keeps that
    /// spelling for the daemon's check; else the
    /// first as it stands (a follow of a missing file waits for it); on a
    /// remote host the first as it stands, which that host's daemon
    /// resolves from the session's folder.
    fn follow_link(&mut self, menu: LinkMenu, window: &mut Window, cx: &mut Context<Self>) {
        let LinkMenu {
            link,
            base_dirs,
            session,
            host,
            ..
        } = menu;
        let Some(session) = session else {
            tracing::debug!("a link menu without a session follows nothing");
            return;
        };
        let Some(first) = link.candidates.first().map(|reading| reading.path.clone()) else {
            tracing::debug!("a link without a reading follows nothing");
            return;
        };
        if !self.link_is_here(&host) {
            self.open_follow_tab(&host, &session, &first);
            return;
        }
        let candidates = link.candidates;
        let ui_dir = self.ui_dir.clone();
        let opener = Arc::clone(&self.opener);
        let resolve = cx.background_executor().spawn(async move {
            open::first_existing_reading(
                &candidates,
                &base_dirs,
                ui_dir.as_deref(),
                opener.as_ref(),
            )
            .map(|reading| reading.to_string_lossy().into_owned())
        });
        cx.spawn_in(window, async move |this, cx| {
            let path = resolve.await.unwrap_or(first);
            // Fails only when the view or the main window is gone, leaving
            // no layout to open the tab in.
            RootView::update_in_main_window(&this, cx, |root, _, _| {
                root.open_follow_tab(&host, &session, &path);
            })
            .ok();
        })
        .detach();
    }

    /// The keys an open link menu takes while it holds the keyboard: Up and
    /// Down move the highlight, held to the rows, and Enter picks the
    /// highlighted row. `false` for any other key, with no link menu open,
    /// while the menu does not hold the keyboard, or while it draws in
    /// another window than the one the key arrived in.
    pub(crate) fn on_link_menu_key(
        &mut self,
        ks: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.menu_focus.is_focused(window)
            || !self
                .overlays
                .shows(Overlay::LinkMenu, window.window_handle().window_id())
        {
            return false;
        }
        let Some(menu) = self.link_menu.as_mut() else {
            return false;
        };
        let delta = match ks.key.as_str() {
            "up" => -1,
            "down" => 1,
            "enter" => {
                let index = menu.selected;
                self.pick_link_row(index, window, cx);
                return true;
            }
            _ => return false,
        };
        menu.selected = link_menu::moved(menu.selected, delta, menu.rows().len());
        cx.notify();
        true
    }

    /// The open link menu over a layer that keeps a click outside it from
    /// reaching what lies beneath. The layer takes the wheel too, so a
    /// scroll anywhere, which would move the menu's link, closes it. Only
    /// window `here`, the one it opened in, draws it.
    pub(crate) fn link_menu_layer(
        &self,
        here: WindowId,
        cx: &mut Context<Self>,
    ) -> Option<[AnyElement; 2]> {
        if !self.overlays.shows(Overlay::LinkMenu, here) {
            return None;
        }
        let menu = self.link_menu.as_ref()?;
        let backdrop = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .on_scroll(cx.listener(|this, _: &ScrollEvent, window, cx| {
                this.close_link_menu(window, cx);
            }));
        let panel = anchored()
            .position(menu.at)
            .snap_to_window()
            .child(self.link_menu_panel(menu, cx));
        Some([
            backdrop.into_any_element(),
            deferred(panel).with_priority(1).into_any_element(),
        ])
    }

    /// The menu's card: its rows, the highlighted one drawn as hovered; a
    /// press on a row picks it.
    fn link_menu_panel(&self, menu: &LinkMenu, cx: &mut Context<Self>) -> Stateful<Div> {
        let t = theme(cx);
        let frame = menu_frame(
            fonts::ui_text_size(cx),
            LINK_MENU,
            &self.menu_focus,
            menu.max_height,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.close_link_menu(window, cx);
                cx.stop_propagation();
            }),
            &t,
        );
        let rows: Vec<AnyElement> = menu
            .rows()
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                menu_item(row.id, row.label, false, &t)
                    .when(index == menu.selected, |item| {
                        item.bg(gpui::rgb(t.popover_hover))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.pick_link_row(index, window, cx);
                        }),
                    )
                    .into_any_element()
            })
            .collect();
        frame.children(rows)
    }
}
