//! The active tab's split tree: a terminal (or an empty placeholder) under a
//! small header in every pane, with draggable dividers between them. Also
//! keeps a terminal per pane attached to the pane's session.

use std::collections::{BTreeSet, HashMap, HashSet};

use alacritty_terminal::vte::ansi::Rgb;

use crate::driver_id::DriverId as _;
use crate::fonts::{ui_px, ui_px_at};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, CursorStyle, Div, DragMoveEvent, ElementId,
    Entity, ExternalPaths, FocusHandle, FontWeight, HighlightStyle, MouseButton, MouseDownEvent,
    Pixels, Point, SharedString, Stateful, StyledText, Subscription, Window, WindowId, anchored,
    canvas, deferred, div, point, prelude::*, px, relative, svg,
};
use protocol::{
    AgentDescriptor, ClientMessage, GridNode, PaneDropEdge, PaneFollow, SessionMember, SessionMode,
    SessionSnapshot, SessionStatus, SplitDirection, SplitPlace, TabContent, TabEntry,
};

use crate::appearance::{self, PaneFrame, Resolved, Source};
use crate::assets::SIDEBAR_MORE_ICON;
use crate::chat::PaneView;
use crate::chat_view::{AGENT_DOWN_TIP, COMPACT_TIP, compact_toggle, offers_chat, view_segment};
use crate::diff_tab::LOADING_TEXT as DIFF_LOADING_TEXT;
use crate::dispatch_view::{RunHeader, RunShown, run_header};
use crate::drag::{self, DragPayload, LeafDrop, OverlayKind};
use crate::ellipsis::{ellipsized, truncating};
use crate::fonts::DEFAULT_FAMILY;
use crate::header_tip::SHOW_DELAY as HEADER_TIP_DELAY;
use crate::link_menu::LinkMenu;
use crate::mouse::ClickThrough;
use crate::open_view::LinkSource;
use crate::pane_header_fit;
use crate::popouts::{self, PopoutKey};
use crate::runtime_icon::{RuntimeIcon, runtime_icon, runtime_icon_view};
use crate::session_actions::StopPlace;
use crate::session_menu::{BorderedButton, MENU_MAX_SHARE, bordered_button};
use crate::shell_dialog::standalone_shell_request;
use crate::sidebar::{can_attach, chip_tip, display_label, label_tooltip, runtime_label};
use crate::spawn_view::{DISABLED_OPACITY, SpawnEntry};
use crate::spawns::{OpenIn, PaneAim};
use crate::status_glyph::{GlyphSize, glyph, glyph_view};
use crate::tabs::{self, PaneBinding, SessionRef, TabsModel};
use crate::target::{HostKey, Target};
use crate::term::prompt_nav::Dir;
use crate::term_view::{PaneEvent, ScrollbackReply, TerminalPane};
use crate::ui_theme::{UiTheme, theme};
use crate::{Drag, PANE_GUTTER, RootView, UI_TEXT_SIZE, new_request_id, tip_box, tooltip};

/// Why a repo-tied spawn button is disabled.
pub(crate) const NO_REPOS_TIP: &str = "Register a repo to spawn repo-tied sessions.";
/// What an Add repo button does.
pub(crate) const ADD_REPO_TIP: &str = "Pick a folder to register as a repo";
/// Why a pane's spawn buttons are disabled while a spawn aimed at it waits.
pub(crate) const PANE_PENDING_TIP: &str = "A new session is on its way to this pane";
/// What a pane's body shows in the main grid while its terminal draws in a
/// pop-out window.
pub const POPPED_OUT_NOTE: &str = "Popped out to its own window";
/// What a popped pane's card's Focus window does.
const FOCUS_WINDOW_TIP: &str = "Bring the pop-out window to the front";
/// What a popped pane's card's Dock back does.
pub(crate) const DOCK_BACK_TIP: &str = "Close the pop-out window and show the pane here again";
/// What a popped tab's Dock back does, on its card and in its pop-out.
pub(crate) const TAB_DOCK_BACK_TIP: &str = "Close the pop-out window and show the tab here again";
/// Why an empty pane's spawn buttons are disabled in a tab pop-out.
pub(crate) const SPAWN_FROM_MAIN_TIP: &str = "Spawn from the main window";
const PANE_SPAWN_TIP: &str = "Open the spawn dialog; the new session fills this pane";
const PANE_SHELL_TIP: &str = "A plain shell in this pane, in the remembered folder";
pub(crate) const SPAWN_TIP: &str = "Spawn a new session";
const OPEN_SHELL_TIP: &str = "A plain shell in a new tab, in the remembered folder";

/// The pane header's height: a fixed strip in a splittable grid.
const PANE_HEADER_HEIGHT: f32 = 36.0;
/// The corner radius of a pane card and its header.
const PANE_RADIUS: f32 = 8.0;
/// The name's size in a pane header.
const TITLE_TEXT_SIZE: f32 = 12.5;
/// A header chip's height: one line inside the fixed header strip.
const CHIP_HEIGHT: f32 = 20.0;
const CHIP_RADIUS: f32 = 5.0;
const CHIP_TEXT_SIZE: f32 = 11.0;
/// A header chip's text shrink against the title's 1: large enough that the
/// chip gives up nearly all its width before the title gives any.
const BRANCH_CHIP_SHRINK: f32 = 1000.0;
/// The least width a shrinking header chip keeps: a few characters and its
/// `…`, so once a chip reaches it the title shrinks next. A chip narrower
/// than this keeps its own width. A header too narrow to hold its chips at
/// this floor ([`pane_header_fit::floors_fit`]) lets them shrink to nothing.
const CHIP_MIN_WIDTH: f32 = 56.0;
const HEADER_BUTTON_SIZE: f32 = 26.0;
const HEADER_BUTTON_RADIUS: f32 = 6.0;
/// The accent line down a pane header's left edge, below its top corner.
const ACCENT_LINE_WIDTH: f32 = 3.0;

/// A pane's terminal and the session it shows.
pub(crate) struct PaneSlot {
    view: Entity<TerminalPane>,
    tab_id: String,
    /// The host of the pane's session, from the pane itself (or a session
    /// pop-out's key); everything the terminal sends goes there.
    host: HostKey,
    /// The pane's session; attached once its host's session list names it.
    session: Option<String>,
    /// The session a new connection unbound the pane from, until the next
    /// tab list binds it again.
    unbound_from: Option<String>,
    attached: bool,
    /// The accent the pane's last apply resolved, `0xRRGGBB`.
    accent: u32,
    /// The window whose focus-in events the pane follows, the one that
    /// draws it, and that subscription.
    focus_in: (WindowId, Subscription),
    _events: Subscription,
}

impl PaneSlot {
    pub(crate) fn view(&self) -> &Entity<TerminalPane> {
        &self.view
    }

    /// The host of the session the pane shows.
    pub(crate) fn host(&self) -> &HostKey {
        &self.host
    }

    /// The session the pane shows.
    pub(crate) fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// The session the pane shows, with its host.
    pub(crate) fn session_ref(&self) -> Option<SessionRef> {
        self.session
            .as_ref()
            .map(|id| SessionRef::new(self.host.clone(), id.clone()))
    }

    /// Whether the pane shows `session`, host and id both.
    fn shows(&self, session: &SessionRef) -> bool {
        self.host == session.host && self.session.as_deref() == Some(session.id.as_str())
    }
}

/// One scrollback retry per session and attempt, and a fresh request id for
/// every `LoadScrollback`. The panes showing a session time out together,
/// and every `LoadScrollback` restarts the daemon's output stream for it, so
/// only the first report goes out.
#[derive(Debug, Default)]
pub(crate) struct RetryGate {
    rounds: HashMap<SessionRef, Round>,
}

/// A session's latest `LoadScrollback`.
#[derive(Debug)]
struct Round {
    /// The id it went out under.
    request_id: String,
    /// Its retry attempt; 0 for the attach's first request.
    attempt: usize,
}

impl RetryGate {
    /// A fresh attach of the session: counts its retries from the start and
    /// returns the id for its first request.
    pub(crate) fn start(&mut self, session: &SessionRef) -> String {
        let request_id = new_request_id();
        self.rounds.insert(
            session.clone(),
            Round {
                request_id: request_id.clone(),
                attempt: 0,
            },
        );
        request_id
    }

    /// The id to send a pane's retry `attempt` for the session under, when
    /// it is the one to send: the pane waited on the session's latest
    /// request, and no pane has reported this attempt yet.
    pub(crate) fn claim(
        &mut self,
        session: &SessionRef,
        request_id: Option<&str>,
        attempt: usize,
    ) -> Option<String> {
        let round = self.rounds.get_mut(session)?;
        if request_id != Some(round.request_id.as_str()) || round.attempt >= attempt {
            return None;
        }
        round.request_id = new_request_id();
        round.attempt = attempt;
        Some(round.request_id.clone())
    }
}

/// What a pane shows while its session has no terminal attached yet.
const WAITING_NOTE: &str = "Waiting for session…";
/// What a pane bound to a headless session draws in place of a terminal.
const HEADLESS_NOTE: &str = "Headless session — not shown in this client";

/// The ratio the split at `split_path` of `tab_id` takes with the pointer
/// at `at`, when the tab has been laid out in `bounds`.
pub(crate) fn divider_ratio(
    model: &TabsModel,
    bounds: Option<Bounds<Pixels>>,
    tab_id: &str,
    split_path: &[u8],
    at: (f32, f32),
) -> Option<f32> {
    let grid = model.tab(tab_id)?.grid()?;
    let rect = grid_rect(bounds?);
    let divider = tabs::dividers(grid, rect, PANE_GUTTER)
        .into_iter()
        .find(|d| d.split_path == split_path)?;
    Some(tabs::ratio_at(&divider, at.0, at.1))
}

/// What the panes of a grid laid out in `bounds` fill: the area inside the
/// gutter that pads it.
fn grid_rect(bounds: Bounds<Pixels>) -> tabs::Rect {
    tabs::Rect::new(
        bounds.origin.x / px(1.0) + PANE_GUTTER,
        bounds.origin.y / px(1.0) + PANE_GUTTER,
        (bounds.size.width / px(1.0) - 2.0 * PANE_GUTTER).max(0.0),
        (bounds.size.height / px(1.0) - 2.0 * PANE_GUTTER).max(0.0),
    )
}

/// The pane that answers each session's terminal queries, by session: its
/// size driver, whose grid matches the PTY, else the first pane in layout
/// order that shows it. Every pane showing a session sees its queries, and
/// each answer reaches the child as input, so only one pane answers.
fn query_answerers(
    bindings: &[PaneBinding],
    drivers: &HashMap<SessionRef, String>,
) -> HashMap<SessionRef, String> {
    let mut answerers = drivers.clone();
    for binding in bindings {
        if let Some(session_id) = &binding.session_id {
            answerers
                .entry(SessionRef::new(binding.host.clone(), session_id.clone()))
                .or_insert_with(|| binding.pane_id.clone());
        }
    }
    answerers
}

impl RootView {
    /// Brings the terminals in line with the tab model: one per pane,
    /// attached to the pane's session and sizing the PTY only in the active
    /// tab. `Detach` goes out once no pane shows a session; the panes still
    /// showing a session that lost one re-attach. The session the focused
    /// pane now shows has its finished turn seen, as a focus would, while
    /// the main window is active.
    pub(crate) fn reconcile_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let before = tabs::view_counts(self.panes.values().filter_map(PaneSlot::session_ref));
        let bindings = self.tabs.bindings();
        self.drop_removed_panes(&bindings, cx);
        let mut to_sync = self.bind_panes(bindings, window, cx);
        // A session pop-out's terminal shows its session beside the panes.
        let mut after = self.tabs.view_counts();
        for session in self.popouts.popped_sessions() {
            *after.entry(session).or_insert(0) += 1;
        }
        let changes = tabs::session_changes(&before, &after);
        for session in changes.detach {
            self.send_to(
                &session.host,
                ClientMessage::Detach {
                    session_id: session.id,
                },
            );
        }
        to_sync.extend(changes.resync);
        for session in to_sync {
            self.sync_session(&session, cx);
        }
        // Shown only while the main window is active: a session counts as
        // watched only while its window is.
        let watched = self.is_main_window(window) && window.is_window_active();
        if let Some(id) = self.shown_session().filter(|_| watched) {
            self.mark_session_seen(None, &id, cx);
        }
        self.sync_chat_loads(cx);
        self.update_pane_roles(cx);
        self.follow_pane_focus_windows(cx);
    }

    /// Drops the terminals of panes no tab holds any more; a session
    /// pop-out's terminal, in no tab, goes only with its window.
    fn drop_removed_panes(&mut self, bindings: &[PaneBinding], cx: &mut Context<Self>) {
        let live: HashSet<&str> = bindings.iter().map(|b| b.pane_id.as_str()).collect();
        self.panes.retain(|pane_id, slot| {
            let keep = live.contains(pane_id.as_str()) || popouts::slot_session(pane_id).is_some();
            if !keep {
                slot.view.update(cx, |pane, _| pane.release());
            }
            keep
        });
    }

    /// Gives every pane a terminal and records its session with its host;
    /// a pane whose session or host changed lets its terminal go, and one
    /// that moved to another host sends there from now on. Returns the
    /// sessions a pane newly shows.
    fn bind_panes(
        &mut self,
        bindings: Vec<PaneBinding>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> BTreeSet<SessionRef> {
        let init_claim = self.tabs.take_init_claim();
        let mut shown = BTreeSet::new();
        for binding in bindings {
            if !self.panes.contains_key(&binding.pane_id) {
                let slot = self.new_slot(&binding.pane_id, &binding.host, window, cx);
                self.panes.insert(binding.pane_id.clone(), slot);
            }
            let sender = self.hosts.sender(&binding.host);
            // A pane that newly shows a session claims its size when this
            // client's user bound it there — by a bind taken once, or by
            // every pane the first-connect chooser's tab list newly binds.
            let newly_bound = self.panes.get(&binding.pane_id).is_some_and(|slot| {
                slot.session != binding.session_id || slot.host != binding.host
            });
            let bound = newly_bound
                && (init_claim
                    || binding.session_id.as_ref().is_some_and(|id| {
                        let session = SessionRef::new(binding.host.clone(), id.clone());
                        self.tabs.take_bind(&session, &binding.pane_id)
                    }));
            let Some(slot) = self.panes.get_mut(&binding.pane_id) else {
                continue;
            };
            slot.tab_id = binding.tab_id;
            let unbound_from = slot.unbound_from.take();
            if slot.session == binding.session_id && slot.host == binding.host {
                continue;
            }
            // A pane that newly shows a session opens in that session's
            // remembered view; one a new connection unbound keeps its view
            // when it is bound to the same session again.
            let before = slot.session.clone().or(unbound_from);
            let rebound =
                before.is_some() && before == binding.session_id && slot.host == binding.host;
            let rehosted = slot.host != binding.host;
            let remote = binding.host != HostKey::Local;
            slot.view.update(cx, |pane, _| {
                pane.release();
                if bound {
                    pane.claim_next_size();
                }
                if rehosted {
                    pane.set_sender(sender, remote);
                }
            });
            slot.attached = false;
            slot.host = binding.host;
            slot.session = binding.session_id;
            shown.extend(slot.session_ref());
            if !rebound {
                let session_id = slot.session.clone();
                self.open_pane_view(&binding.pane_id, session_id.as_deref());
            }
        }
        // A pane-replacing bind this update did not take is not coming.
        self.tabs.settle_pane_binds();
        shown
    }

    /// A terminal slot for `pane_id`, showing a session of `host`: its
    /// input goes to that host's connection, whichever one it is.
    fn new_slot(
        &mut self,
        pane_id: &str,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PaneSlot {
        let tx = self.hosts.sender(host);
        let remote = *host != HostKey::Local;
        let now = self.now.clone();
        let font = self.sidebar.ui_state().terminal_font.clone();
        let view = cx.new(|cx| TerminalPane::new(pane_id.to_owned(), tx, remote, now, font, cx));
        let focus_in = Self::follow_focus_in(pane_id, &view, window, cx);
        // The callback runs in whichever window last drew the root view, not
        // the one the slot was made in; route_pane_event acts for the window
        // that draws the terminal.
        let events = cx.subscribe_in(
            &view,
            window,
            |this, pane, event: &PaneEvent, window, cx| {
                let slot_id = pane.read(cx).pane_id().to_owned();
                this.route_pane_event(&slot_id, event, window, cx);
            },
        );
        PaneSlot {
            view,
            tab_id: String::new(),
            host: host.clone(),
            session: None,
            unbound_from: None,
            attached: false,
            accent: self.sidebar.theme_base().accent,
            focus_in,
            _events: events,
        }
    }

    /// Follows `view`'s focus-in events in `window`: a focus there runs
    /// [`Self::pane_focused`] for pane `pane_id`.
    fn follow_focus_in(
        pane_id: &str,
        view: &Entity<TerminalPane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (WindowId, Subscription) {
        let handle = view.read(cx).focus_handle();
        let id = pane_id.to_owned();
        let subscription = cx.on_focus_in(&handle, window, move |this, window, cx| {
            this.pane_focused(&id, window, cx);
            cx.notify();
        });
        (window.window_handle().window_id(), subscription)
    }

    /// The window that draws tab `tab_id`'s grid: its pop-out's while the
    /// tab is popped out and its window is open, else the main window.
    pub(crate) fn tab_window(&self, tab_id: &str) -> WindowId {
        self.popout_windows
            .get(&PopoutKey::Tab {
                tab_id: tab_id.to_owned(),
            })
            .map_or_else(
                || self.layouts.main(),
                |popout| gpui::AnyWindowHandle::from(popout.handle).window_id(),
            )
    }

    /// Moves each tab pane's focus follower to the window that draws the
    /// pane now, once this update is over: a click on a pane of a popped
    /// tab focuses it as a click in the main window does. A session
    /// pop-out's terminal belongs to no tab and keeps its own.
    pub(crate) fn follow_pane_focus_windows(&self, cx: &mut Context<Self>) {
        for (pane_id, slot) in &self.panes {
            if popouts::slot_session(pane_id).is_some() {
                continue;
            }
            let drawn_in = self.tab_window(&slot.tab_id);
            if drawn_in == slot.focus_in.0 {
                continue;
            }
            let pane_id = pane_id.clone();
            Self::in_window(drawn_in, cx, move |root, window, cx| {
                root.refollow_focus_in(&pane_id, window, cx);
            });
        }
    }

    /// Pane `pane_id` follows its focus-in events in `window` from now on.
    fn refollow_focus_in(&mut self, pane_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let here = window.window_handle().window_id();
        let Some(view) = self
            .panes
            .get(pane_id)
            .filter(|slot| slot.focus_in.0 != here)
            .map(|slot| slot.view.clone())
        else {
            return;
        };
        let focus_in = Self::follow_focus_in(pane_id, &view, window, cx);
        if let Some(slot) = self.panes.get_mut(pane_id) {
            slot.focus_in = focus_in;
        }
    }

    /// Gives pane `pane_id`'s terminal the keyboard of the window that
    /// draws it: a popped tab's pane takes its pop-out's once this update
    /// is over, whichever tab the main window shows.
    pub(crate) fn focus_pane_where_drawn(
        &self,
        pane_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let drawn_in = self
            .panes
            .get(pane_id)
            .map_or_else(|| self.layouts.main(), |slot| self.tab_window(&slot.tab_id));
        if drawn_in == window.window_handle().window_id() {
            self.focus_pane_view(pane_id, window, cx);
            return;
        }
        let pane_id = pane_id.to_owned();
        Self::in_window(drawn_in, cx, move |root, window, cx| {
            root.focus_pane_view(&pane_id, window, cx);
        });
    }

    pub(crate) fn pane_focused(
        &mut self,
        pane_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A session pop-out's terminal belongs to no tab; its window reports
        // its focus.
        if popouts::slot_session(pane_id).is_some() {
            return;
        }
        if let Some(slot) = self.panes.get(pane_id) {
            let (tab_id, session) = (slot.tab_id.clone(), slot.session_ref());
            if let Some(session) = session {
                self.mark_session_seen(Some(&session.host), &session.id, cx);
            }
            self.set_focused_pane(&tab_id, pane_id, cx);
            self.focus_history.record(&tab_id, Some(pane_id));
            self.preselected_target = None;
            // In a popped tab's own window, the pane shows its session while
            // that window is active, as a pane pop-out's terminal does.
            if self.is_tab_popped(&tab_id) && !self.is_main_window(window) {
                self.popout_terminal_focus(pane_id, window.is_window_active(), cx);
            }
            self.remember_focused_session();
            self.update_pane_roles(cx);
            self.drop_stale_sc_picker();
            self.seed_source_control(window, cx);
        }
    }

    /// Attaches every pane showing the session (host and id) afresh, once
    /// its host's list names it, and asks that host for its scrollback
    /// once: the daemon keeps one output stream per session for this
    /// connection, restarted by each request, so the panes share one.
    fn sync_session(&mut self, target: &SessionRef, cx: &mut Context<Self>) {
        // A remote host's list from before its latest `Welcome` names what
        // its old connection served; its panes wait for the fresh one.
        if target.host != HostKey::Local && !self.listed_since_welcome(&target.host) {
            return;
        }
        let Some(session) = self
            .sidebar
            .sessions_of(&target.host)
            .iter()
            .find(|s| s.id == target.id)
            .filter(|s| can_attach(s))
            .cloned()
        else {
            return;
        };
        let request_id = self.retries.start(target);
        let mut attached = false;
        for slot in self.panes.values_mut().filter(|slot| slot.shows(target)) {
            slot.view.update(cx, |pane, cx| {
                pane.attach(&session, self.sidebar.agents(&target.host), cx);
            });
            slot.attached = true;
            attached = true;
        }
        if attached {
            self.request_scrollback(target, request_id, cx);
        }
    }

    /// Sends `LoadScrollback` for the session to its host under
    /// `request_id`, and tells every pane showing it that only this
    /// request's reply counts.
    fn request_scrollback(
        &mut self,
        session: &SessionRef,
        request_id: String,
        cx: &mut Context<Self>,
    ) {
        for view in self.pane_views_of(session) {
            view.update(cx, |pane, _| pane.expect_scrollback(&request_id));
        }
        self.send_to(
            &session.host,
            ClientMessage::LoadScrollback {
                session_id: session.id.clone(),
                request_id: Some(request_id),
            },
        );
    }

    /// Tells every terminal whether it drives its session's PTY size and
    /// whether it answers the session's terminal queries: a pane open in a
    /// pop-out, or a session pop-out's terminal, does both for its session,
    /// whichever tab the main window shows. A pane showing its chat does
    /// neither: its terminal is not drawn, and the chat never sizes the PTY.
    pub(crate) fn update_pane_roles(&self, cx: &mut Context<Self>) {
        let active = self.tabs.active_id();
        let focused = active.and_then(|tab_id| self.tabs.focused_pane(tab_id));
        let mut bindings = self.tabs.bindings();
        bindings.retain(|binding| {
            !self.terminal_hidden(&binding.pane_id, binding.session_id.as_deref())
        });
        let tab_panes = self.popped_tab_panes();
        let mut popped = self.popouts.popped_panes();
        popped.extend(tab_panes.iter().map(String::as_str));
        let mut popped_sessions = self.popouts.popped_sessions();
        popped_sessions.retain(|session| {
            let slot_id = popouts::session_slot_id(&session.host, &session.id);
            !self.terminal_hidden(&slot_id, Some(&session.id))
        });
        let drivers = tabs::size_drivers(
            &bindings,
            active,
            focused.as_deref(),
            &popped,
            &popped_sessions,
        );
        let answerers = query_answerers(&bindings, &drivers);
        for (pane_id, slot) in &self.panes {
            let holds = |roles: &HashMap<SessionRef, String>| {
                slot.session_ref()
                    .is_some_and(|session| roles.get(&session) == Some(pane_id))
            };
            let (drives, answers) = (holds(&drivers), holds(&answerers));
            slot.view.update(cx, |pane, _| {
                pane.set_drives_size(drives);
                pane.set_answers_queries(answers);
            });
        }
    }

    /// The tab pop-out that draws pane `pane_id` with the rest of its tab.
    pub(crate) fn tab_popout_of_pane(&self, pane_id: &str) -> Option<PopoutKey> {
        let tab_id = &self.panes.get(pane_id)?.tab_id;
        self.is_tab_popped(tab_id).then(|| PopoutKey::Tab {
            tab_id: tab_id.clone(),
        })
    }

    /// The panes of the tabs open in tab pop-outs, which draw there whichever
    /// tab the main window shows.
    fn popped_tab_panes(&self) -> Vec<String> {
        self.popouts
            .keys()
            .iter()
            .filter_map(|key| match key {
                PopoutKey::Tab { tab_id } => Some(tab_id),
                PopoutKey::Pane { .. } | PopoutKey::Session { .. } => None,
            })
            .flat_map(|tab_id| self.tab_pane_sessions(tab_id))
            .map(|(pane_id, _)| pane_id)
            .collect()
    }

    /// Gives every pane the font and colours it resolves to: each field its
    /// session's, else its container's, else the app's, and for the size
    /// its tab's override above all of them. `by_user` says this client's
    /// own font change caused it, so a pane it resizes claims its session.
    pub(crate) fn apply_pane_fonts(&mut self, by_user: bool, cx: &mut Context<Self>) {
        self.apply_fonts_where(|_| true, by_user, cx);
    }

    /// Gives the panes showing `session_id` the font and colours they
    /// resolve to, as a snapshot that moved that session's appearance must.
    pub(crate) fn apply_session_pane_fonts(
        &mut self,
        session_id: &str,
        by_user: bool,
        cx: &mut Context<Self>,
    ) {
        self.apply_fonts_where(
            |slot| slot.session.as_deref() == Some(session_id),
            by_user,
            cx,
        );
    }

    /// This client resized the panes of `tab_id` (the window, a split
    /// divider, the sidebar dragged, folded or unfolded): each one's next changed
    /// size claims.
    pub(crate) fn mark_tab_panes_resized(&self, tab_id: &str, cx: &mut Context<Self>) {
        for slot in self.panes.values().filter(|slot| slot.tab_id == tab_id) {
            slot.view.update(cx, |pane, _| pane.mark_resized());
        }
    }

    /// This client moved pane `pane_id` to another window or resized the
    /// window it draws in: its next changed size claims.
    pub(crate) fn mark_pane_resized(&self, pane_id: &str, cx: &mut Context<Self>) {
        if let Some(slot) = self.panes.get(pane_id) {
            slot.view.update(cx, |pane, _| pane.mark_resized());
        }
    }

    /// [`Self::mark_tab_panes_resized`] for the active tab, unless it draws
    /// in a pop-out: the main window never sizes its panes then.
    pub(crate) fn mark_active_panes_resized(&self, cx: &mut Context<Self>) {
        if let Some(tab_id) = self.tabs.active_id()
            && !self.is_tab_popped(tab_id)
        {
            self.mark_tab_panes_resized(tab_id, cx);
        }
    }

    /// Gives every pane `keep` takes the font and colours it resolves to.
    fn apply_fonts_where(
        &mut self,
        keep: impl Fn(&PaneSlot) -> bool,
        by_user: bool,
        cx: &mut Context<Self>,
    ) {
        let resolved: Vec<(String, Resolved, bool)> = self
            .panes
            .iter()
            .filter(|(_, slot)| keep(slot))
            .map(|(pane_id, slot)| {
                let resolved = self.pane_appearance(&slot.tab_id, slot.session.as_deref());
                // A session pop-out's pane belongs to no tab; the window is
                // all its own, so it draws its band as a focused pane does.
                let focused = self.pane_place(pane_id).is_none_or(|(_, focused)| focused);
                (pane_id.clone(), resolved, focused)
            })
            .collect();
        let authored = theme(cx).terminal;
        for (pane_id, resolved, focused) in resolved {
            let Some(slot) = self.panes.get_mut(&pane_id) else {
                continue;
            };
            slot.accent = resolved.accent.value;
            // A background some level sets is a modifier on the theme and
            // gets the contrast nudge; with none the theme's own palette
            // is drawn as given.
            let override_bg = (resolved.background.source != Source::BuiltIn)
                .then(|| appearance::term_rgb(resolved.background.value));
            let palette = crate::theme::resolve_palette(override_bg, &authored);
            let band = frame_band(
                focused,
                resolved.frame.value.map(appearance::term_rgb),
                appearance::term_rgb(resolved.accent.value),
            );
            slot.view.update(cx, |pane, cx| {
                pane.set_font(resolved.font(), by_user, cx);
                pane.set_colors(palette, band, resolved.frame_width.value, cx);
            });
        }
    }

    /// The appearance a pane of `tab_id` showing `session` resolves to.
    fn pane_appearance(&self, tab_id: &str, session: Option<&str>) -> Resolved {
        self.sidebar
            .appearance(session)
            .with_tab_size(self.sidebar.tab_font_size(tab_id))
    }

    /// The colours pane `pane_id` paints on its edges: its border and its
    /// accent line.
    #[must_use]
    pub fn pane_frame_colors(&self, pane_id: &str, cx: &App) -> Option<PaneFrame> {
        let (session, focused) = self.pane_place(pane_id)?;
        Some(self.frame_colors(pane_id, session, focused, &theme(cx)))
    }

    /// The fill of pane `pane_id`'s header, `0xRRGGBB`: raised while the
    /// pane has its tab's focus, the surface otherwise.
    #[must_use]
    pub fn pane_header_fill(&self, pane_id: &str, cx: &App) -> Option<u32> {
        let (_, focused) = self.pane_place(pane_id)?;
        Some(header_fill(focused, &theme(cx)))
    }

    /// The two ends of pane `pane_id`'s header fade, `0xRRGGBB`: its accent
    /// mixed over the header's own fill at the left edge, and the plain fill
    /// where the wash ends, whichever pane has the focus.
    #[must_use]
    pub fn pane_header_fade(&self, pane_id: &str, cx: &App) -> Option<(u32, u32)> {
        let (session, focused) = self.pane_place(pane_id)?;
        let accent = self.panes.get(pane_id).map_or_else(
            || self.sidebar.appearance(session).accent.value,
            |slot| slot.accent,
        );
        let (from, to) = header_fade_colors(header_fill(focused, &theme(cx)), accent);
        Some((appearance::packed(from), appearance::packed(to)))
    }

    /// The session pane `pane_id` shows, and whether the pane has its
    /// tab's focus; `None` when no tab holds the pane.
    fn pane_place(&self, pane_id: &str) -> Option<(Option<&str>, bool)> {
        let (tab_id, session) = self.tabs.tabs().iter().find_map(|tab| {
            let pane = tabs::collect_panes(tab.grid()?)
                .into_iter()
                .find(|pane| pane.id == pane_id)?;
            Some((tab.id.as_str(), pane.session))
        })?;
        let focused = self.tabs.focused_pane(tab_id).as_deref() == Some(pane_id);
        Some((session, focused))
    }

    /// The colours pane `pane_id`, showing `session`, paints on its edges,
    /// from the accent its last apply resolved; a pane without a terminal
    /// yet resolves its session's.
    fn frame_colors(
        &self,
        pane_id: &str,
        session: Option<&str>,
        focused: bool,
        t: &UiTheme,
    ) -> PaneFrame {
        let accent = self.panes.get(pane_id).map_or_else(
            || self.sidebar.appearance(session).accent.value,
            |slot| slot.accent,
        );
        PaneFrame {
            border: if focused { accent } else { t.border },
            accent_line: accent,
        }
    }

    /// What pane `pane_id` fills, `0xRRGGBB`: the terminal's area, and the
    /// padding ring around it.
    #[must_use]
    pub fn pane_fills(&self, pane_id: &str, cx: &App) -> Option<(u32, u32)> {
        let (grid, ring) = self.panes.get(pane_id)?.view.read(cx).fills();
        Some((appearance::packed(grid), appearance::packed(ring)))
    }

    /// The width in px of the frame band pane `pane_id` draws.
    #[must_use]
    pub fn pane_frame_width(&self, pane_id: &str, cx: &App) -> Option<u16> {
        Some(self.panes.get(pane_id)?.view.read(cx).frame_width())
    }

    /// The background and default text colour pane `pane_id`'s terminal
    /// paints with, `0xRRGGBB`.
    #[must_use]
    pub fn pane_terminal_colors(&self, pane_id: &str, cx: &App) -> Option<(u32, u32)> {
        let (background, foreground) = self.panes.get(pane_id)?.view.read(cx).terminal_colors();
        Some((
            appearance::packed(background),
            appearance::packed(foreground),
        ))
    }

    /// The colour the text in visible cell (`row`, `col`) of pane
    /// `pane_id` paints in, `0xRRGGBB`; `None` for a cell with no text.
    #[must_use]
    pub fn pane_cell_foreground(
        &self,
        pane_id: &str,
        (row, col): (usize, usize),
        cx: &App,
    ) -> Option<u32> {
        let view = self.panes.get(pane_id)?.view.read(cx);
        view.cell_foreground(row, col).map(appearance::packed)
    }

    /// Terminal `slot_id`'s scrollback retry, sent once per session and
    /// attempt to the slot's host; its copy, which the root's clipboard
    /// write and chip answer; or its Ctrl+clicked link, which opens.
    pub(crate) fn on_pane_event(
        &mut self,
        slot_id: &str,
        event: &PaneEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = self.panes.get(slot_id).map(|slot| slot.host.clone());
        match event {
            PaneEvent::ScrollbackRetry {
                session_id,
                attempt,
                request_id,
            } => {
                let Some(host) = host else {
                    tracing::debug!(%slot_id, "a gone terminal's scrollback retry; not sent");
                    return;
                };
                // A remote host that said `Welcome` again and has not
                // listed yet attaches its panes afresh with that list.
                if host != HostKey::Local && !self.listed_since_welcome(&host) {
                    tracing::debug!(
                        %slot_id,
                        "the reconnected host has not listed its sessions; retry not sent"
                    );
                    return;
                }
                let session = SessionRef::new(host, session_id.clone());
                if let Some(request_id) =
                    self.retries
                        .claim(&session, request_id.as_deref(), *attempt)
                {
                    tracing::warn!(
                        "scrollback request for session {session_id} timed out; retry {attempt}"
                    );
                    self.request_scrollback(&session, request_id, cx);
                }
            }
            PaneEvent::Copied { text } => self.copy_to_clipboard(text, cx),
            PaneEvent::OpenLink {
                link,
                base_dirs,
                session,
                action,
            } => {
                let Some(host) = host else {
                    tracing::debug!(%slot_id, "a gone terminal's link click; not opened");
                    return;
                };
                let source = LinkSource {
                    host: &host,
                    session: session.as_deref(),
                    action: *action,
                };
                self.open_link(link, base_dirs.clone(), &source, window, cx);
            }
            PaneEvent::LinkMenu {
                link,
                base_dirs,
                session,
                at,
            } => {
                let Some(host) = host else {
                    tracing::debug!(%slot_id, "a gone terminal's link click; no menu");
                    return;
                };
                let menu = LinkMenu {
                    link: link.clone(),
                    base_dirs: base_dirs.clone(),
                    session: session.clone(),
                    host,
                    at: *at,
                    selected: 0,
                    max_height: window.viewport_size().height * MENU_MAX_SHARE,
                };
                // The press focuses the clicked pane, but its focus-in never
                // runs once the menu takes the keyboard, so the pane the click
                // was in takes it here and gets it back when the menu closes.
                self.pane_focused(slot_id, window, cx);
                self.open_link_menu(menu, window, cx);
            }
            PaneEvent::CheckSpacedLink(check) => {
                let Some(host) = host else {
                    return;
                };
                self.check_spaced_link(slot_id, &host, check.clone(), cx);
            }
            PaneEvent::ShellDotMenu { pane_id, index, at } => {
                self.open_shell_menu(pane_id, *index, *at, window, cx);
            }
            PaneEvent::PasteImage {
                session_id,
                name,
                bytes,
            } => self.on_paste_image(slot_id, session_id, name, bytes, cx),
        }
    }

    /// Attaches panes whose session their host's session list did not name
    /// before.
    pub(crate) fn attach_waiting_panes(&mut self, cx: &mut Context<Self>) {
        let waiting: BTreeSet<SessionRef> = self
            .panes
            .values()
            .filter(|slot| !slot.attached)
            .filter_map(PaneSlot::session_ref)
            .collect();
        for session in waiting {
            self.sync_session(&session, cx);
        }
    }

    /// This PC's new connection starts with nothing of its own attached;
    /// the next tab list attaches every pane of This PC again. A session
    /// pop-out's terminal, which no tab list names, keeps its session and
    /// attaches with the next session list. Its chats are loaded afresh too:
    /// each chat pane keeps its view and asks again once bound. A remote
    /// host's terminals stay as they are: their connection did not change.
    pub(crate) fn reset_panes(&mut self, cx: &mut Context<Self>) {
        self.chat.reset_connection(&HostKey::Local, cx);
        let local = self
            .panes
            .iter_mut()
            .filter(|(_, slot)| slot.host == HostKey::Local);
        for (pane_id, slot) in local {
            slot.view.update(cx, |pane, cx| {
                pane.reset_for_reconnect();
                cx.notify();
            });
            if popouts::slot_session(pane_id).is_none() {
                slot.unbound_from = slot.session.take();
            }
            slot.attached = false;
        }
    }

    /// `host`'s first session list on a new connection: every terminal
    /// showing one of its sessions, panes and session pop-outs alike,
    /// keeps its session and attaches again, on that connection, and its
    /// chats load afresh there.
    pub(crate) fn reset_host_panes(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        for slot in self.panes.values_mut().filter(|slot| slot.host == *host) {
            slot.view.update(cx, |pane, cx| {
                pane.reset_for_reconnect();
                cx.notify();
            });
            slot.attached = false;
        }
        self.chat.reset_connection(host, cx);
        self.sync_chat_loads(cx);
    }

    /// Gives `session`'s pop-out a terminal of its own, outside every grid,
    /// sending to the session's host, and attaches it as a pane attaches:
    /// scrollback first.
    pub(crate) fn open_session_slot(
        &mut self,
        session: &SessionRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let slot_id = popouts::session_slot_id(&session.host, &session.id);
        let mut slot = self.new_slot(&slot_id, &session.host, window, cx);
        slot.session = Some(session.id.clone());
        self.panes.insert(slot_id.clone(), slot);
        self.open_pane_view(&slot_id, Some(&session.id));
        self.apply_fonts_where(|slot| slot.shows(session), false, cx);
        self.sync_session(session, cx);
        self.sync_chat_loads(cx);
        self.update_pane_roles(cx);
    }

    /// Moves session `old`'s pop-out terminal to `new`, its restart on the
    /// same host or the same session on the host `old`'s host moved to:
    /// the same terminal, emptied, sending to `new`'s host and attached
    /// afresh to `new`.
    pub(crate) fn rekey_session_slot(
        &mut self,
        old: &SessionRef,
        new: &SessionRef,
        cx: &mut Context<Self>,
    ) {
        let old_id = popouts::session_slot_id(&old.host, &old.id);
        let Some(mut slot) = self.panes.remove(&old_id) else {
            return;
        };
        let new_id = popouts::session_slot_id(&new.host, &new.id);
        let sender = (slot.host != new.host).then(|| self.hosts.sender(&new.host));
        let remote = new.host != HostKey::Local;
        slot.view.update(cx, |pane, cx| {
            pane.release();
            pane.set_pane_id(new_id.clone());
            if let Some(sender) = sender {
                pane.set_sender(sender, remote);
            }
            cx.notify();
        });
        slot.host = new.host.clone();
        slot.session = Some(new.id.clone());
        slot.attached = false;
        self.panes.insert(new_id.clone(), slot);
        self.chat.rekey_pane(&old_id, &new_id);
        if self.popout_focused.as_deref() == Some(old_id.as_str()) {
            self.popout_focused = Some(new_id);
        }
        self.apply_fonts_where(|slot| slot.shows(new), false, cx);
        self.sync_session(new, cx);
        self.sync_chat_loads(cx);
    }

    /// Drops `session`'s pop-out terminal as its window closes; a session
    /// no other terminal shows is detached on its host while that host
    /// still lists it.
    pub(crate) fn drop_session_slot(&mut self, session: &SessionRef, cx: &mut Context<Self>) {
        let slot_id = popouts::session_slot_id(&session.host, &session.id);
        let Some(slot) = self.panes.remove(&slot_id) else {
            return;
        };
        slot.view.update(cx, |pane, _| pane.release());
        self.chat.forget_pane(&slot_id);
        self.sync_chat_loads(cx);
        if self.popout_focused.as_deref() == Some(slot_id.as_str()) {
            self.popout_focused = None;
        }
        let listed = self
            .sidebar
            .sessions_of(&session.host)
            .iter()
            .any(|s| s.id == session.id);
        if listed && self.pane_views_of(session).is_empty() {
            self.send_to(
                &session.host,
                ClientMessage::Detach {
                    session_id: session.id.clone(),
                },
            );
        }
    }

    /// The pane pop-out window `here` draws: its popped pane or session, or
    /// for a popped tab the tab's focused pane, else its first by id.
    pub(crate) fn window_pane(&self, here: WindowId) -> Option<String> {
        let mut drawn: Vec<(&String, &PaneSlot)> = self
            .panes
            .iter()
            .filter(|(pane_id, _)| self.slot_window(pane_id) == Some(here))
            .collect();
        drawn.sort_by(|a, b| a.0.cmp(b.0));
        let focused = drawn.iter().find(|(pane_id, slot)| {
            self.tabs.focused_pane(&slot.tab_id).as_deref() == Some(pane_id.as_str())
        });
        focused
            .or(drawn.first())
            .map(|(pane_id, _)| (*pane_id).clone())
    }

    /// The terminals showing `session`, host and id both.
    pub(crate) fn pane_views_of(&self, session: &SessionRef) -> Vec<Entity<TerminalPane>> {
        self.panes
            .values()
            .filter(|slot| slot.shows(session))
            .map(|slot| slot.view.clone())
            .collect()
    }

    /// The terminals showing a session of `host`.
    pub(crate) fn pane_views_on(&self, host: &HostKey) -> Vec<Entity<TerminalPane>> {
        self.panes
            .values()
            .filter(|slot| slot.host == *host)
            .map(|slot| slot.view.clone())
            .collect()
    }

    /// Hands a session's scrollback reply from `host` to every pane showing
    /// that host's session.
    pub(crate) fn feed_scrollback(
        &mut self,
        host: &HostKey,
        reply: &ScrollbackReply,
        cx: &mut Context<Self>,
    ) {
        let session = SessionRef::new(host.clone(), reply.session_id.clone());
        self.feed_panes(&session, cx, |pane, cx| pane.on_scrollback(reply, cx));
    }

    /// Hands live output for `session` to every pane showing it.
    pub(crate) fn feed_output(
        &mut self,
        session: &SessionRef,
        data_b64: &str,
        cx: &mut Context<Self>,
    ) {
        self.feed_panes(session, cx, |pane, cx| pane.on_pty_output(data_b64, cx));
    }

    /// Hands daemon output for `session` to every pane showing it.
    pub(crate) fn feed_panes(
        &mut self,
        session: &SessionRef,
        cx: &mut Context<Self>,
        mut feed: impl FnMut(
            &mut TerminalPane,
            &mut Context<TerminalPane>,
        ) -> Result<(), base64::DecodeError>,
    ) {
        for view in self.pane_views_of(session) {
            let fed = view.update(cx, |pane, cx| {
                cx.notify();
                feed(pane, cx)
            });
            if let Err(err) = fed {
                self.status = format!("bad base64 from daemon: {err}");
            }
        }
    }

    /// Whether a pane showing a session has the keyboard. A headless pane's
    /// hidden terminal can hold the focus but shows nothing, so the app's
    /// shortcuts stay live there.
    pub(crate) fn terminal_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.panes.values().any(|slot| {
            let Some(session) = slot
                .session
                .as_deref()
                .and_then(|id| self.sidebar.session(id))
            else {
                return false;
            };
            session.mode != SessionMode::Headless && slot.view.read(cx).is_focused(window)
        })
    }

    /// Gives `window`'s keyboard to `pane_id`'s terminal, unless the pane
    /// is open in a pop-out, or `window` is the main one and the pane's tab
    /// is popped out: its terminal draws in the pop-out, so the main window
    /// focuses nothing for it. A pane showing its chat gives its hidden
    /// terminal no keys: the window keeps the keyboard.
    pub(crate) fn focus_pane_view(
        &self,
        pane_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(slot) = self.panes.get(pane_id)
            && !self.is_pane_popped(pane_id)
            && !(self.is_tab_popped(&slot.tab_id) && self.is_main_window(window))
        {
            if self.pane_view(pane_id).hides_terminal() {
                self.root_focus.focus(window, cx);
            } else {
                slot.view.read(cx).focus_handle().focus(window, cx);
            }
        }
    }

    /// Gives the keyboard to the active tab: its focused pane, or a diff or
    /// viewer tab's body. A popped focused pane takes nothing; a popped tab's
    /// card holds no focus, so the main window keeps the keyboard to itself.
    /// With no tab or no pane to take it, the window's own focus does, so the
    /// app's shortcuts keep working.
    pub(crate) fn focus_active_pane(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_active_diff_tab(window, cx) || self.focus_active_viewer_tab(window, cx) {
            return;
        }
        let Some(tab_id) = self.tabs.active_id() else {
            self.root_focus.focus(window, cx);
            return;
        };
        if self.is_tab_popped(tab_id) && self.is_main_window(window) {
            self.root_focus.focus(window, cx);
            return;
        }
        match self.tabs.focused_pane(tab_id) {
            Some(pane_id) => self.focus_pane_view(&pane_id, window, cx),
            None => self.root_focus.focus(window, cx),
        }
    }

    /// The session the main window shows focused, for the unseen-turn rule:
    /// the active tab's focused pane's, unless that pane or its tab draws in
    /// a pop-out.
    pub(crate) fn shown_session(&self) -> Option<String> {
        let tab = self.tabs.active_tab()?;
        let pane_id = self.tabs.focused_pane(&tab.id)?;
        if self.is_pane_popped(&pane_id) || self.is_tab_popped(&tab.id) {
            return None;
        }
        self.focused_session()
    }

    /// The session of the active tab's focused pane.
    pub(crate) fn focused_session(&self) -> Option<String> {
        let tab = self.tabs.active_tab()?;
        let pane_id = self.tabs.focused_pane(&tab.id)?;
        tabs::collect_panes(tab.grid()?)
            .into_iter()
            .find(|pane| pane.id == pane_id)?
            .session
            .map(str::to_owned)
    }

    pub(crate) fn split_pane(
        &mut self,
        tab_id: &str,
        pane_id: &str,
        direction: SplitDirection,
        before: bool,
    ) {
        let msg = ClientMessage::SplitPane {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
            host: None,
            direction,
            place: if before {
                SplitPlace::First
            } else {
                SplitPlace::Second
            },
            touch: true,
            new_session_id: None,
            request_id: Some(crate::new_request_id()),
            follow: None,
        };
        self.tabs.note_layout_request(&msg);
        self.send_to(&HostKey::Local, msg);
    }

    /// Whether any connected host has a repo registered, which a repo-tied
    /// spawn needs.
    #[must_use]
    pub fn has_repos(&self) -> bool {
        self.held_hosts()
            .iter()
            .any(|host| !self.is_remote_offline(host) && !self.sidebar.repos_of(host).is_empty())
    }

    /// The panes of the tab on screen that show nothing: no session and no
    /// follow.
    #[must_use]
    pub fn empty_pane_ids(&self) -> Vec<String> {
        self.active_panes(|pane| !pane.is_bound())
    }

    /// Whether the main area with no tab open offers its spawn choices: only
    /// once the connection is open and the tab list has arrived, and This
    /// PC's repo list has too or another host already holds a repo, so a
    /// repo-less hint never flashes before the repos do.
    #[must_use]
    pub fn no_tab_choices_shown(&self) -> bool {
        self.tabs.active_tab().is_none()
            && self.conn().is_open()
            && self.tabs.is_loaded()
            && (self.sidebar.repos_loaded() || self.has_repos())
    }

    fn active_panes(&self, keep: impl Fn(&tabs::Pane<'_>) -> bool) -> Vec<String> {
        self.tabs
            .active_tab()
            .and_then(TabEntry::grid)
            .map(|grid| {
                tabs::collect_panes(grid)
                    .into_iter()
                    .filter(|pane| keep(pane))
                    .map(|pane| pane.id.to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Gives `tab_id`'s focus to `pane_id` and applies every pane's colours
    /// again: the band follows the focus even when the terminal never takes
    /// the keyboard, as the spawn dialog opening over it leaves it.
    pub(crate) fn set_focused_pane(&mut self, tab_id: &str, pane_id: &str, cx: &mut Context<Self>) {
        self.tabs.set_focused(tab_id, pane_id);
        self.apply_pane_fonts(false, cx);
    }

    /// Gives pane `pane_id` of `tab_id` its tab's focus and the keyboard.
    fn focus_pane_in(
        &mut self,
        tab_id: &str,
        pane_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_focused_pane(tab_id, pane_id, cx);
        self.focus_history.record(tab_id, Some(pane_id));
        self.preselected_target = None;
        self.remember_focused_session();
        self.focus_pane_view(pane_id, window, cx);
    }

    /// A pane's "New session…": focuses the pane and opens the spawn dialog
    /// aimed at it. Over a stopped session (`replacing`) the dialog starts
    /// on that session's repo or workspace and the session is discarded
    /// once the new one takes the pane; in an empty pane it starts on the
    /// split sibling's. Nothing while a spawn aimed at the pane is on its
    /// way.
    pub(crate) fn new_session_in_pane(
        &mut self,
        tab_id: &str,
        pane_id: &str,
        replacing: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.spawns.aims_at(tab_id, pane_id) {
            return;
        }
        self.focus_pane_in(tab_id, pane_id, window, cx);
        let preselect = match replacing {
            Some(id) => Some(id.to_owned()),
            None => self
                .tabs
                .tab(tab_id)
                .and_then(TabEntry::grid)
                .and_then(|grid| tabs::split_sibling_session(grid, pane_id))
                .map(str::to_owned),
        };
        let replacing_ref =
            replacing.map(|id| tabs::SessionRef::new(self.tabs.pane_host(tab_id, pane_id), id));
        let aim = PaneAim {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
            expected: replacing_ref.clone(),
            discard: replacing_ref,
        };
        self.open_spawn_dialog(SpawnEntry::Pane { aim, preselect }, window, cx);
        cx.notify();
    }

    /// An empty pane's "Shell here": the quick shell, into this pane, on
    /// the pane's own host when it is bound; a pane that shows nothing takes
    /// the host of the focused session, else This PC (an empty pane focused
    /// is no focused session). Nothing while a spawn aimed at the pane is on
    /// its way.
    fn shell_in_pane(
        &mut self,
        tab_id: &str,
        pane_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.spawns.aims_at(tab_id, pane_id) {
            return;
        }
        let bound = self
            .tabs
            .tab(tab_id)
            .and_then(TabEntry::grid)
            .and_then(|grid| {
                tabs::collect_panes(grid)
                    .into_iter()
                    .find(|pane| pane.id == pane_id)
            })
            .is_some_and(|pane| pane.is_bound());
        let host = if bound {
            self.tabs.pane_host(tab_id, pane_id)
        } else {
            self.focused_host()
        };
        self.focus_pane_in(tab_id, pane_id, window, cx);
        let cwd = self.quick_shell_default(&host).map(str::to_owned);
        let open_in = OpenIn::Pane(PaneAim {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
            expected: None,
            discard: None,
        });
        self.spawn_on(&host, standalone_shell_request(cwd), open_in, cx);
        cx.notify();
    }

    /// The main area with no tab open: "Spawn a session" and "Open shell",
    /// and with no repo registered, "Add repo" and why the first is
    /// disabled. Before the
    /// connection, the tabs and the repos are in, only "No tab open".
    fn no_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = theme(cx);
        if !self.no_tab_choices_shown() {
            return muted_note(crate::fonts::ui_text_size(cx), "No tab open", &t)
                .into_any_element();
        }
        let has_repos = self.has_repos();
        let spawn = BorderedButton {
            selector: "empty-spawn-session".to_owned(),
            label: "Spawn a session",
            tip: if has_repos { SPAWN_TIP } else { NO_REPOS_TIP },
            enabled: has_repos,
        };
        let spawn = bordered_button(spawn, cx, |this, window, cx| {
            this.open_spawn_dialog(SpawnEntry::Toolbar, window, cx);
        });
        let shell = BorderedButton {
            selector: "empty-open-shell".to_owned(),
            label: "Open shell",
            tip: OPEN_SHELL_TIP,
            enabled: true,
        };
        let shell = bordered_button(shell, cx, |this, _, cx| this.quick_shell(cx));
        let mut buttons = vec![spawn, shell];
        if !has_repos {
            let gate = self.add_repo_gate();
            let add = BorderedButton {
                selector: "empty-add-repo".to_owned(),
                label: "Add repo",
                tip: gate.tip,
                enabled: gate.enabled,
            };
            buttons.push(bordered_button(add, cx, |this, window, cx| {
                this.add_repo(window, cx);
            }));
        }
        spawn_choices(crate::fonts::ui_text_size(cx), "No tab open", buttons, &t)
            .when(!has_repos, |note| {
                note.child(
                    div()
                        .driver_id(|| "empty-repo-hint".to_owned())
                        .child(NO_REPOS_TIP),
                )
            })
            .into_any_element()
    }

    /// An empty pane: "No session" over "New session…" and "Shell here". A
    /// press on it gives the pane the keyboard.
    fn empty_pane(
        &self,
        tab_id: &str,
        pane_id: &str,
        focus: Option<FocusHandle>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let has_repos = self.has_repos();
        let pending = self.spawns.aims_at(tab_id, pane_id);
        let refused = self.pane_spawn_refusal(tab_id);
        let spawn = BorderedButton {
            selector: format!("empty-pane-new-session-{pane_id}"),
            label: "New session…",
            tip: if let Some(refusal) = refused {
                refusal
            } else if !has_repos {
                NO_REPOS_TIP
            } else if pending {
                PANE_PENDING_TIP
            } else {
                PANE_SPAWN_TIP
            },
            enabled: refused.is_none() && has_repos && !pending,
        };
        let ids = (tab_id.to_owned(), pane_id.to_owned());
        let spawn = bordered_button(spawn, cx, move |this, window, cx| {
            this.new_session_in_pane(&ids.0, &ids.1, None, window, cx);
        });
        let shell = BorderedButton {
            selector: format!("empty-pane-shell-{pane_id}"),
            label: "Shell here",
            tip: if let Some(refusal) = refused {
                refusal
            } else if pending {
                PANE_PENDING_TIP
            } else {
                PANE_SHELL_TIP
            },
            enabled: refused.is_none() && !pending,
        };
        let ids = (tab_id.to_owned(), pane_id.to_owned());
        let shell = bordered_button(shell, cx, move |this, window, cx| {
            this.shell_in_pane(&ids.0, &ids.1, window, cx);
        });
        let ids = (tab_id.to_owned(), pane_id.to_owned());
        let body = spawn_choices(
            crate::fonts::ui_text_size(cx),
            "No session",
            [spawn, shell],
            &t,
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                this.open_empty_pane_menu(&ids.0, &ids.1, event.position, window, cx);
                cx.stop_propagation();
            }),
        );
        match focus {
            Some(handle) => body
                .track_focus(&handle)
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    handle.focus(window, cx);
                })
                .into_any_element(),
            None => body.into_any_element(),
        }
    }

    /// The active tab: its split tree, a diff or viewer tab's body, the card
    /// a popped-out tab leaves, or the spawn choices when there is no tab.
    pub(crate) fn grid_area(&self, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        let content = match self.tabs.active_tab() {
            None => self.no_tab(cx),
            Some(tab) => match &tab.content {
                TabContent::Grid { .. } | TabContent::Diff { .. }
                    if self.is_tab_popped(&tab.id) =>
                {
                    Self::popped_tab_card(tab, self.sidebar.tab_display_name(tab), cx)
                }
                TabContent::Diff { .. } => self.diff_tab_element(&tab.id).unwrap_or_else(|| {
                    muted_note(crate::fonts::ui_text_size(cx), DIFF_LOADING_TEXT, &t)
                        .into_any_element()
                }),
                TabContent::Viewer { .. } => self
                    .viewer_tab_element(&tab.id, cx)
                    .unwrap_or_else(|| div().into_any_element()),
                TabContent::Grid { grid } => self.tab_grid(&tab.id, grid, self.layouts.main(), cx),
                TabContent::Dashboard => self.dashboard_tab_element(cx),
                TabContent::Unknown => muted_note(
                    crate::fonts::ui_text_size(cx),
                    "This tab needs a newer app.",
                    &t,
                )
                .into_any_element(),
            },
        };
        grid_frame(content, cx)
    }

    /// `tab_id`'s split tree as window `here` draws it, with the layer that
    /// takes a pane or leaf drop on it.
    fn tab_grid(
        &self,
        tab_id: &str,
        grid: &GridNode,
        here: WindowId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = format!("tab-grid-{tab_id}");
        div()
            .driver_id(|| name)
            .relative()
            .flex()
            .size_full()
            .p(px(PANE_GUTTER))
            .child(self.render_node(tab_id, grid, &mut Vec::new(), here, cx))
            .child(drop_layer(tab_id, cx))
            .into_any_element()
    }

    fn render_node(
        &self,
        tab_id: &str,
        node: &GridNode,
        path: &mut Vec<u8>,
        here: WindowId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            GridNode::Pane {
                pane_id,
                session_id,
                follow,
                ..
            } => {
                let shown = PaneShown {
                    session: session_id.as_deref(),
                    follow: follow.as_ref(),
                };
                self.render_pane(tab_id, pane_id, shown, here, cx)
            }
            GridNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                path.push(0);
                let first = self.render_node(tab_id, first, path, here, cx);
                path.pop();
                path.push(1);
                let second = self.render_node(tab_id, second, path, here, cx);
                path.pop();
                let divider = self.pane_divider(tab_id, path, *direction, here, cx);
                split_box(*direction, *ratio, [first, divider, second])
            }
        }
    }

    /// Whether the divider at `split_path` of `tab_id` shows its drag
    /// highlight in window `here`: only while a drag that started in that
    /// window holds it.
    #[must_use]
    pub fn divider_highlighted(&self, tab_id: &str, split_path: &[u8], here: WindowId) -> bool {
        matches!(
            &self.drag,
            Some(Drag::Divider { window, tab_id: t, split_path: p, .. })
                if *window == here && t == tab_id && p == split_path
        )
    }

    fn pane_divider(
        &self,
        tab_id: &str,
        path: &[u8],
        direction: SplitDirection,
        here: WindowId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let active = self.divider_highlighted(tab_id, path, here);
        let name = format!("divider-{tab_id}-{path:?}");
        let id = ElementId::Name(SharedString::from(name.clone()));
        let (tab_id, split_path) = (tab_id.to_owned(), path.to_vec());
        gutter_handle(id, direction == SplitDirection::Horizontal, active, &t)
            .driver_id(|| name)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.drag = Some(Drag::Divider {
                        window: window.window_handle().window_id(),
                        tab_id: tab_id.clone(),
                        split_path: split_path.clone(),
                        origin: (event.position.x / px(1.0), event.position.y / px(1.0)),
                        ratio: None,
                    });
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }

    fn render_pane(
        &self,
        tab_id: &str,
        pane_id: &str,
        shown: PaneShown<'_>,
        here: WindowId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let PaneShown { session, .. } = shown;
        if let Some(key) = self.popouts.popped_key(pane_id).cloned() {
            // One terminal draws in one window: the pop-out's.
            return self.popped_card(key, pane_id, session, cx);
        }
        let focused = self.tabs.focused_pane(tab_id).as_deref() == Some(pane_id);
        let body = self.pane_body(tab_id, pane_id, shown, cx);
        let host = self.tabs.pane_host(tab_id, pane_id);
        let body = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .capture_any_mouse_down(cx.listener({
                let pane_id = pane_id.to_owned();
                move |this, event: &MouseDownEvent, window, cx| {
                    this.focus_inactive_pane(&pane_id, event, window, cx);
                }
            }))
            .children(self.orphan_banner(
                crate::fonts::ui_text_size(cx),
                pane_id,
                &host,
                session,
                &theme(cx),
            ))
            .child(body)
            .children(self.exited_overlay(
                Some(tab_id),
                pane_id,
                session.map(|id| SessionRef::new(self.tabs.pane_host(tab_id, pane_id), id)),
                cx,
            ))
            .children(self.abandoned_overlay(pane_id, session, cx));
        let colors = self.frame_colors(pane_id, session, focused, &t);
        let overlay = self
            .pane_drop_overlay(cx)
            .filter(|(target, _)| target == pane_id)
            .map(|(_, edge)| drop_overlay(pane_id, edge, colors.accent_line, cx, &t));
        let frame = format!("pane-frame-{pane_id}");
        let parts = session
            .and_then(|_| self.pane_session_in(tab_id, pane_id))
            .map(|(host, session)| self.session_header_parts(&host, session));
        let target = self.header_target(tab_id, pane_id, session, parts.as_ref(), here);
        let header_ground = header_fill(focused, &t);
        let (fade_from, fade_to) = header_fade_stops(header_ground, colors.accent_line);
        let header = self
            .pane_header(&target, colors.accent_line, cx)
            .bg(gpui::linear_gradient(90.0, fade_from, fade_to))
            .child(
                div()
                    .absolute()
                    .top(px(PANE_RADIUS))
                    .bottom_0()
                    .left_0()
                    .w(px(ACCENT_LINE_WIDTH))
                    .bg(gpui::rgb(colors.accent_line)),
            );
        div()
            .driver_id(|| frame)
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .rounded(px(PANE_RADIUS))
            .border_1()
            .border_color(gpui::rgb(colors.border))
            .child(header)
            .child(body)
            .children(overlay)
            .into_any_element()
    }

    /// Tells the pressed pane whether it is the tab's focused one, then, with
    /// the General setting off, drops what any press drops, closes an open
    /// jump palette (its own outside-click listener never hears a press
    /// stopped here) and gives the pane its tab's focus and keyboard,
    /// stopping the press ahead of any pane body's own handler: one click's
    /// focus and a closed palette, nothing else.
    fn focus_inactive_pane(
        &mut self,
        pane_id: &str,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_id) = self.panes.get(pane_id).map(|slot| slot.tab_id.clone()) else {
            return;
        };
        let inactive = self.tabs.focused_pane(&tab_id).as_deref() != Some(pane_id);
        if let Some(slot) = self.panes.get(pane_id) {
            slot.view
                .update(cx, |pane, _| pane.note_press_on_inactive_pane(inactive));
        }
        if ClickThrough::get(cx) || !inactive {
            return;
        }
        self.on_any_mouse_down(event, window, cx);
        self.close_quick_jump(window, cx);
        self.focus_pane_in(&tab_id, pane_id, window, cx);
        cx.stop_propagation();
    }

    /// What pane `pane_id` of `tab_id` draws as its body: its session's
    /// terminal or note, a note for what the pane follows instead, or an
    /// empty pane's spawn choices.
    fn pane_body(
        &self,
        tab_id: &str,
        pane_id: &str,
        shown: PaneShown<'_>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let PaneShown { session, follow } = shown;
        if let (None, Some(follow)) = (session, follow) {
            let note = match follow {
                PaneFollow::DispatchRun { run_id } => {
                    let host = self.tabs.pane_host(tab_id, pane_id);
                    self.dispatch_body(pane_id, &host, run_id, cx)
                }
                PaneFollow::Subagent {
                    session_id,
                    agent_id,
                } => {
                    let host = self.tabs.pane_host(tab_id, pane_id);
                    let key = crate::transcript_view::TranscriptKey::Subagent {
                        session_id: session_id.clone(),
                        agent_id: agent_id.clone(),
                    };
                    self.subagent_body(pane_id, &host, &key, cx)
                }
                PaneFollow::RunSubagent { run_id, agent_id } => {
                    let host = self.tabs.pane_host(tab_id, pane_id);
                    let key = crate::transcript_view::TranscriptKey::RunSubagent {
                        run_id: run_id.clone(),
                        agent_id: agent_id.clone(),
                    };
                    self.subagent_body(pane_id, &host, &key, cx)
                }
                PaneFollow::Unknown => follow_placeholder(pane_id, cx),
            };
            return match self.panes.get(pane_id) {
                Some(slot) => {
                    let handle = slot.view.read(cx).focus_handle();
                    note.track_focus(&handle)
                        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                            handle.focus(window, cx);
                        })
                        .into_any_element()
                }
                None => note.into_any_element(),
            };
        }
        match (session, self.panes.get(pane_id)) {
            (Some(id), Some(slot)) if slot.attached => {
                let terminal = div()
                    .flex_1()
                    .min_h(px(0.0))
                    .driver_id(|| format!("pane-grid-{pane_id}"))
                    .child(slot.view.clone());
                self.attached_body(pane_id, id, terminal, cx)
            }
            (Some(id), slot) => self.note_body(pane_id, id, slot, cx),
            (None, Some(slot)) => {
                let handle = slot.view.read(cx).focus_handle();
                self.empty_pane(tab_id, pane_id, Some(handle), cx)
            }
            (None, None) => self.empty_pane(tab_id, pane_id, None, cx),
        }
    }

    /// The card a popped pane leaves in its place in the main grid: its
    /// session's label, the popped-out note, Focus window and Dock back. It
    /// holds no focus, so keys typed while it shows go nowhere. `key` is the
    /// pop-out showing the pane, whichever tab it names.
    fn popped_card(
        &self,
        key: PopoutKey,
        pane_id: &str,
        session_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        // The card names the session its pane shows, on the pane's host:
        // a session id two hosts share must not borrow the other's rows.
        let host = match &key {
            PopoutKey::Pane { tab_id, .. } => self.tabs.pane_host(tab_id, pane_id),
            other => other.host(),
        };
        let label = session_id
            .and_then(|id| self.hosted_session(&host, id))
            .map_or_else(
                || drag::EMPTY_PANE_LABEL.to_owned(),
                |session| display_label(session, self.sidebar.label_agents(&host)),
            );
        let focus_key = key.clone();
        let focus = BorderedButton {
            selector: format!("popped-focus-{pane_id}"),
            label: "Focus window",
            tip: FOCUS_WINDOW_TIP,
            enabled: true,
        };
        let focus = bordered_button(focus, cx, move |this, window, cx| {
            this.open_popout(focus_key.clone(), window, cx);
        });
        let dock = BorderedButton {
            selector: format!("popped-dock-{pane_id}"),
            label: "Dock back",
            tip: DOCK_BACK_TIP,
            enabled: true,
        };
        let dock = bordered_button(dock, cx, move |this, _, cx| this.dock_back(&key, cx));
        let names = CardNames {
            card: format!("pane-popped-{pane_id}"),
            label: format!("popped-label-{pane_id}"),
        };
        popped_out_card(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            names,
            label,
            [focus, dock],
            &t,
        )
    }

    /// The card a popped-out tab leaves in the main window in place of its
    /// grid: the tab's name, the popped-out note, Focus window and Dock
    /// back. It holds no focus, so keys typed while it shows go nowhere.
    /// `name` is the name the tab shows.
    fn popped_tab_card(tab: &TabEntry, name: String, cx: &mut Context<Self>) -> AnyElement {
        let t = theme(cx);
        let key = PopoutKey::Tab {
            tab_id: tab.id.clone(),
        };
        let focus_key = key.clone();
        let focus = BorderedButton {
            selector: format!("popped-tab-focus-{}", tab.id),
            label: "Focus window",
            tip: FOCUS_WINDOW_TIP,
            enabled: true,
        };
        let focus = bordered_button(focus, cx, move |this, window, cx| {
            this.open_popout(focus_key.clone(), window, cx);
        });
        let dock = BorderedButton {
            selector: format!("popped-tab-dock-{}", tab.id),
            label: "Dock back",
            tip: TAB_DOCK_BACK_TIP,
            enabled: true,
        };
        let dock = bordered_button(dock, cx, move |this, _, cx| this.dock_back(&key, cx));
        let names = CardNames {
            card: format!("popped-tab-{}", tab.id),
            label: format!("popped-tab-label-{}", tab.id),
        };
        popped_out_card(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            names,
            name,
            [focus, dock],
            &t,
        )
    }

    /// Why an empty pane of `tab_id` offers no spawn: a popped-out tab
    /// spawns from the main window only.
    #[must_use]
    pub fn pane_spawn_refusal(&self, tab_id: &str) -> Option<&'static str> {
        self.is_tab_popped(tab_id).then_some(SPAWN_FROM_MAIN_TIP)
    }

    /// What `key`'s pop-out window `here` draws: a pane's or a session's
    /// toolbar over its terminal, or a tab's toolbar over its grid. `None`
    /// while a pane or tab pop-out has nothing to show.
    pub(crate) fn popout_pane(
        &self,
        key: &PopoutKey,
        here: WindowId,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        match key {
            PopoutKey::Pane { tab_id, pane_id } => self.popped_pane(key, tab_id, pane_id, cx),
            PopoutKey::Session { session_id, .. } => {
                Some(self.popped_session(&key.host(), session_id, cx))
            }
            PopoutKey::Tab { tab_id } => self.popped_tab(tab_id, here, cx),
        }
    }

    /// What tab `tab_id`'s pop-out window `here` draws: a toolbar with the
    /// tab's name and Dock back over the tab's whole grid, or over a diff
    /// tab's diff (its loading note before it has a view), with no tab
    /// strip. `None` once the tab has gone, or for a viewer tab.
    fn popped_tab(
        &self,
        tab_id: &str,
        here: WindowId,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let t = theme(cx);
        let tab = self.tabs.tab(tab_id)?;
        let content = match &tab.content {
            TabContent::Grid { grid } => self.tab_grid(tab_id, grid, here, cx),
            TabContent::Diff { .. } => self.diff_tab_element(tab_id).unwrap_or_else(|| {
                muted_note(crate::fonts::ui_text_size(cx), DIFF_LOADING_TEXT, &t).into_any_element()
            }),
            TabContent::Viewer { .. } | TabContent::Dashboard | TabContent::Unknown => return None,
        };
        let name = format!("popout-tab-title-{tab_id}");
        let title = div()
            .driver_id(|| name)
            .flex_none()
            .whitespace_nowrap()
            .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
            .text_color(gpui::rgb(t.text))
            .child(self.sidebar.tab_display_name(tab))
            .into_any_element();
        let dock = Self::popout_tab_dock_back(tab_id, cx);
        let toolbar = popout_toolbar_row(&format!("tab-{tab_id}"), vec![title], vec![dock], cx);
        let body = grid_frame(content, cx);
        Some(popout_column(toolbar, body.into_any_element()))
    }

    /// What a session pop-out draws: its toolbar over its terminal, with the
    /// note, the banner and the overlays a pane would draw for it; once the
    /// daemon has dropped the session, the not-found note under a toolbar
    /// holding only Close window.
    fn popped_session(
        &self,
        host: &HostKey,
        session_id: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let slot_id = popouts::session_slot_id(host, session_id);
        let Some(session) = self
            .sidebar
            .sessions_of(host)
            .iter()
            .find(|s| s.id == session_id)
        else {
            let toolbar = popout_toolbar_row(
                &slot_id,
                Vec::new(),
                vec![Self::popout_close_window(&slot_id, cx)],
                cx,
            );
            let name = format!("popout-not-found-{slot_id}");
            let note = muted_note(
                crate::fonts::ui_text_size(cx),
                popouts::SESSION_NOT_FOUND,
                &t,
            )
            .driver_id(|| name);
            return popout_column(toolbar, note.into_any_element());
        };
        let parts = self.session_header_parts(host, session);
        let title = header_title(&slot_id, PaneTitle::Session(&parts), TipAt::Mouse, true, cx);
        let mut controls = self.header_stop(&slot_id, Some(session_id), StopPlace::Popout, cx);
        controls.push(Self::popout_close_window(&slot_id, cx));
        let toolbar = popout_toolbar_row(&slot_id, title, controls, cx);
        let slot = self.panes.get(&slot_id);
        let body = match slot {
            Some(slot) if slot.attached => {
                let terminal = div()
                    .flex_1()
                    .min_h(px(0.0))
                    .size_full()
                    .child(slot.view.clone());
                self.attached_body(&slot_id, session_id, terminal, cx)
            }
            slot => self.note_body(&slot_id, session_id, slot, cx),
        };
        let body = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .children(self.orphan_banner(
                crate::fonts::ui_text_size(cx),
                &slot_id,
                host,
                Some(session_id),
                &theme(cx),
            ))
            .child(body)
            .children(self.exited_overlay(
                None,
                &slot_id,
                Some(SessionRef::new(host.clone(), session_id)),
                cx,
            ))
            .children(self.abandoned_overlay(&slot_id, Some(session_id), cx));
        popout_column(toolbar, body.into_any_element())
    }

    /// What pane `pane_id`'s pop-out window draws: its toolbar over its
    /// terminal, with the note, the banner and the overlays the main grid
    /// would draw for the pane. `None` while the pop-out has no pane to show.
    fn popped_pane(
        &self,
        key: &PopoutKey,
        tab_id: &str,
        pane_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let terminal = self.popout_terminal(key)?;
        let session_id = popouts::pane_in_tabs(pane_id, self.tabs.tabs())
            .and_then(|pane| pane.session)
            .map(str::to_owned);
        let session_id = session_id.as_deref();
        // As the main grid draws the pane: the terminal once attached, else
        // the note for its session, gone or not yet attached.
        let body = match (session_id, self.panes.get(pane_id)) {
            (Some(id), Some(slot)) if slot.attached => {
                let terminal = div().flex_1().min_h(px(0.0)).size_full().child(terminal);
                self.attached_body(pane_id, id, terminal, cx)
            }
            (Some(id), slot) => self.note_body(pane_id, id, slot, cx),
            (None, _) => div()
                .flex_1()
                .min_h(px(0.0))
                .size_full()
                .child(terminal)
                .into_any_element(),
        };
        let host = self.tabs.pane_host(tab_id, pane_id);
        let body = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .children(self.orphan_banner(
                crate::fonts::ui_text_size(cx),
                pane_id,
                &host,
                session_id,
                &theme(cx),
            ))
            .child(body)
            .children(self.exited_overlay(
                Some(tab_id),
                pane_id,
                session_id.map(|id| SessionRef::new(self.tabs.pane_host(tab_id, pane_id), id)),
                cx,
            ))
            .children(self.abandoned_overlay(pane_id, session_id, cx));
        let toolbar = self.popout_toolbar(pane_id, session_id, cx);
        Some(popout_column(toolbar, body.into_any_element()))
    }

    /// A pane pop-out's toolbar: the pane header's status glyph and name,
    /// the tab the pane is docked in, the header's chips, then Stop and
    /// Dock back. It has no split, close or Pop out.
    fn popout_toolbar(
        &self,
        pane_id: &str,
        session_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let parts = self.popout_header_parts(pane_id);
        let mut title = header_title(
            pane_id,
            pane_title(parts.as_ref(), None, None),
            TipAt::Mouse,
            true,
            cx,
        );
        if let Some(docked) = self.popout_docked_in(pane_id) {
            let name = format!("popout-tab-{pane_id}");
            let docked = div()
                .driver_id(|| name)
                .flex_none()
                .whitespace_nowrap()
                .text_color(gpui::rgb(t.muted))
                .child(docked)
                .into_any_element();
            // After the glyph and the name, before the chips.
            title.insert(title.len().min(2), docked);
        }
        let mut controls = self.header_stop(pane_id, session_id, StopPlace::Popout, cx);
        controls.push(Self::popout_dock_back(pane_id, cx));
        popout_toolbar_row(pane_id, title, controls, cx)
    }

    /// The session's status glyph, name and chips (runtime, repo:branch
    /// with a count of further members), its Pop out, its Stop or
    /// exit code, split right and down (Shift: left and up), move and
    /// close. When the header in `target`'s window is too narrow for those
    /// buttons beside a readable name, everything from Pop out to the
    /// prompt arrows gives way to one ⋯ button opening a menu of them,
    /// Close staying last and a stopped session's exit code beside ⋯. A
    /// right-click opens the session's menu; a left press dragged anywhere
    /// but on a button or a chip drags the pane, its ghost edged in
    /// `accent`.
    fn pane_header(
        &self,
        target: &HeaderTarget<'_>,
        accent: u32,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let t = theme(cx);
        let HeaderTarget {
            tab_id,
            pane_id,
            session_id,
            parts,
            follow,
            at,
            ..
        } = *target;
        let menu_ids = (tab_id.to_owned(), pane_id.to_owned());
        let menu_session = session_id.map(str::to_owned);
        let name = format!("pane-header-{pane_id}");
        let named = pane_title(parts, follow, target.run);
        let label: SharedString = drag::ghost_label(named.label()).into();
        let payload = DragPayload::Pane {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
        };
        let root = cx.entity().downgrade();
        let drag_session = session_id.map(str::to_owned);
        let (controls, buttons) = self.header_right(target, cx);
        let floors = self.chip_floors_fit(target, cx);
        let controls = (!controls.is_empty()).then(|| header_controls(cx).children(controls));
        div()
            .id(ElementId::Name(SharedString::from(name.clone())))
            .driver_id(|| name)
            .on_drag(payload, move |payload, offset, _, cx| {
                let drag_session = drag_session.clone();
                if let Err(err) = root.update(cx, |this, _| {
                    this.pane_drag = Some(payload.clone());
                    this.pane_drag_session = drag_session;
                }) {
                    tracing::warn!(%err, "pane drag: the window's root view is gone");
                }
                let label = label.clone();
                cx.new(|_| PaneGhost {
                    label,
                    accent,
                    offset,
                })
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if let Some(id) = &menu_session {
                        let host = this.tabs.pane_host(&menu_ids.0, &menu_ids.1);
                        this.open_session_menu(
                            &host,
                            id,
                            Some(&menu_ids),
                            event.position,
                            window,
                            cx,
                        );
                    } else {
                        let (tab, pane) = &menu_ids;
                        this.open_empty_pane_menu(tab, pane, event.position, window, cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.0))
            .h(ui_px(cx, PANE_HEADER_HEIGHT))
            .pl(px(12.0))
            .pr(px(8.0))
            .rounded_t(px(PANE_RADIUS))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .text_size(ui_px(cx, UI_TEXT_SIZE))
            .text_color(gpui::rgb(t.text_2))
            .when(at == TipAt::BelowRow, |row| {
                row.child(header_row_probe(pane_id))
            })
            .children(header_title(pane_id, named, at, floors, cx))
            .child(div().flex_1())
            .children(controls)
            .children(buttons)
            // After every element of the row is built: it reads what they
            // recorded.
            .children(
                (at == TipAt::BelowRow)
                    .then(|| self.header_tip_below(pane_id, cx))
                    .flatten(),
            )
    }

    /// The tip of the header element under the mouse, when it shows and this
    /// render built the element, with that tip, in the header row of `row`:
    /// a box whose top is the row's bottom edge plus 2 px, its left the
    /// mouse's x when the hover began, kept 8 px inside the window and
    /// painted above everything else in it.
    fn header_tip_below(&self, row: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tip = self.header_tips.shown()?;
        let frame = cx.try_global::<HeaderTipFrame>()?;
        if frame.looking_for.as_deref() != Some(tip.owner.as_str()) {
            return None;
        }
        frame
            .built
            .as_ref()
            .filter(|built| built.row == row && built.text == tip.text)?;
        let header = *frame.rows.get(row)?;
        let at = point(px(tip.x), header.bottom() + px(2.0));
        let tip_box = tip_box(SharedString::from(tip.text.clone()), cx)
            .driver_id(|| "pane-header-tip".to_owned());
        Some(
            deferred(
                anchored()
                    .position(at)
                    .snap_to_window_with_margin(px(8.0))
                    .child(tip_box),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    /// The mouse entered header element `owner`, whose tip is `text`: its
    /// tip shows after the hover delay, unless the hover ends first.
    fn hover_header_tip(
        &mut self,
        owner: &str,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A drag under way passes over header elements; none of them tips.
        if cx.has_active_drag() {
            return;
        }
        let x = window.mouse_position().x / px(1.0);
        self.header_tips.hover(owner, text, x, (self.now)());
        self.header_tip_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HEADER_TIP_DELAY).await;
            if let Err(err) = this.update(cx, |_, cx| cx.notify()) {
                tracing::debug!("a header tip came due after the root view went: {err:#}");
            }
        }));
        cx.notify();
    }

    /// Before the main window renders: drops the header tip whose element
    /// the last render did not build, or built with other text (its pane
    /// closed or changed session), shows the hovered one once its delay has
    /// passed, and tells this render's header elements which one to look
    /// for.
    pub(crate) fn settle_header_tips(&mut self, cx: &mut Context<Self>) {
        let frame = cx.default_global::<HeaderTipFrame>();
        let built = frame.built.take();
        let looked_for = frame.looking_for.take();
        let hovered = self
            .header_tips
            .hovered()
            .map(|tip| (tip.owner.clone(), tip.text.clone()));
        // A hover that began after the last render was not looked for then:
        // nothing is known of it yet.
        if let Some((owner, text)) = hovered
            && looked_for.as_deref() == Some(owner.as_str())
        {
            self.header_tips
                .retain(|_| built.as_ref().is_some_and(|built| built.text == text));
        }
        // The move that starts a drag can hover an element before gpui
        // holds the drag: no tip waits out a drag either.
        if cx.has_active_drag() {
            self.header_tips.press();
        }
        self.header_tips.due((self.now)());
        cx.default_global::<HeaderTipFrame>().looking_for =
            self.header_tips.hovered().map(|tip| tip.owner.clone());
    }

    /// Every button at the right of pane `pane_id`'s header: the Terminal /
    /// Chat toggle on a session that offers a chat, then split, move, the
    /// prompt jumps (on a terminal only) and close, each tipping `at`.
    fn pane_buttons(
        &self,
        tab_id: &str,
        pane_id: &str,
        session_id: Option<&str>,
        at: TipAt,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let session = session_id.and_then(|id| self.sidebar.session(id));
        let shown = self.view_shown(pane_id, session_id);
        let prompt_jumps = session
            .filter(|session| {
                matches!(
                    session.mode,
                    SessionMode::Interactive | SessionMode::PlainShell
                )
            })
            .filter(|_| !shown.hides_terminal())
            .map(|_| self.pane_prompt_jumps(pane_id, cx));
        let mut buttons = Self::header_buttons(tab_id, pane_id, session_id, prompt_jumps, at, cx);
        if session.is_some_and(offers_chat) {
            let chat_up = session_id.is_some_and(|id| self.session_chat_available(id));
            buttons.insert(0, view_segments(pane_id, shown, chat_up, at, cx));
            if shown == PaneView::Chat {
                let base = header_control(pane_id, "chat-compact", "Compact", COMPACT_TIP, at, cx);
                buttons.insert(1, compact_toggle(base, self.sidebar.chat_mode(), cx));
            }
        }
        buttons
    }

    /// Follows a drag over the shown tab: the pane under the pointer and the
    /// edge a drop there takes, while that drop would be accepted.
    fn on_pane_drag_move(
        &mut self,
        event: &DragMoveEvent<DragPayload>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let payload = event.drag(cx).clone();
        let target = self
            .drop_at(&payload, window, event.event.position, None)
            .map(|(pane_id, edge, _)| (pane_id, edge));
        if target != self.pane_drop {
            self.pane_drop = target;
            cx.notify();
        }
    }

    /// A drop on the shown tab: the move the pane under the pointer takes,
    /// worked out again where the release is, with its undo entry.
    fn on_pane_drop(&mut self, payload: &DragPayload, window: &mut Window, cx: &mut Context<Self>) {
        self.pane_drop = None;
        self.pane_drag = None;
        self.pane_drag_session = None;
        self.sidebar_drag = None;
        self.sidebar_drop = None;
        let request_id = Some(new_request_id());
        let at = window.mouse_position();
        if let Some((_, _, drop)) = self.drop_at(payload, window, at, request_id) {
            self.send_leaf_drop(drop, cx);
        }
        cx.notify();
    }

    /// Files dropped from the desktop on the shown tab: they go to the pane
    /// under the pointer, unless it is popped out.
    fn on_file_drop(&mut self, paths: &ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane_id) = self.file_drop_target(window) else {
            tracing::debug!("a file drop on no pane that takes one; ignored");
            return;
        };
        self.drop_files(&pane_id, paths.paths(), cx);
    }

    /// The pane of the shown tab under the pointer in `window`, when it
    /// takes a file drop: a popped pane's card takes none.
    fn file_drop_target(&self, window: &Window) -> Option<String> {
        let here = window.window_handle().window_id();
        let tab = self.window_tab(here)?;
        let rect = grid_rect(self.layouts.grid(here)?);
        let at = window.mouse_position();
        let at = (at.x / px(1.0), at.y / px(1.0));
        let (pane_id, _) = drag::pane_target(tab.grid()?, rect, PANE_GUTTER, at)?;
        (!self.is_pane_popped(&pane_id)).then_some(pane_id)
    }

    /// Sends what a pane or leaf drop does: a pane move with its undo entry,
    /// or the request that places an unbound session, with none. The placing
    /// request arms the bind, so the pane the session lands in claims its
    /// size.
    pub(crate) fn send_leaf_drop(&mut self, drop: LeafDrop, cx: &mut Context<Self>) {
        match drop {
            LeafDrop::Move(drop) => self.send_pane_move(&drop, cx),
            LeafDrop::Send { message, session } => {
                // A followed run has no terminal, so there is no size for a
                // bind to claim: only a placed session arms one.
                if let (Some(session), Some(target)) = (session, tabs::bind_target(&message)) {
                    self.tabs.arm_bind(&session, target);
                }
                self.tabs.note_layout_request(&message);
                self.send_to(&HostKey::Local, *message);
            }
        }
    }

    /// The pane of the shown tab under `at`, the edge the drop lands on and
    /// what the drop of `payload` there does, when it would be accepted: a
    /// pane from any tab, and a sidebar leaf's, a run's or a subagent's drop
    /// on a pane of the shown one. The pane is found in the grid `window`
    /// laid out.
    fn drop_at(
        &self,
        payload: &DragPayload,
        window: &Window,
        at: Point<Pixels>,
        request_id: Option<String>,
    ) -> Option<(String, PaneDropEdge, LeafDrop)> {
        let here = window.window_handle().window_id();
        let tab = self.window_tab(here)?;
        if !matches!(
            payload,
            DragPayload::Pane { .. }
                | DragPayload::Leaf { .. }
                | DragPayload::Run { .. }
                | DragPayload::Subagent { .. }
                | DragPayload::RunSubagent { .. }
        ) {
            return None;
        }
        let rect = grid_rect(self.layouts.grid(here)?);
        let at = (at.x / px(1.0), at.y / px(1.0));
        let (pane_id, edge) = drag::pane_target(tab.grid()?, rect, PANE_GUTTER, at)?;
        // A popped pane's card takes no drop: it would split or rebind the
        // pane its pop-out shows.
        if self.is_pane_popped(&pane_id) {
            return None;
        }
        let target = drag::LeafTarget {
            tab_id: &tab.id,
            pane_id: &pane_id,
            edge,
        };
        if let DragPayload::Leaf {
            session_id, host, ..
        } = payload
        {
            let session = SessionRef::new(host.clone(), session_id.clone());
            let source_online = !self.is_remote_offline(host);
            let (edge, drop) = drag::leaf_drop(
                &self.tabs,
                &self.sidebar.sessions_by_host(),
                &session,
                &target,
                request_id,
                source_online,
            )?;
            return Some((pane_id, edge, drop));
        }
        let following = match payload {
            DragPayload::Run { run_id, host } => Some((
                host,
                PaneFollow::DispatchRun {
                    run_id: run_id.clone(),
                },
            )),
            DragPayload::Subagent {
                session_id,
                agent_id,
                host,
            } => Some((
                host,
                PaneFollow::Subagent {
                    session_id: session_id.clone(),
                    agent_id: agent_id.clone(),
                },
            )),
            DragPayload::RunSubagent {
                run_id,
                agent_id,
                host,
            } => Some((
                host,
                PaneFollow::RunSubagent {
                    run_id: run_id.clone(),
                    agent_id: agent_id.clone(),
                },
            )),
            _ => None,
        };
        if let Some((host, follow)) = following {
            let (edge, drop) = self.follow_drop_at(host, follow, &target, request_id)?;
            return Some((pane_id, edge, drop));
        }
        let drop = drag::pane_drop(
            self.tabs.tabs(),
            payload,
            &tab.id,
            &pane_id,
            edge,
            request_id,
        )?;
        Some((pane_id, edge, LeafDrop::Move(drop)))
    }

    /// The drop of the sidebar `follow`, listed by `host`, at `target`: a
    /// split with a new pane following it, or the pane's content replaced by
    /// it; none while `host` is offline.
    fn follow_drop_at(
        &self,
        host: &HostKey,
        follow: PaneFollow,
        target: &drag::LeafTarget<'_>,
        request_id: Option<String>,
    ) -> Option<(PaneDropEdge, LeafDrop)> {
        if self.is_remote_offline(host) {
            return None;
        }
        let tabs = self.tabs.tabs();
        match follow {
            PaneFollow::DispatchRun { run_id } => {
                drag::run_drop(tabs, host, &run_id, target, request_id)
            }
            PaneFollow::Subagent {
                session_id,
                agent_id,
            } => drag::subagent_drop(tabs, host, &session_id, &agent_id, target, request_id),
            PaneFollow::RunSubagent { run_id, agent_id } => {
                drag::run_subagent_drop(tabs, host, &run_id, &agent_id, target, request_id)
            }
            PaneFollow::Unknown => None,
        }
    }

    /// A left press on a header button, chip or Stop: it disarms what any
    /// press disarms but goes no further, so the header never takes it as
    /// the start of a pane drag.
    fn press_on_header_control(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.on_any_mouse_down(event, window, cx);
        cx.stop_propagation();
    }

    /// Split right and down (Shift: left and up), move to a new tab when
    /// the pane shows a session, previous and next prompt when
    /// `prompt_jumps` holds whether each can jump, and close, each tipping
    /// `at`.
    fn header_buttons(
        tab_id: &str,
        pane_id: &str,
        session_id: Option<&str>,
        prompt_jumps: Option<(bool, bool)>,
        at: TipAt,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let ids = (tab_id.to_owned(), pane_id.to_owned());
        let (right, down) = (ids.clone(), ids);
        let mut buttons = vec![
            header_button(
                pane_id,
                "split-right",
                "│",
                "Split right (Shift: left)",
                at,
                cx,
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, _, _| {
                let (tab, pane) = &right;
                this.split_pane(
                    tab,
                    pane,
                    SplitDirection::Horizontal,
                    event.modifiers().shift,
                );
            }))
            .into_any_element(),
            header_button(pane_id, "split-down", "─", "Split down (Shift: up)", at, cx)
                .on_click(cx.listener(move |this, event: &ClickEvent, _, _| {
                    let (tab, pane) = &down;
                    this.split_pane(tab, pane, SplitDirection::Vertical, event.modifiers().shift);
                }))
                .into_any_element(),
        ];
        if session_id.is_some() {
            let (tab, pane) = (tab_id.to_owned(), pane_id.to_owned());
            buttons.push(
                header_button(
                    pane_id,
                    "pane-move-new-tab",
                    "↗",
                    "Move this pane to a new tab",
                    at,
                    cx,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                    this.move_pane_to_new_tab(&tab, &pane);
                }))
                .into_any_element(),
            );
        }
        if let Some((previous, next)) = prompt_jumps {
            buttons.push(prompt_button(pane_id, Dir::Previous, previous, at, cx));
            buttons.push(prompt_button(pane_id, Dir::Next, next, at, cx));
        }
        buttons.push(Self::close_button(tab_id, pane_id, session_id, at, cx));
        buttons
    }

    /// The right of `target`'s header, each part tipping its place: its Pop
    /// out and Stop, then its other buttons; or, when the header is too
    /// narrow for them beside the name, how the session stands (an armed
    /// Stop's Confirm stop and Cancel, or a stopped session's exit code),
    /// then ⋯ and Close.
    fn header_right(
        &self,
        target: &HeaderTarget<'_>,
        cx: &mut Context<Self>,
    ) -> (Vec<AnyElement>, Vec<AnyElement>) {
        let HeaderTarget {
            tab_id,
            pane_id,
            session_id,
            at,
            ..
        } = *target;
        let cluster = self.header_cluster(tab_id, pane_id, session_id, cx);
        // A followed dispatch run's Stop is always in sight.
        let run_stop = self.header_run_stop(tab_id, pane_id, at, cx);
        if let Some(menu_at) = self.header_overflow_anchor(target, &cluster, cx) {
            // How the session stands stays in sight beside ⋯: an armed
            // Stop's Confirm stop and Cancel, a stopped session's exit code.
            let mut controls =
                if matches!(cluster.stop, Some(StopSlot::Armed | StopSlot::ExitCode(_))) {
                    self.header_stop(pane_id, session_id, StopPlace::Grid, cx)
                } else {
                    Vec::new()
                };
            controls.extend(run_stop);
            let buttons = vec![
                overflow_button(tab_id, pane_id, menu_at, at, cx),
                Self::close_button(tab_id, pane_id, session_id, at, cx),
            ];
            return (controls, buttons);
        }
        let mut controls: Vec<AnyElement> = self
            .header_pop_out(tab_id, pane_id, session_id, at, cx)
            .into_iter()
            .collect();
        controls.extend(self.header_stop(pane_id, session_id, StopPlace::Grid, cx));
        controls.extend(run_stop);
        let buttons = self.pane_buttons(tab_id, pane_id, session_id, at, cx);
        (controls, buttons)
    }

    /// A pane header's Close, last in the row, tipping `at`.
    fn close_button(
        tab_id: &str,
        pane_id: &str,
        session_id: Option<&str>,
        at: TipAt,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let close = (tab_id.to_owned(), pane_id.to_owned());
        let close_session = session_id.map(str::to_owned);
        header_button(pane_id, "close-pane", "×", "Close pane", at, cx)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                let (tab_id, pane_id) = &close;
                this.close_pane_clicked(tab_id, pane_id, close_session.as_deref(), window, cx);
            }))
            .into_any_element()
    }

    /// What pane `pane_id`'s header holds from Pop out to the prompt
    /// arrows, the cluster a narrow header moves into its ⋯ menu: the same
    /// rules [`Self::header_pop_out`], [`Self::header_stop`] and
    /// [`Self::pane_buttons`] draw by.
    pub(crate) fn header_cluster(
        &self,
        tab_id: &str,
        pane_id: &str,
        session_id: Option<&str>,
        cx: &App,
    ) -> HeaderCluster {
        let host = self.slot_host(pane_id);
        let hosted = session_id.and_then(|id| self.sidebar.session_on(&host, id));
        let pop_out = hosted.filter(|session| can_attach(session)).map(|_| {
            let key = PopoutKey::Pane {
                tab_id: tab_id.to_owned(),
                pane_id: pane_id.to_owned(),
            };
            self.popouts.blocking(&key, self.tabs.tabs()).is_some()
        });
        let stop = hosted.map(|session| {
            if crate::session_actions::header_shows_exit_code(session) {
                StopSlot::ExitCode(crate::session_actions::exit_code_label(session))
            } else if self.confirm.is_armed(StopPlace::Grid, &host, &session.id) {
                StopSlot::Armed
            } else {
                StopSlot::Stop
            }
        });
        let session = session_id.and_then(|id| self.sidebar.session(id));
        let shown = self.view_shown(pane_id, session_id);
        let views = session.is_some_and(offers_chat).then(|| {
            let chat_up = session_id.is_some_and(|id| self.session_chat_available(id));
            (shown, chat_up)
        });
        let compact = views
            .filter(|(shown, _)| *shown == PaneView::Chat)
            .map(|_| self.sidebar.chat_mode());
        let prompts = session
            .filter(|session| {
                matches!(
                    session.mode,
                    SessionMode::Interactive | SessionMode::PlainShell
                )
            })
            .filter(|_| !shown.hides_terminal())
            .map(|_| self.pane_prompt_jumps(pane_id, cx));
        HeaderCluster {
            pop_out,
            stop,
            views,
            compact,
            move_tab: session_id.is_some(),
            prompts,
        }
    }

    /// Whether pane `pane_id` of the tab on screen draws its header
    /// collapsed: its cluster moved into the ⋯ menu, as the main window
    /// last laid its grid out.
    #[must_use]
    pub fn pane_header_collapsed(&self, pane_id: &str, cx: &App) -> bool {
        let Some(tab_id) = self.tabs.active_tab().map(|tab| tab.id.clone()) else {
            return false;
        };
        let session_id = self
            .tab_pane_sessions(&tab_id)
            .into_iter()
            .find(|(id, _)| id == pane_id)
            .and_then(|(_, session)| session);
        let parts = session_id
            .as_ref()
            .and_then(|_| self.pane_session_in(&tab_id, pane_id))
            .map(|(host, session)| self.session_header_parts(&host, session));
        let main = self.layouts.main();
        let target = self.header_target(
            &tab_id,
            pane_id,
            session_id.as_deref(),
            parts.as_ref(),
            main,
        );
        let cluster = self.header_cluster(target.tab_id, target.pane_id, target.session_id, cx);
        self.header_overflow_anchor(&target, &cluster, cx).is_some()
    }

    /// The header `pane_id` of tab `tab_id` builds in window `here`, for
    /// session `session_id` and the parts it draws: its tips show below the
    /// row, or at the mouse when the tab draws in its pop-out window, which
    /// keeps gpui's tooltips.
    fn header_target<'a>(
        &'a self,
        tab_id: &'a str,
        pane_id: &'a str,
        session_id: Option<&'a str>,
        parts: Option<&'a PaneHeaderParts>,
        here: WindowId,
    ) -> HeaderTarget<'a> {
        let at = if self.is_tab_popped(tab_id) {
            TipAt::Mouse
        } else {
            TipAt::BelowRow
        };
        HeaderTarget {
            tab_id,
            pane_id,
            session_id,
            parts,
            follow: self.tabs.pane_follow(tab_id, pane_id),
            run: self.pane_run_shown(tab_id, pane_id),
            run_stop: self.run_stop_labels(tab_id, pane_id),
            at,
            here,
        }
    }

    /// Pane `pane_id`'s card as window `here` last laid its grid out, in
    /// window coordinates; `None` before the grid has been laid out.
    fn pane_card(&self, tab_id: &str, pane_id: &str, here: WindowId) -> Option<tabs::Rect> {
        let bounds = self.layouts.grid(here)?;
        let grid = self.tabs.tab(tab_id)?.grid()?;
        tabs::pane_rects(grid, grid_rect(bounds), PANE_GUTTER)
            .into_iter()
            .find(|(id, _)| id == pane_id)
            .map(|(_, rect)| rect)
    }

    /// Where the ⋯ menu of `target`'s header opens, its top right corner
    /// just under the ⋯ button, when its header is too narrow for `cluster`
    /// beside the name at its minimum; `None` while the cluster fits.
    fn header_overflow_anchor(
        &self,
        target: &HeaderTarget<'_>,
        cluster: &HeaderCluster,
        cx: &App,
    ) -> Option<Point<Pixels>> {
        let card = self.pane_card(target.tab_id, target.pane_id, target.here)?;
        let fit = header_fit(card, target, cluster, cx);
        pane_header_fit::collapses(&fit).then(|| overflow_menu_anchor(card, cx))
    }

    /// Whether `target`'s header holds its chips at their floor, judged on
    /// the same widths as its collapse; true before the grid is laid out.
    fn chip_floors_fit(&self, target: &HeaderTarget<'_>, cx: &App) -> bool {
        let Some(card) = self.pane_card(target.tab_id, target.pane_id, target.here) else {
            return true;
        };
        let cluster = self.header_cluster(target.tab_id, target.pane_id, target.session_id, cx);
        let fit = header_fit(card, target, &cluster, cx);
        pane_header_fit::floors_fit(&fit, CHIP_MIN_WIDTH, HEADER_BUTTON_SIZE)
    }

    /// The session shown by pane `pane_id` of the tab on screen, with the
    /// pane's host, as that host lists it.
    pub(crate) fn active_pane_hosted(&self, pane_id: &str) -> Option<(HostKey, &SessionSnapshot)> {
        let tab_id = &self.tabs.active_tab()?.id;
        self.pane_session_in(tab_id, pane_id)
    }

    /// The session pane `pane_id` of tab `tab_id` shows, with the host the
    /// pane names, as that host lists it.
    fn pane_session_in(&self, tab_id: &str, pane_id: &str) -> Option<(HostKey, &SessionSnapshot)> {
        let grid = self.tabs.tab(tab_id)?.grid()?;
        let pane = tabs::collect_panes(grid)
            .into_iter()
            .find(|pane| pane.id == pane_id)?;
        let session = self.hosted_session(&pane.host, pane.session?)?;
        Some((pane.host, session))
    }

    /// Session `id` as `host` lists it.
    fn hosted_session(&self, host: &HostKey, id: &str) -> Option<&SessionSnapshot> {
        self.sidebar.sessions_of(host).iter().find(|s| s.id == id)
    }

    /// Whether pane `pane_id`'s previous and next prompt buttons have
    /// somewhere to scroll; both false for a pane that is not open.
    #[must_use]
    pub fn pane_prompt_jumps(&self, pane_id: &str, cx: &App) -> (bool, bool) {
        self.panes.get(pane_id).map_or((false, false), |slot| {
            let pane = slot.view.read(cx);
            (pane.can_jump(Dir::Previous), pane.can_jump(Dir::Next))
        })
    }

    /// A prompt button's click: closes the gutter menu as a wheel scroll
    /// does, then scrolls pane `pane_id` to the prompt `dir` names.
    pub(crate) fn jump_prompt(
        &mut self,
        pane_id: &str,
        dir: Dir,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_shell_menu(window, cx);
        if let Some(slot) = self.panes.get(pane_id) {
            slot.view.update(cx, |pane, cx| pane.jump_prompt(dir, cx));
        }
        cx.notify();
    }

    /// The text of the pane header tip on screen, if one is.
    #[must_use]
    pub fn header_tip_text(&self) -> Option<String> {
        self.header_tips.shown().map(|tip| tip.text.clone())
    }

    /// What the header of pane `pane_id` in the tab on screen shows of its
    /// session; `None` for an empty pane.
    #[must_use]
    pub fn pane_header_parts(&self, pane_id: &str) -> Option<PaneHeaderParts> {
        self.active_pane_hosted(pane_id)
            .map(|(host, session)| self.session_header_parts(&host, session))
    }

    /// What a pane header shows of `host`'s `session`, with the host's chip
    /// when it is a remote one.
    fn session_header_parts(&self, host: &HostKey, session: &SessionSnapshot) -> PaneHeaderParts {
        let label_agents = self.sidebar.label_agents(host);
        let host = self.host_chip_label(host);
        header_parts(
            session,
            self.sidebar.is_unseen(&session.id),
            label_agents,
            host.as_ref(),
        )
    }

    /// The label of `host`'s chip; `None` for This PC. The target comes
    /// from the host's connection, else its saved entry; a host known by
    /// neither shows its key.
    pub(crate) fn host_chip_label(&self, host: &HostKey) -> Option<HostLabel> {
        let HostKey::Remote(key) = host else {
            return None;
        };
        let target = self
            .conn_of(host)
            .map(|conn| conn.target().clone())
            .or_else(|| {
                self.sidebar
                    .ui_state()
                    .saved_hosts
                    .iter()
                    .filter_map(|saved| saved.target().ok())
                    .find(|target| HostKey::of(target) == *host)
            });
        Some(
            target
                .as_ref()
                .and_then(HostLabel::of)
                .unwrap_or_else(|| HostLabel {
                    short: key.clone(),
                    full: key.clone(),
                }),
        )
    }

    /// What the toolbar of pane `pane_id`'s pop-out shows of its session,
    /// as its header in the main grid would; `None` while the pane is not
    /// popped out or is empty.
    #[must_use]
    pub fn popout_header_parts(&self, pane_id: &str) -> Option<PaneHeaderParts> {
        self.popouts.popped_key(pane_id)?;
        self.popped_pane_session(pane_id)
            .map(|(host, session)| self.session_header_parts(&host, session))
    }

    /// The pop-out toolbar's "in {tab name}": the tab pane `pane_id` is
    /// docked in; `None` while the pane is not popped out.
    #[must_use]
    pub fn popout_docked_in(&self, pane_id: &str) -> Option<String> {
        let PopoutKey::Pane { tab_id, .. } = self.popouts.popped_key(pane_id)? else {
            return None;
        };
        let name = self
            .tabs
            .tab(tab_id)
            .map_or_else(|| tab_id.clone(), |tab| self.sidebar.tab_display_name(tab));
        Some(format!("in {name}"))
    }

    /// The session pane `pane_id` shows, in whichever tab holds it, with the
    /// host the pane names, as that host lists it.
    fn popped_pane_session(&self, pane_id: &str) -> Option<(HostKey, &SessionSnapshot)> {
        let pane = popouts::pane_in_tabs(pane_id, self.tabs.tabs())?;
        let session = self.hosted_session(&pane.host, pane.session?)?;
        Some((pane.host, session))
    }

    /// The body of a pane showing `session_id` before its terminal attaches:
    /// a muted note. A headless pane never attaches, so its note holds the
    /// pane's focus handle and a click in it focuses the pane, as an empty
    /// pane does, and the app's shortcuts stay live.
    fn note_body(
        &self,
        pane_id: &str,
        session_id: &str,
        slot: Option<&PaneSlot>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let session = self.sidebar.session(session_id);
        let note = muted_note(crate::fonts::ui_text_size(cx), body_note(session), &t)
            .driver_id(|| format!("pane-note-{pane_id}"));
        let headless = session.is_some_and(|session| session.mode == SessionMode::Headless);
        match slot.filter(|_| headless) {
            Some(slot) => {
                let handle = slot.view.read(cx).focus_handle();
                note.track_focus(&handle)
                    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                        handle.focus(window, cx);
                    })
                    .into_any_element()
            }
            None => note.into_any_element(),
        }
    }

    /// The note the body of pane `pane_id` draws where a terminal would be,
    /// while its terminal draws in a pop-out window or the pane shows a
    /// session it has not attached to; `None` otherwise.
    #[must_use]
    pub fn pane_note(&self, pane_id: &str) -> Option<&'static str> {
        let slot = self.panes.get(pane_id)?;
        if self.is_pane_popped(pane_id) {
            return Some(POPPED_OUT_NOTE);
        }
        let slot = Some(slot).filter(|slot| !slot.attached)?;
        let session = self.sidebar.session(slot.session.as_deref()?);
        Some(body_note(session))
    }

    /// Whether pane `pane_id`'s terminal has the keyboard in `window`.
    #[must_use]
    pub fn pane_terminal_focused(&self, pane_id: &str, window: &Window, cx: &App) -> bool {
        self.panes
            .get(pane_id)
            .is_some_and(|slot| slot.view.read(cx).is_focused(window))
    }

    /// The pane the drop overlay is drawn on and the edge it shows, while a
    /// pane drag, or a sidebar leaf, run or subagent drag, is under way.
    #[must_use]
    pub fn pane_drop_overlay(&self, cx: &App) -> Option<(String, PaneDropEdge)> {
        let dragging = self.active_pane_drag(cx).is_some()
            || matches!(
                self.active_sidebar_drag(cx),
                Some(
                    DragPayload::Leaf { .. }
                        | DragPayload::Run { .. }
                        | DragPayload::Subagent { .. }
                        | DragPayload::RunSubagent { .. }
                )
            );
        self.pane_drop.clone().filter(|_| dragging)
    }
}

/// What a pane header shows of its session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneHeaderParts {
    pub status: SessionStatus,
    pub mode: SessionMode,
    /// The agent's last turn ended while this client looked elsewhere.
    pub unseen: bool,
    /// The session is idle with its turn's background work still running.
    pub background: bool,
    /// The status glyph's hover text.
    pub status_tip: String,
    pub title: String,
    pub title_tip: String,
    /// The chips after the name, in the order they show.
    pub chips: Vec<HeaderChip>,
}

/// What a pane header chip says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderChipKind {
    /// The agent, or a plain shell's program.
    Runtime,
    /// The first member's `repo:branch`.
    Branch,
    /// `+N`: the further members of a workspace session.
    More,
    /// The provider endpoint an agent session is routed to.
    Provider,
    /// The remote host the session runs on; This PC's sessions have none.
    Host,
}

impl HeaderChipKind {
    /// The chip's part of its selector, `pane-chip-<pane>-<name>`.
    fn name(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Branch => "branch",
            Self::More => "more",
            Self::Provider => "provider",
            Self::Host => "host",
        }
    }
}

/// How a host chip names a remote host: its short name, and its full name
/// on hover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostLabel {
    pub short: String,
    pub full: String,
}

impl HostLabel {
    /// `target`'s label; `None` for This PC.
    fn of(target: &Target) -> Option<Self> {
        let full = match target {
            Target::Local => return None,
            Target::Tailnet(host) => host.name(),
            Target::Ssh(host) => host.name(),
        };
        Some(Self {
            short: target.short_name().unwrap_or(full).to_owned(),
            full: full.to_owned(),
        })
    }
}

/// One chip in a pane header: its text, its hover text, and for the runtime
/// chip the icon it is drawn as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderChip {
    pub kind: HeaderChipKind,
    pub text: String,
    pub tip: String,
    /// The runtime chip's icon; `None` draws the chip's text.
    pub icon: Option<RuntimeIcon>,
}

impl HeaderChip {
    fn new(kind: HeaderChipKind, text: impl Into<String>, tip: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            tip: tip.into(),
            icon: None,
        }
    }

    /// The chip drawn as `icon`.
    #[must_use]
    fn with_icon(mut self, icon: Option<RuntimeIcon>) -> Self {
        self.icon = icon;
        self
    }
}

/// What a pane header shows of `session`; `host` labels the remote host it
/// runs on, `None` for This PC.
fn header_parts(
    session: &SessionSnapshot,
    unseen: bool,
    label_agents: &[AgentDescriptor],
    host: Option<&HostLabel>,
) -> PaneHeaderParts {
    let mut chips: Vec<HeaderChip> = host
        .map(|host| HeaderChip::new(HeaderChipKind::Host, &host.short, &host.full))
        .into_iter()
        .collect();
    chips.extend(runtime_label(session, label_agents).map(|runtime| {
        let tip = format!("Running {runtime}");
        let icon = runtime_icon(session, Some(&runtime), label_agents);
        HeaderChip::new(HeaderChipKind::Runtime, runtime, tip).with_icon(icon)
    }));
    chips.extend(session.agent_chips.iter().map(|agent_chip| {
        HeaderChip::new(
            HeaderChipKind::Provider,
            agent_chip.text.clone(),
            chip_tip(agent_chip).to_owned(),
        )
    }));
    chips.extend(member_chips(&session.members));
    PaneHeaderParts {
        status: session.status,
        mode: session.mode,
        unseen,
        background: session.background,
        status_tip: format!(
            "status: {}",
            status_label(session.status, session.background)
        ),
        title: display_label(session, label_agents),
        title_tip: label_tooltip(session, label_agents),
        chips,
    }
}

/// The first member's `repo:branch` chip, hovering as its worktree path,
/// then a `+N` chip for the rest whose hover lists them.
fn member_chips(members: &[SessionMember]) -> Vec<HeaderChip> {
    let name = |member: &SessionMember| format!("{}:{}", member.repo_name, member.branch);
    let Some((first, rest)) = members.split_first() else {
        return Vec::new();
    };
    let text = name(first);
    let tip = member_tip(text.clone(), first);
    let mut chips = vec![HeaderChip::new(HeaderChipKind::Branch, text, tip)];
    if !rest.is_empty() {
        let tip = rest
            .iter()
            .map(|member| member_tip(name(member), member))
            .collect::<Vec<_>>()
            .join("\n");
        chips.push(HeaderChip::new(
            HeaderChipKind::More,
            format!("+{}", rest.len()),
            tip,
        ));
    }
    chips
}

/// A member's `repo:branch` over its worktree path, or the name alone when
/// the member has no path.
fn member_tip(name: String, member: &SessionMember) -> String {
    if member.worktree_path.is_empty() {
        name
    } else {
        format!("{name}\n{}", member.worktree_path)
    }
}

/// The colour a pane's frame band draws: its frame colour while it has its
/// tab's focus, else its accent where no frame is set; `None` while it does
/// not, where the band shows the terminal's background.
fn frame_band(focused: bool, frame: Option<Rgb>, accent: Rgb) -> Option<Rgb> {
    if focused {
        Some(frame.unwrap_or(accent))
    } else {
        None
    }
}

/// A pane header's fill: raised while the pane has its tab's focus.
fn header_fill(focused: bool, t: &UiTheme) -> u32 {
    if focused { t.raised } else { t.surface }
}

/// How much of its accent a header's fade mixes over the header's own fill
/// at its left edge, the same whether the pane has the focus or not.
const HEADER_FADE_STRENGTH: f64 = 0.22;

/// The fraction of a header's width at which its fade reaches the plain
/// fill.
const HEADER_FADE_END: f32 = 0.34;

/// `fill`'s header fade as the two colours its gradient draws: the accent
/// mixed [`HEADER_FADE_STRENGTH`] over `fill` at the left edge, and the plain
/// fill where the wash ends.
fn header_fade_colors(fill: u32, accent: u32) -> (Rgb, Rgb) {
    let ground = appearance::term_rgb(fill);
    (
        crate::theme::mix(ground, appearance::term_rgb(accent), HEADER_FADE_STRENGTH),
        ground,
    )
}

/// The header's gradient stops: the accent wash at the left edge and the
/// plain fill at [`HEADER_FADE_END`] of the header's width.
fn header_fade_stops(fill: u32, accent: u32) -> (gpui::LinearColorStop, gpui::LinearColorStop) {
    let (from, to) = header_fade_colors(fill, accent);
    (
        gpui::linear_color_stop(gpui::rgb(appearance::packed(from)), 0.0),
        gpui::linear_color_stop(gpui::rgb(appearance::packed(to)), HEADER_FADE_END),
    )
}

/// The header's left side: the status glyph, the name and the chips, or
/// what the pane follows instead, or "Empty pane", each tipping `at`. With
/// `floors`, a text chip shrinks no further than [`CHIP_MIN_WIDTH`];
/// without, as far as nothing.
fn header_title(
    pane_id: &str,
    named: PaneTitle<'_>,
    at: TipAt,
    floors: bool,
    cx: &mut Context<RootView>,
) -> Vec<AnyElement> {
    let t = theme(cx);
    let title_id = format!("pane-title-{pane_id}");
    let selector = title_id.clone();
    let title = div()
        .id(ElementId::Name(SharedString::from(title_id.clone())))
        .driver_id(|| selector)
        .relative()
        .min_w(px(0.0))
        .overflow_hidden()
        .whitespace_nowrap();
    let strong = |title: Stateful<Div>, cx: &mut Context<RootView>| {
        title
            .text_size(ui_px(cx, TITLE_TEXT_SIZE))
            .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
            .text_color(gpui::rgb(t.text))
    };
    let parts = match named {
        PaneTitle::Session(parts) => parts,
        PaneTitle::Follow(_, Some(run)) => {
            let title = strong(title, cx);
            let [chip, _] = run_chip_widths(&run, cx);
            let floor = if floors {
                (chip > CHIP_MIN_WIDTH).then_some(px(CHIP_MIN_WIDTH))
            } else {
                Some(px(0.0))
            };
            return crate::dispatch_view::run_title(pane_id, &run, title, floor, cx);
        }
        PaneTitle::Follow(follow, None) => {
            return vec![title.child(follow_label(follow)).into_any_element()];
        }
        PaneTitle::Empty => return vec![title.child(drag::EMPTY_PANE_LABEL).into_any_element()],
    };
    let title = strong(title, cx);
    let status_id = format!("pane-status-{pane_id}");
    let status = div()
        .id(SharedString::from(status_id.clone()))
        .flex_none()
        .child(glyph_view(
            glyph(
                parts.status,
                parts.background,
                parts.mode,
                parts.unseen,
                &theme(cx),
            ),
            GlyphSize::Leaf,
            &format!("pane-glyph-{pane_id}"),
            &theme(cx),
        ));
    let title = title.children(ellipsized(
        &title_id,
        SharedString::from(parts.title.clone()),
        StyledText::new(parts.title.clone()),
    ));
    let mut out = vec![
        header_tipped(status, &status_id, pane_id, &parts.status_tip, at, cx).into_any_element(),
        header_tipped(title, &title_id, pane_id, &parts.title_tip, at, cx).into_any_element(),
    ];
    out.extend(
        parts
            .chips
            .iter()
            .map(|chip| header_chip(pane_id, chip, at, floors, cx)),
    );
    out
}

/// A 20 px Geist Mono chip: the runtime a 20 px square holding its icon,
/// repo:branch with its repo name bright, the rest filled.
fn header_chip(
    pane_id: &str,
    chip: &HeaderChip,
    at: TipAt,
    floors: bool,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let name = format!("pane-chip-{pane_id}-{}", chip.kind.name());
    let selector = name.clone();
    let base = div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .driver_id(|| selector)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(RootView::press_on_header_control),
        )
        .flex()
        .flex_none()
        .items_center()
        .h(ui_px(cx, CHIP_HEIGHT))
        .px(px(CHIP_PAD))
        .rounded(px(CHIP_RADIUS))
        .border_1()
        .border_color(gpui::rgba(t.transparent))
        .bg(gpui::rgb(t.chip))
        .font_family(DEFAULT_FAMILY)
        .font_weight(FontWeight::NORMAL)
        .text_size(ui_px(cx, CHIP_TEXT_SIZE))
        .text_color(gpui::rgb(t.text_2));
    let base = header_tipped(base, &name, pane_id, &chip.tip, at, cx);
    let base = if chip.kind == HeaderChipKind::Branch {
        base
    } else {
        base.whitespace_nowrap()
    };
    let text = chip.text.clone();
    let floor = if floors {
        (chip_width(chip, cx) > CHIP_MIN_WIDTH).then_some(px(CHIP_MIN_WIDTH))
    } else {
        Some(px(0.0))
    };
    match chip.kind {
        HeaderChipKind::Runtime => match &chip.icon {
            Some(icon) => base
                .w(ui_px(cx, CHIP_HEIGHT))
                .px(px(0.0))
                .justify_center()
                .child(runtime_icon_view(icon, ui_px(cx, 12.0), &name, &t)),
            None => chip_text(&name, base, text, floor),
        },
        HeaderChipKind::Branch => branch_chip(&name, base, text, floor, &t),
        HeaderChipKind::More | HeaderChipKind::Provider | HeaderChipKind::Host => {
            chip_text(&name, base, text, floor)
        }
    }
    .into_any_element()
}

/// A text chip's label: it shrinks and truncates with `…` before the title
/// does, down to `floor`, its tooltip keeping the whole text. `name` names
/// its text for the ellipsis probe.
fn chip_text(
    name: &str,
    base: Stateful<Div>,
    text: String,
    floor: Option<Pixels>,
) -> Stateful<Div> {
    let chip = base
        .overflow_hidden()
        .child(truncating(name, StyledText::new(text)));
    give_way(chip, floor)
}

/// `chip` shrinking before the title, down to `floor`; with no floor (a
/// chip no wider than [`CHIP_MIN_WIDTH`]) it keeps its own width.
pub(crate) fn give_way(chip: Stateful<Div>, floor: Option<Pixels>) -> Stateful<Div> {
    let Some(floor) = floor else {
        return chip;
    };
    let mut chip = chip.min_w(floor);
    chip.style().flex_shrink = Some(BRANCH_CHIP_SHRINK);
    chip
}

/// The repo:branch chip, its repo name bright and its colon subtle. It is
/// the first part of a header to give way when the header runs short: it
/// shrinks and truncates with `…` before the title does, down to `floor`,
/// its tooltip keeping the whole text. `id` names its text for the ellipsis
/// probe.
fn branch_chip(
    id: &str,
    base: Stateful<Div>,
    text: String,
    floor: Option<Pixels>,
    t: &UiTheme,
) -> Stateful<Div> {
    let highlights = text.find(':').map(|colon| {
        let color = |token: u32| HighlightStyle {
            color: Some(gpui::rgb(token).into()),
            ..HighlightStyle::default()
        };
        [
            (0..colon, color(t.text)),
            (colon..colon + 1, color(t.subtle)),
        ]
    });
    let label = StyledText::new(text).with_highlights(highlights.into_iter().flatten());
    let chip = base.overflow_hidden().child(truncating(id, label));
    give_way(chip, floor)
}

/// A split's two children around their divider, the first sized by `ratio`.
fn split_box(direction: SplitDirection, ratio: f32, parts: [AnyElement; 3]) -> AnyElement {
    let [first, divider, second] = parts;
    let ratio = drawn_ratio(ratio);
    let horizontal = direction == SplitDirection::Horizontal;
    let cell = || div().flex().min_w(px(0.0)).min_h(px(0.0)).overflow_hidden();
    let first_cell = if horizontal {
        cell().flex_none().h_full().w(relative(ratio))
    } else {
        cell().flex_none().w_full().h(relative(ratio))
    };
    div()
        .flex()
        .size_full()
        .when(!horizontal, Styled::flex_col)
        .child(first_cell.child(first))
        .child(divider)
        .child(cell().flex_1().child(second))
        .into_any_element()
}

/// The share of a split its first child is drawn with, whatever the layout
/// holds: not a finite number is an even split, and anything else stays inside
/// the daemon's range.
fn drawn_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        tabs::clamp_ratio(ratio)
    } else {
        0.5
    }
}

/// The note a pane draws for `session` while it has no terminal to show: a
/// headless session is never shown in this client, any other waits to
/// attach.
fn body_note(session: Option<&SessionSnapshot>) -> &'static str {
    if session.is_some_and(|session| session.mode == SessionMode::Headless) {
        HEADLESS_NOTE
    } else {
        WAITING_NOTE
    }
}

/// The session's status as the pane header's status tip spells it; an idle
/// session with its turn's `background` work still running reads
/// "background".
fn status_label(status: SessionStatus, background: bool) -> &'static str {
    match status {
        SessionStatus::Spawning => "starting",
        SessionStatus::Idle if background => "background",
        SessionStatus::Idle => "idle",
        SessionStatus::Working => "working",
        SessionStatus::AwaitingInput => "awaiting input",
        SessionStatus::Stopped => "stopped",
        SessionStatus::Error => "error",
    }
}

/// Muted text centred in the space it is given.
pub(crate) fn muted_note(offset: i8, text: impl Into<SharedString>, t: &UiTheme) -> Div {
    div()
        .flex()
        .flex_1()
        .size_full()
        .items_center()
        .justify_center()
        .text_size(ui_px_at(offset, UI_TEXT_SIZE))
        .text_color(gpui::rgb(t.muted))
        .child(text.into())
}

/// What a pane that follows something this app does not know draws: a
/// muted note saying it needs a newer app.
fn follow_placeholder(pane_id: &str, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let text = "This pane needs a newer app.";
    muted_note(crate::fonts::ui_text_size(cx), text, &t)
        .driver_id(|| format!("pane-follow-{pane_id}"))
}

/// A muted `heading` over a row of `buttons`, centred in the space it is
/// given.
fn spawn_choices(
    offset: i8,
    heading: &'static str,
    buttons: impl IntoIterator<Item = Stateful<Div>>,
    t: &UiTheme,
) -> Div {
    muted_note(offset, heading, t)
        .flex_col()
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .flex_wrap()
                .justify_center()
                .gap(px(6.0))
                .children(buttons),
        )
}

/// A 26 px square glyph button in a pane header.
fn header_button(
    pane_id: &str,
    action: &str,
    glyph: &'static str,
    tip: &'static str,
    at: TipAt,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let t = theme(cx);
    header_control(pane_id, action, glyph, tip, at, cx)
        .hover(move |style| style.bg(gpui::rgb(t.hover)).text_color(gpui::rgb(t.text)))
}

/// A pane header button, tagged `<action>-<pane>`, without a hover look:
/// the caller sets the one it needs (gpui takes one per element). Its tip
/// shows `at`.
fn header_control(
    pane_id: &str,
    action: &str,
    glyph: &'static str,
    tip: &'static str,
    at: TipAt,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let t = theme(cx);
    let name = format!("{action}-{pane_id}");
    let control = div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .driver_id(|| name)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(RootView::press_on_header_control),
        )
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(HEADER_BUTTON_SIZE))
        .rounded(px(HEADER_BUTTON_RADIUS))
        .text_color(gpui::rgb(t.text_2))
        .cursor_pointer();
    header_tipped(
        control,
        &format!("{action}-{pane_id}"),
        pane_id,
        tip,
        at,
        cx,
    )
    .child(glyph)
}

/// Where a pane header element's tip shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TipAt {
    /// As a header tip, below the header row: the main window's headers.
    BelowRow,
    /// As gpui's tooltip at the mouse: a pop-out window's.
    Mouse,
}

/// What the main window's pane headers record while they are built and
/// painted, for the header tip.
#[derive(Default)]
pub(crate) struct HeaderTipFrame {
    /// The hovered header element the render under way looks for.
    looking_for: Option<String>,
    /// That element as the render built it.
    built: Option<BuiltSite>,
    /// Each header row's window bounds, by the id of the pane it heads.
    rows: HashMap<String, Bounds<Pixels>>,
}

impl gpui::Global for HeaderTipFrame {}

/// The hovered header element as a render built it: its tip and its row.
struct BuiltSite {
    text: String,
    row: String,
}

/// Makes `site`, the header element tagged `owner` in the header row of
/// `row`, show `text` `at`: below the header row after the hover delay, or
/// as gpui's tooltip at the mouse.
pub(crate) fn header_tipped(
    site: Stateful<Div>,
    owner: &str,
    row: &str,
    text: &str,
    at: TipAt,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    if at == TipAt::Mouse {
        return site.tooltip(tooltip(text.to_owned()));
    }
    // Only the hovered element says it was built, and nothing paints.
    let looked_for = cx
        .try_global::<HeaderTipFrame>()
        .is_some_and(|frame| frame.looking_for.as_deref() == Some(owner));
    if looked_for {
        cx.global_mut::<HeaderTipFrame>().built = Some(BuiltSite {
            text: text.to_owned(),
            row: row.to_owned(),
        });
    }
    let (owner, text) = (owner.to_owned(), text.to_owned());
    site.on_hover(cx.listener(move |this, hovered: &bool, window, cx| {
        if *hovered {
            this.hover_header_tip(&owner, &text, window, cx);
        } else if this.header_tips.leave(&owner) {
            cx.notify();
        }
    }))
}

/// An invisible layer that records where the header row of `row` was laid
/// out, for the header tips that open below it, when that changed.
fn header_row_probe(row: &str) -> impl IntoElement {
    let row = row.to_owned();
    canvas(
        move |bounds, _, cx| {
            let frame = cx.default_global::<HeaderTipFrame>();
            if frame.rows.get(&row) != Some(&bounds) {
                frame.rows.insert(row, bounds);
            }
        },
        |_, (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// A pane header's Terminal | Chat | Summary control, the view `shown`
/// marked; each segment is tagged `pane-view-<view>-<pane>` and a click
/// switches pane `pane_id` to its view. While the session's agent is not
/// running (`chat_up` false) Chat and Summary are drawn disabled, saying so.
fn view_segments(
    pane_id: &str,
    shown: PaneView,
    chat_up: bool,
    at: TipAt,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let segments: Vec<AnyElement> = [
        (PaneView::Terminal, "pane-view-terminal"),
        (PaneView::Chat, "pane-view-chat"),
        (PaneView::Summary, "pane-view-summary"),
    ]
    .into_iter()
    .map(|(view, action)| {
        let enabled = chat_up || !view.hides_terminal();
        let (label, tip) = view_segment(view);
        let tip = if enabled { tip } else { AGENT_DOWN_TIP };
        let segment = header_button(pane_id, action, label, tip, at, cx)
            .w_auto()
            .px(px(HEADER_SEGMENT_PAD))
            .when(view == shown, |segment| {
                segment.bg(gpui::rgb(t.chip)).text_color(gpui::rgb(t.text))
            });
        if !enabled {
            return segment
                .opacity(DISABLED_OPACITY)
                .cursor_default()
                .into_any_element();
        }
        let pane = pane_id.to_owned();
        segment
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                if this.pane_view(&pane) != view {
                    this.set_pane_view(&pane, view, window, cx);
                }
            }))
            .into_any_element()
    })
    .collect();
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(HEADER_SEGMENT_GAP))
        .children(segments)
        .into_any_element()
}

/// A pane header's previous or next prompt button, drawn disabled when it
/// has nowhere to scroll so the header's buttons never move. Its click asks
/// the pane again, so a header drawn before new output still jumps.
fn prompt_button(
    pane_id: &str,
    dir: Dir,
    enabled: bool,
    at: TipAt,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let (action, glyph, tip) = match dir {
        Dir::Previous => ("pane-prompt-prev", "↑", "Previous prompt"),
        Dir::Next => ("pane-prompt-next", "↓", "Next prompt"),
    };
    let pane = pane_id.to_owned();
    let button = header_button(pane_id, action, glyph, tip, at, cx).on_click(cx.listener(
        move |this, _: &ClickEvent, window, cx| {
            this.jump_prompt(&pane, dir, window, cx);
        },
    ));
    if enabled {
        button.into_any_element()
    } else {
        button
            .opacity(DISABLED_OPACITY)
            .cursor_default()
            .into_any_element()
    }
}

/// The gutter between two pane cards: transparent until hovered or
/// dragged, when it shows `t.hover`. A vertical line between side-by-side
/// panes, a horizontal one between stacked panes.
fn gutter_handle(id: ElementId, vertical_line: bool, active: bool, t: &UiTheme) -> Stateful<Div> {
    let handle = div()
        .id(id)
        .flex_none()
        .hover(move |style| style.bg(gpui::rgb(t.hover)))
        .when(active, move |handle| handle.bg(gpui::rgb(t.hover)));
    if vertical_line {
        handle
            .w(px(PANE_GUTTER))
            .h_full()
            .cursor(CursorStyle::ResizeLeftRight)
    } else {
        handle
            .h(px(PANE_GUTTER))
            .w_full()
            .cursor(CursorStyle::ResizeUpDown)
    }
}

/// The fill of a dragged pane's ghost and of the swap badge in `t`: the
/// ground at 78%, as `0xRRGGBBAA`.
fn ghost_fill(t: &UiTheme) -> u32 {
    (t.ground << 8) | 0xc7
}
/// How strongly a half or the centre box is tinted with the accent.
const DROP_TINT_ALPHA: f32 = 0.22;
/// How strongly an outer strip is tinted with the accent.
const OUTER_TINT_ALPHA: f32 = 0.40;
/// The ghost's and the swap badge's text size.
const GHOST_TEXT_SIZE: f32 = 12.0;

/// What follows the pointer while a pane is dragged: a pill with the pane's
/// header label, edged in the pane's accent, its corner at the pointer.
struct PaneGhost {
    label: SharedString,
    accent: u32,
    /// Where the pointer sat in the header when the drag started. gpui
    /// draws the ghost that far up and left of the pointer, as if the
    /// header itself were dragged; the pill moves back by it.
    offset: Point<Pixels>,
}

impl Render for PaneGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        div().child(
            ghost_pill(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                self.label.clone(),
                self.accent,
                crate::fonts::ui_family(cx),
                &t,
            )
            .driver_id(|| "pane-drag-ghost".to_owned())
            .absolute()
            .left(self.offset.x)
            .top(self.offset.y),
        )
    }
}

/// A translucent pill with `text`, edged in `accent`: the drag ghost and the
/// swap badge, drawn in the interface's `family`.
fn ghost_pill(
    offset: i8,
    shift: i8,
    text: impl Into<SharedString>,
    accent: u32,
    family: SharedString,
    t: &UiTheme,
) -> Div {
    div()
        .flex_none()
        .px(px(10.0))
        .py(px(3.0))
        .rounded_full()
        .bg(gpui::rgba(ghost_fill(t)))
        .border_1()
        .border_color(gpui::rgb(accent))
        .whitespace_nowrap()
        .font_family(family)
        .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::NORMAL))
        .text_size(ui_px_at(offset, GHOST_TEXT_SIZE))
        .text_color(gpui::rgb(t.text))
        .child(text.into())
}

/// A pop-out window's toolbar for the terminal `slot_id`: `title` on the
/// left, `controls` on the right.
fn popout_toolbar_row(
    slot_id: &str,
    title: Vec<AnyElement>,
    controls: Vec<AnyElement>,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let name = format!("popout-toolbar-{slot_id}");
    div()
        .driver_id(|| name)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.0))
        .h(ui_px(cx, PANE_HEADER_HEIGHT))
        .pl(px(12.0))
        .pr(px(8.0))
        .border_b_1()
        .border_color(gpui::rgb(t.line))
        .bg(gpui::rgb(t.raised))
        .text_size(ui_px(cx, UI_TEXT_SIZE))
        .text_color(gpui::rgb(t.text_2))
        .children(title)
        .child(div().flex_1())
        .child(header_controls(cx).children(controls))
        .into_any_element()
}

/// What a card standing in for popped-out content is found by: the card,
/// and its heading.
struct CardNames {
    card: String,
    label: String,
}

/// A card standing in for content a pop-out window draws: `label` over the
/// popped-out note and `buttons`.
fn popped_out_card(
    offset: i8,
    shift: i8,
    names: CardNames,
    label: String,
    buttons: [Stateful<Div>; 2],
    t: &UiTheme,
) -> AnyElement {
    let CardNames {
        card,
        label: label_name,
    } = names;
    let heading = div()
        .driver_id(|| label_name)
        .max_w_full()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(ui_px_at(offset, TITLE_TEXT_SIZE))
        .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
        .text_color(gpui::rgb(t.text))
        .child(label);
    let buttons = div().flex().justify_center().gap(px(6.0)).children(buttons);
    div()
        .driver_id(|| card)
        .flex()
        .flex_col()
        .size_full()
        .items_center()
        .justify_center()
        .gap(px(8.0))
        .px(px(12.0))
        .rounded(px(PANE_RADIUS))
        .border_1()
        .border_color(gpui::rgb(t.line))
        .bg(gpui::rgb(t.surface))
        .text_size(ui_px_at(offset, UI_TEXT_SIZE))
        .text_color(gpui::rgb(t.muted))
        .child(heading)
        .child(POPPED_OUT_NOTE)
        .child(buttons)
        .into_any_element()
}

/// The area a window's grid fills, around `content`, with the probe that
/// records where the window laid the grid out.
fn grid_frame(content: AnyElement, cx: &mut Context<RootView>) -> Div {
    div()
        .relative()
        .flex()
        .flex_1()
        .min_h(px(0.0))
        .child(bounds_probe(cx))
        .child(content)
}

/// A pop-out window's content: `toolbar` over `body`.
fn popout_column(toolbar: AnyElement, body: AnyElement) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(toolbar)
        .child(body)
        .into_any_element()
}

/// What a pane shows: the session it holds, or what it follows instead.
#[derive(Clone, Copy)]
struct PaneShown<'a> {
    session: Option<&'a str>,
    follow: Option<&'a PaneFollow>,
}

/// What a pane header builds: its ids, the session it shows with the header
/// parts it draws, what the pane follows instead, its buttons' tip place and
/// the window it draws in.
#[derive(Clone, Copy)]
struct HeaderTarget<'a> {
    tab_id: &'a str,
    pane_id: &'a str,
    session_id: Option<&'a str>,
    parts: Option<&'a PaneHeaderParts>,
    /// What the pane follows instead of a session, for its title.
    follow: Option<&'a PaneFollow>,
    /// What the pane shows of the dispatch run it follows.
    run: Option<RunShown<'a>>,
    /// The labels of the followed run's Stop, which stays in the row
    /// however narrow the header.
    run_stop: &'static [&'static str],
    at: TipAt,
    here: WindowId,
}

/// What a pane's header names: the session it shows with the parts it
/// draws, what it follows instead, or nothing for an empty pane.
enum PaneTitle<'a> {
    /// The session's parts.
    Session(&'a PaneHeaderParts),
    /// What the pane follows instead of a session, with the header of the
    /// dispatch run it follows when anything is known of the run.
    Follow(&'a PaneFollow, Option<RunHeader>),
    /// An empty pane.
    Empty,
}

impl PaneTitle<'_> {
    /// The text a drag of the pane carries: the session's or the run's
    /// title, else what the pane follows; nothing for an empty pane.
    fn label(&self) -> Option<&str> {
        match self {
            PaneTitle::Session(parts) => Some(parts.title.as_str()),
            PaneTitle::Follow(_, Some(run)) => Some(run.title.as_str()),
            PaneTitle::Follow(follow, None) => Some(follow_label(follow)),
            PaneTitle::Empty => None,
        }
    }
}

/// What a pane's header names, from the parts of the session it shows,
/// what it follows instead and what it shows of the dispatch run it
/// follows.
fn pane_title<'a>(
    parts: Option<&'a PaneHeaderParts>,
    follow: Option<&'a PaneFollow>,
    run: Option<RunShown<'_>>,
) -> PaneTitle<'a> {
    match (parts, follow) {
        (Some(parts), _) => PaneTitle::Session(parts),
        (None, Some(follow)) => PaneTitle::Follow(follow, run.and_then(|run| run_header(&run))),
        (None, None) => PaneTitle::Empty,
    }
}

/// What a pane that follows something is titled when it shows no session.
fn follow_label(follow: &PaneFollow) -> &'static str {
    match follow {
        PaneFollow::DispatchRun { .. } => "Dispatch run",
        PaneFollow::Subagent { .. } | PaneFollow::RunSubagent { .. } => "Subagent",
        PaneFollow::Unknown => "Unknown pane",
    }
}

/// What a pane header's cluster, Pop out to the prompt arrows, holds.
pub(crate) struct HeaderCluster {
    /// Pop out, when the session can attach: whether an open pop-out
    /// already shows the session, which dims it.
    pub(crate) pop_out: Option<bool>,
    /// Stop, its armed confirm or the exit code; `None` for an empty pane.
    pub(crate) stop: Option<StopSlot>,
    /// The Terminal / Chat / Summary switch, when the session offers a
    /// chat: the view shown and whether the agent is up.
    pub(crate) views: Option<(PaneView, bool)>,
    /// Compact, while the switch shows the chat: the chat mode.
    pub(crate) compact: Option<crate::chat::ChatMode>,
    /// Move to new tab, on a pane with a session.
    pub(crate) move_tab: bool,
    /// The prompt arrows, on a terminal: whether each has a prompt to
    /// scroll to.
    pub(crate) prompts: Option<(bool, bool)>,
}

/// What the header shows in Stop's place.
pub(crate) enum StopSlot {
    Stop,
    /// Confirm stop and Cancel.
    Armed,
    /// The stopped session's exit code, as the header spells it.
    ExitCode(String),
}

/// What the header's ⋯ button tips.
const OVERFLOW_TIP: &str = "More";

/// The width of a pane card's border, on each side.
const CARD_BORDER: f32 = 1.0;
/// How far below a header row its ⋯ menu opens.
const MENU_BELOW_HEADER: f32 = 2.0;

/// A header text button's padding on each side (`header_text_button`).
const HEADER_TEXT_BUTTON_PAD: f32 = 4.0;
/// A switch segment's or Compact's padding on each side.
const HEADER_SEGMENT_PAD: f32 = 8.0;
/// The gap between the switch's segments.
const HEADER_SEGMENT_GAP: f32 = 2.0;
/// A text chip's padding on each side.
const CHIP_PAD: f32 = 7.0;

/// The ⋯ button a narrow header shows in place of its cluster: a press
/// opens the menu of the cluster's buttons at `menu_at`. Its tip shows `at`.
fn overflow_button(
    tab_id: &str,
    pane_id: &str,
    menu_at: Point<Pixels>,
    at: TipAt,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let ids = (tab_id.to_owned(), pane_id.to_owned());
    header_button(pane_id, "pane-overflow", "", OVERFLOW_TIP, at, cx)
        .child(
            svg()
                .path(SIDEBAR_MORE_ICON)
                .size(px(15.0))
                .flex_none()
                .text_color(gpui::rgb(t.text_2)),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                this.press_on_header_control(event, window, cx);
                let (tab, pane) = &ids;
                this.open_pane_overflow_menu(tab, pane, menu_at, window, cx);
            }),
        )
        .into_any_element()
}

/// The width of each item a header puts between its chips and Close, in
/// header order: Pop out to the prompt arrows, a segmented switch counting
/// as one item.
fn cluster_item_widths(cluster: &HeaderCluster, cx: &App) -> Vec<f32> {
    let text = HeaderText { cx };
    let mut items = Vec::new();
    items.extend(cluster.pop_out.is_some().then(|| text.button("Pop out")));
    items.extend(cluster.stop.as_ref().map(|stop| stop_width(stop, &text)));
    items.extend(cluster.views.is_some().then(|| view_segments_width(&text)));
    items.extend(cluster.compact.is_some().then(|| text.segment("Compact")));
    items.extend([HEADER_BUTTON_SIZE, HEADER_BUTTON_SIZE]);
    items.extend(cluster.move_tab.then_some(HEADER_BUTTON_SIZE));
    if cluster.prompts.is_some() {
        items.extend([HEADER_BUTTON_SIZE, HEADER_BUTTON_SIZE]);
    }
    items
}

/// The width of the item a header draws in Stop's place: Stop's text
/// button, or the stopped session's exit code label.
fn stop_width(stop: &StopSlot, text: &HeaderText<'_>) -> f32 {
    text.button(match stop {
        StopSlot::ExitCode(label) => label,
        StopSlot::Stop | StopSlot::Armed => "Stop",
    })
}

/// The width of a session's Terminal / Chat / Summary switch: its three
/// segments and the gaps between them.
fn view_segments_width(text: &HeaderText<'_>) -> f32 {
    let views = [PaneView::Terminal, PaneView::Chat, PaneView::Summary];
    let labels: f32 = views
        .iter()
        .map(|view| text.segment(view_segment(*view).0))
        .sum();
    labels + 2.0 * HEADER_SEGMENT_GAP
}

/// Where a header's ⋯ menu opens on card `card`: its top right corner just
/// under the ⋯ button, which sits a card border and the row's right padding
/// in from the card's right edge.
fn overflow_menu_anchor(card: tabs::Rect, cx: &App) -> Point<Pixels> {
    let right = card.x + card.width
        - CARD_BORDER
        - pane_header_fit::PAD_RIGHT
        - HEADER_BUTTON_SIZE
        - pane_header_fit::GAP;
    let below = card.y + CARD_BORDER + ui_px(cx, PANE_HEADER_HEIGHT) / px(1.0) + MENU_BELOW_HEADER;
    point(px(right), px(below))
}

/// Text a pane header draws, measured in the font and size it draws it in.
struct HeaderText<'a> {
    cx: &'a App,
}

impl HeaderText<'_> {
    /// The width of `text` in the UI font at the header's text size.
    fn ui(&self, text: &str) -> f32 {
        text_width(
            self.cx,
            text,
            crate::fonts::ui_family(self.cx),
            crate::fonts::ui_weight(self.cx, FontWeight::NORMAL),
            ui_px(self.cx, UI_TEXT_SIZE),
        )
    }

    /// A header text button's width: `text` and its padding on each side.
    fn button(&self, text: &str) -> f32 {
        self.ui(text) + 2.0 * HEADER_TEXT_BUTTON_PAD
    }

    /// A switch segment's or Compact's width: `text` and its padding on
    /// each side.
    fn segment(&self, text: &str) -> f32 {
        self.ui(text) + 2.0 * HEADER_SEGMENT_PAD
    }
}

/// The width `text` takes in `family` at `weight` and `size`, in px: the
/// sum of its characters' advances.
fn text_width(
    cx: &App,
    text: &str,
    family: impl Into<SharedString>,
    weight: FontWeight,
    size: Pixels,
) -> f32 {
    let text_system = cx.text_system();
    let font_id = text_system.resolve_font(&gpui::Font {
        weight,
        ..gpui::font(family)
    });
    text.chars()
        .map(|ch| {
            text_system
                .advance(font_id, size, ch)
                .map_or(0.0, |advance| advance.width / px(1.0))
        })
        .sum()
}

/// The width `chip` takes in a header that has room for it: its icon's
/// square, or its text with padding and border.
/// What `target`'s header has to fit in `card`, with `cluster` as its
/// buttons: what both its collapse and its chips' floor are judged on.
fn header_fit(
    card: tabs::Rect,
    target: &HeaderTarget<'_>,
    cluster: &HeaderCluster,
    cx: &App,
) -> pane_header_fit::HeaderFit {
    pane_header_fit::HeaderFit {
        // The card's border takes a pixel on each side.
        width: card.width - 2.0 * CARD_BORDER,
        glyph: target.parts.map_or(0.0, |_| GlyphSize::Leaf.px()),
        chips: match (target.parts, target.run.and_then(|run| run_header(&run))) {
            (Some(parts), _) => parts
                .chips
                .iter()
                .map(|chip| chip_width(chip, cx))
                .collect(),
            (None, Some(run)) => run_chip_widths(&run, cx).to_vec(),
            (None, None) => Vec::new(),
        },
        cluster: cluster_item_widths(cluster, cx),
        fixed: target
            .run_stop
            .iter()
            .map(|label| HeaderText { cx }.button(label))
            .collect(),
        close: HEADER_BUTTON_SIZE,
    }
}

/// The widths a followed dispatch run's header gives its chips: the
/// `DS · backend · tier n` chip and the status word, as `run_title` draws
/// them.
fn run_chip_widths(run: &RunHeader, cx: &App) -> [f32; 2] {
    let chip = text_width(
        cx,
        &run.chip,
        DEFAULT_FAMILY,
        crate::fonts::mono_weight(),
        ui_px(cx, crate::dispatch_view::CHIP_TEXT_SIZE),
    );
    [chip, HeaderText { cx }.ui(run.status)]
}

fn chip_width(chip: &HeaderChip, cx: &App) -> f32 {
    let square = ui_px(cx, CHIP_HEIGHT) / px(1.0);
    match chip.kind {
        HeaderChipKind::Runtime if chip.icon.is_some() => square,
        _ => {
            let text = text_width(
                cx,
                &chip.text,
                DEFAULT_FAMILY,
                FontWeight::NORMAL,
                ui_px(cx, CHIP_TEXT_SIZE),
            );
            text + 2.0 * CHIP_PAD + 2.0
        }
    }
}

/// The header's Stop or exit code, in a row a left press on which never
/// starts a pane drag.
fn header_controls(cx: &mut Context<RootView>) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(RootView::press_on_header_control),
        )
}

/// A layer over the shown tab's grid that follows a drag-and-drop gesture
/// and takes its drop. It blocks no pointer, but while a drag is under way
/// it keeps the mouse from the terminals beneath it.
fn drop_layer(tab_id: &str, cx: &mut Context<RootView>) -> Stateful<Div> {
    div()
        .id(ElementId::Name(SharedString::from(format!(
            "pane-drop-layer-{tab_id}"
        ))))
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .on_drag_move(cx.listener(RootView::on_pane_drag_move))
        .on_drop(cx.listener(RootView::on_pane_drop))
        .on_drop(cx.listener(RootView::on_file_drop))
        .on_mouse_move(|_, _, cx| {
            if cx.has_active_drag() {
                cx.stop_propagation();
            }
        })
        .on_any_mouse_down(|_, _, cx| {
            if cx.has_active_drag() {
                cx.stop_propagation();
            }
        })
}

/// The tint over pane `pane_id` for a drop on `edge`, in the pane's
/// `accent`: a half, an outer strip, or the centre box with its badge,
/// drawn in the interface's family at its text size.
fn drop_overlay(
    pane_id: &str,
    edge: PaneDropEdge,
    accent: u32,
    cx: &App,
    t: &UiTheme,
) -> AnyElement {
    let family = crate::fonts::ui_family(cx);
    let overlay = drag::drop_overlay(edge, tabs::Rect::new(0.0, 0.0, 1.0, 1.0));
    let (alpha, border) = match overlay.kind {
        OverlayKind::Half | OverlayKind::Centre => (DROP_TINT_ALPHA, 1.0),
        OverlayKind::Outer => (OUTER_TINT_ALPHA, 2.0),
    };
    let name = format!("pane-drop-overlay-{pane_id}");
    let rect = overlay.rect;
    div()
        .driver_id(|| name)
        .absolute()
        .left(relative(rect.x))
        .top(relative(rect.y))
        .w(relative(rect.width))
        .h(relative(rect.height))
        .bg(gpui::Rgba {
            a: alpha,
            ..gpui::rgb(accent)
        })
        .border(px(border))
        .border_color(gpui::rgb(accent))
        .when(overlay.kind == OverlayKind::Centre, |centre| {
            centre
                .rounded(px(PANE_RADIUS))
                .flex()
                .items_center()
                .justify_center()
                .child(ghost_pill(
                    crate::fonts::ui_text_size(cx),
                    crate::fonts::ui_weight_shift(cx),
                    drag::SWAP_BADGE,
                    accent,
                    family,
                    t,
                ))
        })
        .into_any_element()
}

/// An invisible layer that records where the grid was laid out in the window
/// drawing it, for the divider drags and pane drops there.
fn bounds_probe(cx: &mut Context<RootView>) -> impl IntoElement {
    let root = cx.entity();
    canvas(
        move |bounds, window, cx| {
            let window_id = window.window_handle().window_id();
            root.update(cx, |this, cx| {
                if this.layouts.grid(window_id) != Some(bounds) {
                    this.layouts.set_grid(window_id, bounds);
                    // The pane headers decide whether they collapse from
                    // these bounds: the next frame draws them by the new ones.
                    cx.notify();
                }
            });
        },
        |_, (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{
        HeaderChip, HeaderChipKind, RetryGate, drawn_ratio, frame_band, header_fade_colors,
        header_fade_stops, header_parts, status_label,
    };
    use crate::appearance::term_rgb;
    use crate::runtime_icon::RuntimeIcon;
    use crate::tabs::SessionRef;
    use crate::target::HostKey;
    use crate::theme::mix;
    use protocol::{AgentDescriptor, AgentId, SessionMode, SessionSnapshot, SessionStatus};
    use serde_json::json;

    #[test]
    fn drawn_ratio_cases() {
        for (ratio, drawn) in [
            (0.0, 0.05),
            (-1.0, 0.05),
            (7.0, 0.95),
            (0.5, 0.5),
            (f32::NAN, 0.5),
            (f32::INFINITY, 0.5),
        ] {
            assert!(
                (drawn_ratio(ratio) - drawn).abs() < f32::EPSILON,
                "{ratio} draws as {}, not {drawn}",
                drawn_ratio(ratio)
            );
        }
    }

    const DEEPSEEK_PROVIDER_TIP: &str = "Runs on DeepSeek";

    #[test]
    fn status_labels() {
        assert_eq!(status_label(SessionStatus::Spawning, false), "starting");
        assert_eq!(status_label(SessionStatus::Idle, false), "idle");
        assert_eq!(status_label(SessionStatus::Working, false), "working");
        assert_eq!(
            status_label(SessionStatus::AwaitingInput, false),
            "awaiting input"
        );
        assert_eq!(status_label(SessionStatus::Stopped, false), "stopped");
        assert_eq!(status_label(SessionStatus::Error, false), "error");
    }

    #[test]
    fn a_focused_band_is_the_frame_else_the_accent() {
        let accent = term_rgb(0x00b4_8ead);
        let frame = term_rgb(0x0012_3456);
        assert_eq!(frame_band(true, Some(frame), accent), Some(frame));
        assert_eq!(frame_band(true, None, accent), Some(accent));
    }

    #[test]
    fn an_unfocused_band_draws_nothing_whatever_the_frame() {
        let accent = term_rgb(0x00b4_8ead);
        let frame = term_rgb(0x0012_3456);
        assert_eq!(frame_band(false, Some(frame), accent), None);
        assert_eq!(frame_band(false, None, accent), None);
    }

    #[test]
    fn a_header_fades_its_accent_over_its_own_fill() {
        let fill = 0x16_151a;
        let accent = 0x008f_b3f0;
        assert_eq!(
            header_fade_colors(fill, accent),
            (mix(term_rgb(fill), term_rgb(accent), 0.22), term_rgb(fill))
        );
    }

    #[test]
    fn the_fade_reaches_the_plain_fill_a_third_of_the_way_across() {
        let (start, end) = header_fade_stops(0x16_151a, 0x008f_b3f0);
        assert!((start.percentage - 0.0).abs() < f32::EPSILON);
        assert!(
            (end.percentage - 0.34).abs() < f32::EPSILON,
            "the wash ends at {} of the header, not 0.34",
            end.percentage
        );
    }

    /// Session `id` of This PC.
    fn on_this_pc(id: &str) -> SessionRef {
        SessionRef::new(HostKey::Local, id)
    }

    #[test]
    fn grid_retry_goes_out_once_per_session_and_attempt() {
        let mut gate = RetryGate::default();
        let (s1, s2) = (on_this_pc("s1"), on_this_pc("s2"));
        let first = gate.start(&s1);
        let retry = gate
            .claim(&s1, Some(&first), 2)
            .expect("the first report of a round");
        assert_ne!(retry, first, "every request gets its own id");
        assert_eq!(
            gate.claim(&s1, Some(&first), 2),
            None,
            "a second pane of the same session, still waiting on the first id"
        );
        assert_eq!(
            gate.claim(&s1, Some(&retry), 2),
            None,
            "a second pane of the same session, already told the retry's id"
        );
        let other = gate.start(&s2);
        assert!(gate.claim(&s2, Some(&other), 2).is_some());
        let next = gate.claim(&s1, Some(&retry), 3).expect("the next round");
        assert_ne!(next, retry);
        assert_eq!(
            gate.claim(&s1, Some(&next), 2),
            None,
            "a late report of an earlier round"
        );
        let again = gate.start(&s1);
        assert_ne!(again, first);
        assert!(
            gate.claim(&s1, Some(&again), 2).is_some(),
            "a fresh attach counts again"
        );
    }

    #[test]
    fn a_retry_from_before_a_reattach_is_ignored() {
        let mut gate = RetryGate::default();
        let s1 = on_this_pc("s1");
        let before = gate.start(&s1);
        let current = gate.start(&s1);
        assert_eq!(gate.claim(&s1, Some(&before), 2), None);
        assert_eq!(gate.claim(&s1, None, 2), None, "a pane that knows no id");
        assert!(
            gate.claim(&s1, Some(&current), 2).is_some(),
            "the ignored retry left the expected id alone"
        );
    }

    /// An interactive session run by `agent`, showing `chips`.
    fn session_with(agent: &str, chips: &serde_json::Value) -> SessionSnapshot {
        serde_json::from_value(json!({
            "id": "s1",
            "label": "s1",
            "kind": "single",
            "members": [],
            "status": "idle",
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": {
                "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0,
                "last_activity_at": null,
            },
            "recent_actions": [],
            "agent": agent,
            "agent_chips": chips,
        }))
        .expect("session fixture")
    }

    fn chip(kind: HeaderChipKind, text: &str, tip: &str) -> HeaderChip {
        HeaderChip::new(kind, text, tip)
    }

    /// A runtime chip drawn as the registry Claude's icon.
    fn runtime(text: &str) -> HeaderChip {
        let icon = claude_descriptor().icon.expect("Claude carries an icon");
        chip(HeaderChipKind::Runtime, text, &format!("Running {text}"))
            .with_icon(Some(RuntimeIcon::Agent(icon.into())))
    }

    /// The registry's Claude descriptor, as the daemon sends it.
    fn claude_descriptor() -> AgentDescriptor {
        agent_registry::descriptors()
            .into_iter()
            .find(|descriptor| descriptor.id == AgentId("claude".to_owned()))
            .expect("the registry has Claude")
    }

    #[test]
    fn a_deepseek_claude_session_shows_a_chip_after_the_runtime_one() {
        let deepseek = json!([{ "text": "DeepSeek", "tip": DEEPSEEK_PROVIDER_TIP }]);
        let parts = header_parts(
            &session_with("claude", &deepseek),
            false,
            &[claude_descriptor()],
            None,
        );
        assert_eq!(
            parts.chips,
            [
                runtime("claude"),
                chip(HeaderChipKind::Provider, "DeepSeek", DEEPSEEK_PROVIDER_TIP),
            ]
        );
    }

    #[test]
    fn an_anthropic_claude_session_shows_no_provider_chip() {
        let parts = header_parts(
            &session_with("claude", &json!([])),
            false,
            &[claude_descriptor()],
            None,
        );
        assert_eq!(parts.chips, [runtime("claude")]);
    }

    #[test]
    fn a_chip_without_a_tip_hovers_as_its_text() {
        let parts = header_parts(
            &session_with("claude", &json!([{ "text": "mistral", "tip": null }])),
            false,
            &[claude_descriptor()],
            None,
        );
        assert_eq!(
            parts.chips,
            [
                runtime("claude"),
                chip(HeaderChipKind::Provider, "mistral", "mistral"),
            ]
        );
    }

    #[test]
    fn a_plain_shell_shows_no_provider_chip() {
        let mut shell = session_with("claude", &json!([]));
        shell.mode = SessionMode::PlainShell;
        shell.program_name = Some("pwsh".to_owned());
        assert_eq!(
            header_parts(&shell, false, &[claude_descriptor()], None).chips,
            [chip(HeaderChipKind::Runtime, "pwsh", "Running pwsh")
                .with_icon(Some(RuntimeIcon::Pwsh))],
            "a plain shell says no provider and wears its shell's icon"
        );
    }
}
