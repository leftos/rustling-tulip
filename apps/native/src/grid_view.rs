//! The active tab's split tree: a terminal (or an empty placeholder) under a
//! small header in every pane, with draggable dividers between them. Also
//! keeps a terminal per pane attached to the pane's session.

use std::collections::{BTreeSet, HashMap, HashSet};

use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Div, ElementId, Entity, FocusHandle, MouseButton,
    MouseDownEvent, Pixels, SharedString, Stateful, Subscription, Window, canvas, div, prelude::*,
    px, relative,
};
use protocol::{
    ClientMessage, GridNode, SessionSnapshot, SplitDirection, SplitPlace, TabContent, TabEntry,
};

use crate::appearance::{self, PaneFrame, Resolved};
use crate::diff_tab::LOADING_TEXT as DIFF_LOADING_TEXT;
use crate::session_menu::{BorderedButton, bordered_button};
use crate::shell_dialog::standalone_shell_request;
use crate::sidebar::can_attach;
use crate::spawn_view::SpawnEntry;
use crate::spawns::{OpenIn, PaneAim};
use crate::tabs::{self, PaneBinding, TabsModel};
use crate::term_view::{PaneEvent, ScrollbackReply, TerminalPane};
use crate::{
    BAR_BG, BORDER, DIVIDER_WIDTH, Drag, HOVER_BG, MUTED, RootView, TEXT, UI_TEXT_SIZE,
    drag_handle, new_request_id, tooltip,
};

/// Why a repo-tied spawn button is disabled.
pub(crate) const NO_REPOS_TIP: &str = "Register a repo to spawn repo-tied sessions.";
/// Why a pane's spawn buttons are disabled while a spawn aimed at it waits.
pub(crate) const PANE_PENDING_TIP: &str = "A new session is on its way to this pane";
const PANE_SPAWN_TIP: &str = "Open the spawn dialog; the new session fills this pane";
const PANE_SHELL_TIP: &str = "A plain shell in this pane, in the remembered folder";
pub(crate) const SPAWN_TIP: &str = "Spawn a new session";
const OPEN_SHELL_TIP: &str = "A plain shell in a new tab, in the remembered folder";

const PANE_HEADER_HEIGHT: f32 = 20.0;
/// The accent line down a pane's left edge.
const ACCENT_LINE_WIDTH: f32 = 3.0;

/// A pane's terminal and the session it shows.
pub(crate) struct PaneSlot {
    view: Entity<TerminalPane>,
    tab_id: String,
    /// The pane's session; attached once the session list names it.
    session: Option<String>,
    attached: bool,
    /// The accent the pane's last apply resolved, `0xRRGGBB`.
    accent: u32,
    _focus_in: Subscription,
    _events: Subscription,
}

impl PaneSlot {
    pub(crate) fn view(&self) -> &Entity<TerminalPane> {
        &self.view
    }

    /// The session the pane shows.
    pub(crate) fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }
}

/// One scrollback retry per session and attempt, and a fresh request id for
/// every `LoadScrollback`. The panes showing a session time out together,
/// and every `LoadScrollback` restarts the daemon's output stream for it, so
/// only the first report goes out.
#[derive(Debug, Default)]
pub(crate) struct RetryGate {
    rounds: HashMap<String, Round>,
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
    pub(crate) fn start(&mut self, session_id: &str) -> String {
        let request_id = new_request_id();
        self.rounds.insert(
            session_id.to_owned(),
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
        session_id: &str,
        request_id: Option<&str>,
        attempt: usize,
    ) -> Option<String> {
        let round = self.rounds.get_mut(session_id)?;
        if request_id != Some(round.request_id.as_str()) || round.attempt >= attempt {
            return None;
        }
        round.request_id = new_request_id();
        round.attempt = attempt;
        Some(round.request_id.clone())
    }
}

/// What a pane shows while its session has no terminal attached.
fn unattached_note(session: Option<&SessionSnapshot>) -> &'static str {
    match session {
        Some(session) if !can_attach(session) => "Headless session (no terminal)",
        _ => "Waiting for session…",
    }
}

/// The ratio the split at `split_path` of `tab_id` takes with the pointer
/// at `at`, when the tab has been laid out in `bounds`.
pub(crate) fn divider_ratio(
    model: &TabsModel,
    bounds: Option<Bounds<Pixels>>,
    tab_id: &str,
    split_path: &[u8],
    at: (f32, f32),
) -> Option<f32> {
    let bounds = bounds?;
    let grid = model.tab(tab_id)?.grid()?;
    let rect = tabs::Rect::new(
        bounds.origin.x / px(1.0),
        bounds.origin.y / px(1.0),
        bounds.size.width / px(1.0),
        bounds.size.height / px(1.0),
    );
    let divider = tabs::dividers(grid, rect, DIVIDER_WIDTH)
        .into_iter()
        .find(|d| d.split_path == split_path)?;
    Some(tabs::ratio_at(&divider, at.0, at.1))
}

/// The pane that answers each session's terminal queries, by session: its
/// size driver, whose grid matches the PTY, else the first pane in layout
/// order that shows it. Every pane showing a session sees its queries, and
/// each answer reaches the child as input, so only one pane answers.
fn query_answerers(
    bindings: &[PaneBinding],
    drivers: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut answerers = drivers.clone();
    for binding in bindings {
        if let Some(session_id) = &binding.session_id {
            answerers
                .entry(session_id.clone())
                .or_insert_with(|| binding.pane_id.clone());
        }
    }
    answerers
}

impl RootView {
    /// Brings the terminals in line with the tab model: one per pane,
    /// attached to the pane's session and sizing the PTY only in the active
    /// tab. `Detach` goes out once no pane shows a session; the panes still
    /// showing a session that lost one re-attach.
    pub(crate) fn reconcile_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let before = tabs::view_counts(
            self.panes
                .values()
                .filter_map(|slot| slot.session.as_deref()),
        );
        let bindings = self.tabs.bindings();
        self.drop_removed_panes(&bindings, cx);
        let mut to_sync = self.bind_panes(bindings, window, cx);
        let changes = tabs::session_changes(&before, &self.tabs.view_counts());
        for session_id in changes.detach {
            self.send(ClientMessage::Detach { session_id });
        }
        to_sync.extend(changes.resync);
        for session_id in to_sync {
            self.sync_session(&session_id, cx);
        }
        self.update_pane_roles(cx);
    }

    fn drop_removed_panes(&mut self, bindings: &[PaneBinding], cx: &mut Context<Self>) {
        let live: HashSet<&str> = bindings.iter().map(|b| b.pane_id.as_str()).collect();
        self.panes.retain(|pane_id, slot| {
            let keep = live.contains(pane_id.as_str());
            if !keep {
                slot.view.update(cx, |pane, _| pane.release());
            }
            keep
        });
    }

    /// Gives every pane a terminal and records its session; returns the
    /// sessions a pane newly shows.
    fn bind_panes(
        &mut self,
        bindings: Vec<PaneBinding>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> BTreeSet<String> {
        let mut shown = BTreeSet::new();
        for binding in bindings {
            if !self.panes.contains_key(&binding.pane_id) {
                let slot = self.new_slot(&binding.pane_id, window, cx);
                self.panes.insert(binding.pane_id.clone(), slot);
            }
            let Some(slot) = self.panes.get_mut(&binding.pane_id) else {
                continue;
            };
            slot.tab_id = binding.tab_id;
            if slot.session != binding.session_id {
                slot.view.update(cx, |pane, _| pane.release());
                slot.attached = false;
                slot.session = binding.session_id;
                shown.extend(slot.session.clone());
            }
        }
        shown
    }

    fn new_slot(&self, pane_id: &str, window: &mut Window, cx: &mut Context<Self>) -> PaneSlot {
        let (tx, now) = (self.tx.clone(), self.now.clone());
        let font = self.sidebar.ui_state().terminal_font.clone();
        let view = cx.new(|cx| TerminalPane::new(pane_id.to_owned(), tx, now, font, cx));
        let handle = view.read(cx).focus_handle();
        let id = pane_id.to_owned();
        let focus_in = cx.on_focus_in(&handle, window, move |this, window, cx| {
            this.pane_focused(&id, window, cx);
            cx.notify();
        });
        let events = cx.subscribe_in(&view, window, |this, _, event: &PaneEvent, window, cx| {
            this.on_pane_event(event, window, cx);
        });
        PaneSlot {
            view,
            tab_id: String::new(),
            session: None,
            attached: false,
            accent: appearance::BUILTIN_ACCENT,
            _focus_in: focus_in,
            _events: events,
        }
    }

    pub(crate) fn pane_focused(
        &mut self,
        pane_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab_id) = self.panes.get(pane_id).map(|slot| slot.tab_id.clone()) {
            self.tabs.set_focused(&tab_id, pane_id);
            self.update_pane_roles(cx);
            self.drop_stale_sc_picker();
            self.seed_source_control(window, cx);
        }
    }

    /// Attaches every pane showing the session afresh and asks for its
    /// scrollback once: the daemon keeps one output stream per session for
    /// this connection, restarted by each request, so the panes share one.
    fn sync_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let Some(session) = self
            .sidebar
            .session(session_id)
            .filter(|s| can_attach(s))
            .cloned()
        else {
            return;
        };
        let request_id = self.retries.start(session_id);
        let mut attached = false;
        for slot in self
            .panes
            .values_mut()
            .filter(|slot| slot.session.as_deref() == Some(session_id))
        {
            slot.view.update(cx, |pane, cx| pane.attach(&session, cx));
            slot.attached = true;
            attached = true;
        }
        if attached {
            self.request_scrollback(session_id, request_id, cx);
        }
    }

    /// Sends `LoadScrollback` for the session under `request_id`, and tells
    /// every pane showing it that only this request's reply counts.
    fn request_scrollback(&mut self, session_id: &str, request_id: String, cx: &mut Context<Self>) {
        for view in self.pane_views(Some(session_id)) {
            view.update(cx, |pane, _| pane.expect_scrollback(&request_id));
        }
        self.send(ClientMessage::LoadScrollback {
            session_id: session_id.to_owned(),
            request_id: Some(request_id),
        });
    }

    /// Tells every pane whether it drives its session's PTY size and whether
    /// it answers the session's terminal queries.
    fn update_pane_roles(&self, cx: &mut Context<Self>) {
        let active = self.tabs.active_id();
        let focused = active.and_then(|tab_id| self.tabs.focused_pane(tab_id));
        let bindings = self.tabs.bindings();
        let drivers = tabs::size_drivers(&bindings, active, focused.as_deref());
        let answerers = query_answerers(&bindings, &drivers);
        for (pane_id, slot) in &self.panes {
            let holds = |roles: &HashMap<String, String>| {
                slot.session
                    .as_ref()
                    .is_some_and(|session| roles.get(session) == Some(pane_id))
            };
            let (drives, answers) = (holds(&drivers), holds(&answerers));
            slot.view.update(cx, |pane, _| {
                pane.set_drives_size(drives);
                pane.set_answers_queries(answers);
            });
        }
    }

    /// Gives every pane the font and colours it resolves to: each field its
    /// session's, else its container's, else the app's, and for the size
    /// its tab's override above all of them.
    pub(crate) fn apply_pane_fonts(&mut self, cx: &mut Context<Self>) {
        self.apply_fonts_where(|_| true, cx);
    }

    /// Gives the panes showing `session_id` the font and colours they
    /// resolve to, as a snapshot that moved that session's appearance must.
    pub(crate) fn apply_session_pane_fonts(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.apply_fonts_where(|slot| slot.session.as_deref() == Some(session_id), cx);
    }

    /// Gives every pane `keep` takes the font and colours it resolves to.
    fn apply_fonts_where(&mut self, keep: impl Fn(&PaneSlot) -> bool, cx: &mut Context<Self>) {
        let resolved: Vec<(String, Resolved)> = self
            .panes
            .iter()
            .filter(|(_, slot)| keep(slot))
            .map(|(pane_id, slot)| {
                let resolved = self.pane_appearance(&slot.tab_id, slot.session.as_deref());
                (pane_id.clone(), resolved)
            })
            .collect();
        for (pane_id, resolved) in resolved {
            let Some(slot) = self.panes.get_mut(&pane_id) else {
                continue;
            };
            slot.accent = resolved.accent.value;
            let background = appearance::term_rgb(resolved.background.value);
            let frame = resolved.frame.value.map(appearance::term_rgb);
            slot.view.update(cx, |pane, cx| {
                pane.set_font(resolved.font(), cx);
                pane.set_colors(background, frame, cx);
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
    pub fn pane_frame_colors(&self, pane_id: &str) -> Option<PaneFrame> {
        let (tab_id, session) = self.tabs.tabs().iter().find_map(|tab| {
            let pane = tabs::collect_panes(tab.grid()?)
                .into_iter()
                .find(|pane| pane.id == pane_id)?;
            Some((tab.id.as_str(), pane.session))
        })?;
        let focused = self.tabs.focused_pane(tab_id).as_deref() == Some(pane_id);
        Some(self.frame_colors(pane_id, session, focused))
    }

    /// The colours pane `pane_id`, showing `session`, paints on its edges,
    /// from the accent its last apply resolved; a pane without a terminal
    /// yet resolves its session's.
    fn frame_colors(&self, pane_id: &str, session: Option<&str>, focused: bool) -> PaneFrame {
        let accent = self.panes.get(pane_id).map_or_else(
            || self.sidebar.appearance(session).accent.value,
            |slot| slot.accent,
        );
        PaneFrame {
            border: if focused { accent } else { BORDER },
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

    /// A pane's scrollback retry, sent once per session and attempt; a
    /// pane's copy, which the root's clipboard write and chip answer; or a
    /// pane's Ctrl+clicked link, which opens.
    fn on_pane_event(&mut self, event: &PaneEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            PaneEvent::ScrollbackRetry {
                session_id,
                attempt,
                request_id,
            } => {
                if let Some(request_id) =
                    self.retries
                        .claim(session_id, request_id.as_deref(), *attempt)
                {
                    tracing::warn!(
                        "scrollback request for session {session_id} timed out; retry {attempt}"
                    );
                    self.request_scrollback(session_id, request_id, cx);
                }
            }
            PaneEvent::Copied { text } => self.copy_to_clipboard(text, cx),
            PaneEvent::OpenLink { link, base_dirs } => {
                self.open_link(link, base_dirs.clone(), window, cx);
            }
            PaneEvent::ShellDotMenu { pane_id, index, at } => {
                self.open_shell_menu(pane_id, *index, *at, window, cx);
            }
        }
    }

    /// Attaches panes whose session the session list did not name before.
    pub(crate) fn attach_waiting_panes(&mut self, cx: &mut Context<Self>) {
        let waiting: BTreeSet<String> = self
            .panes
            .values()
            .filter(|slot| !slot.attached)
            .filter_map(|slot| slot.session.clone())
            .collect();
        for session_id in waiting {
            self.sync_session(&session_id, cx);
        }
    }

    /// A new connection starts with nothing attached; the next tab list
    /// attaches every pane again.
    pub(crate) fn reset_panes(&mut self, cx: &mut Context<Self>) {
        for slot in self.panes.values_mut() {
            slot.view.update(cx, |pane, cx| {
                pane.reset_for_reconnect();
                cx.notify();
            });
            slot.session = None;
            slot.attached = false;
        }
    }

    /// The terminals showing `session_id`, or every terminal.
    pub(crate) fn pane_views(&self, session_id: Option<&str>) -> Vec<Entity<TerminalPane>> {
        self.panes
            .values()
            .filter(|slot| session_id.is_none() || slot.session.as_deref() == session_id)
            .map(|slot| slot.view.clone())
            .collect()
    }

    /// Hands a session's scrollback reply to every pane showing it.
    pub(crate) fn feed_scrollback(&mut self, reply: &ScrollbackReply, cx: &mut Context<Self>) {
        self.feed_panes(&reply.session_id, cx, |pane, cx| {
            pane.on_scrollback(reply, cx)
        });
    }

    /// Hands live output for `session_id` to every pane showing it.
    pub(crate) fn feed_output(&mut self, session_id: &str, data_b64: &str, cx: &mut Context<Self>) {
        self.feed_panes(session_id, cx, |pane, cx| pane.on_pty_output(data_b64, cx));
    }

    /// Hands daemon output for `session_id` to every pane showing it.
    pub(crate) fn feed_panes(
        &mut self,
        session_id: &str,
        cx: &mut Context<Self>,
        mut feed: impl FnMut(
            &mut TerminalPane,
            &mut Context<TerminalPane>,
        ) -> Result<(), base64::DecodeError>,
    ) {
        for view in self.pane_views(Some(session_id)) {
            let fed = view.update(cx, |pane, cx| {
                cx.notify();
                feed(pane, cx)
            });
            if let Err(err) = fed {
                self.status = format!("bad base64 from daemon: {err}");
            }
        }
    }

    /// Whether a pane showing a session has the keyboard.
    pub(crate) fn terminal_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.panes
            .values()
            .any(|slot| slot.session.is_some() && slot.view.read(cx).is_focused(window))
    }

    pub(crate) fn focus_pane_view(&self, pane_id: &str, window: &mut Window, cx: &Context<Self>) {
        if let Some(slot) = self.panes.get(pane_id) {
            slot.view.read(cx).focus(window);
        }
    }

    /// Gives the keyboard to the active tab: its focused pane, or a diff
    /// tab's view.
    pub(crate) fn focus_active_pane(&self, window: &mut Window, cx: &Context<Self>) {
        if self.focus_active_diff_tab(window, cx) {
            return;
        }
        if let Some(pane_id) = self
            .tabs
            .active_id()
            .and_then(|tab_id| self.tabs.focused_pane(tab_id))
        {
            self.focus_pane_view(&pane_id, window, cx);
        }
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

    fn split_pane(&self, tab_id: &str, pane_id: &str, direction: SplitDirection, before: bool) {
        self.send(ClientMessage::SplitPane {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
            direction,
            place: if before {
                SplitPlace::First
            } else {
                SplitPlace::Second
            },
            new_session_id: None,
        });
    }

    /// Whether any repo is registered, which a repo-tied spawn needs.
    #[must_use]
    pub fn has_repos(&self) -> bool {
        !self.sidebar.repos().is_empty()
    }

    /// The panes of the tab on screen that show no session.
    #[must_use]
    pub fn empty_pane_ids(&self) -> Vec<String> {
        self.active_panes(|session| session.is_none())
    }

    /// Whether the main area with no tab open offers its spawn choices: only
    /// once the connection is open and the tab list and the repo list have
    /// arrived, so a repo-less hint never flashes before the repos do.
    #[must_use]
    pub fn no_tab_choices_shown(&self) -> bool {
        self.tabs.active_tab().is_none()
            && self.conn.is_open()
            && self.tabs.is_loaded()
            && self.sidebar.repos_loaded()
    }

    fn active_panes(&self, keep: impl Fn(Option<&str>) -> bool) -> Vec<String> {
        self.tabs
            .active_tab()
            .and_then(TabEntry::grid)
            .map(|grid| {
                tabs::collect_panes(grid)
                    .into_iter()
                    .filter(|pane| keep(pane.session))
                    .map(|pane| pane.id.to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Gives pane `pane_id` of `tab_id` its tab's focus and the keyboard.
    fn focus_pane_in(
        &mut self,
        tab_id: &str,
        pane_id: &str,
        window: &mut Window,
        cx: &Context<Self>,
    ) {
        self.tabs.set_focused(tab_id, pane_id);
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
        let aim = PaneAim {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
            expected: replacing.map(str::to_owned),
            discard: replacing.map(str::to_owned),
        };
        self.open_spawn_dialog(SpawnEntry::Pane { aim, preselect }, window, cx);
        cx.notify();
    }

    /// An empty pane's "Shell here": the quick shell, into this pane.
    /// Nothing while a spawn aimed at the pane is on its way.
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
        self.focus_pane_in(tab_id, pane_id, window, cx);
        let cwd = self.sidebar.quick_shell_dir().map(str::to_owned);
        let open_in = OpenIn::Pane(PaneAim {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
            expected: None,
            discard: None,
        });
        self.spawn(standalone_shell_request(cwd), open_in, cx);
        cx.notify();
    }

    /// The main area with no tab open: "Spawn a session" and "Open shell",
    /// and why the first is disabled when no repo is registered. Before the
    /// connection, the tabs and the repos are in, only "No tab open".
    fn no_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        if !self.no_tab_choices_shown() {
            return muted_note("No tab open").into_any_element();
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
        spawn_choices("No tab open", [spawn, shell])
            .when(!has_repos, |note| {
                note.child(
                    div()
                        .debug_selector(|| "empty-repo-hint".to_owned())
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
        let has_repos = self.has_repos();
        let pending = self.spawns.aims_at(tab_id, pane_id);
        let spawn = BorderedButton {
            selector: format!("empty-pane-new-session-{pane_id}"),
            label: "New session…",
            tip: if !has_repos {
                NO_REPOS_TIP
            } else if pending {
                PANE_PENDING_TIP
            } else {
                PANE_SPAWN_TIP
            },
            enabled: has_repos && !pending,
        };
        let ids = (tab_id.to_owned(), pane_id.to_owned());
        let spawn = bordered_button(spawn, cx, move |this, window, cx| {
            this.new_session_in_pane(&ids.0, &ids.1, None, window, cx);
        });
        let shell = BorderedButton {
            selector: format!("empty-pane-shell-{pane_id}"),
            label: "Shell here",
            tip: if pending {
                PANE_PENDING_TIP
            } else {
                PANE_SHELL_TIP
            },
            enabled: !pending,
        };
        let ids = (tab_id.to_owned(), pane_id.to_owned());
        let shell = bordered_button(shell, cx, move |this, window, cx| {
            this.shell_in_pane(&ids.0, &ids.1, window, cx);
        });
        let body = spawn_choices("No session", [spawn, shell]);
        match focus {
            Some(handle) => body
                .track_focus(&handle)
                .on_mouse_down(MouseButton::Left, move |_, window, _| {
                    handle.focus(window);
                })
                .into_any_element(),
            None => body.into_any_element(),
        }
    }

    /// The active tab: its split tree, a notice for a diff tab, or the
    /// spawn choices when there is no tab.
    pub(crate) fn grid_area(&self, cx: &mut Context<Self>) -> Div {
        let content = match self.tabs.active_tab() {
            None => self.no_tab(cx),
            Some(tab) => match &tab.content {
                TabContent::Diff { .. } => self
                    .diff_tab_element(&tab.id)
                    .unwrap_or_else(|| muted_note(DIFF_LOADING_TEXT).into_any_element()),
                TabContent::Grid { grid } => self.render_node(&tab.id, grid, &mut Vec::new(), cx),
            },
        };
        div()
            .relative()
            .flex()
            .flex_1()
            .min_h(px(0.0))
            .child(bounds_probe(cx))
            .child(content)
    }

    fn render_node(
        &self,
        tab_id: &str,
        node: &GridNode,
        path: &mut Vec<u8>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            GridNode::Pane {
                pane_id,
                session_id,
            } => self.render_pane(tab_id, pane_id, session_id.as_deref(), cx),
            GridNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                path.push(0);
                let first = self.render_node(tab_id, first, path, cx);
                path.pop();
                path.push(1);
                let second = self.render_node(tab_id, second, path, cx);
                path.pop();
                let divider = self.pane_divider(tab_id, path, *direction, cx);
                split_box(*direction, *ratio, [first, divider, second])
            }
        }
    }

    fn pane_divider(
        &self,
        tab_id: &str,
        path: &[u8],
        direction: SplitDirection,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = matches!(
            &self.drag,
            Some(Drag::Divider { tab_id: t, split_path, .. }) if t == tab_id && split_path == path
        );
        let name = format!("divider-{tab_id}-{path:?}");
        let id = ElementId::Name(SharedString::from(name.clone()));
        let (tab_id, split_path) = (tab_id.to_owned(), path.to_vec());
        drag_handle(id, direction == SplitDirection::Horizontal, active)
            .debug_selector(|| name)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.drag = Some(Drag::Divider {
                        tab_id: tab_id.clone(),
                        split_path: split_path.clone(),
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
        session_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focused = self.tabs.focused_pane(tab_id).as_deref() == Some(pane_id);
        let body = match (session_id, self.panes.get(pane_id)) {
            (Some(_), Some(slot)) if slot.attached => div()
                .flex_1()
                .min_h(px(0.0))
                .debug_selector(|| format!("pane-grid-{pane_id}"))
                .child(slot.view.clone())
                .into_any_element(),
            (Some(id), _) => {
                muted_note(unattached_note(self.sidebar.session(id))).into_any_element()
            }
            (None, Some(slot)) => {
                let handle = slot.view.read(cx).focus_handle();
                self.empty_pane(tab_id, pane_id, Some(handle), cx)
            }
            (None, None) => self.empty_pane(tab_id, pane_id, None, cx),
        };
        let body = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(body)
            .children(self.exited_overlay(tab_id, pane_id, session_id, cx));
        let colors = self.frame_colors(pane_id, session_id, focused);
        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .border_1()
            .border_color(gpui::rgb(colors.border))
            .child(self.pane_header(tab_id, pane_id, session_id, cx))
            .child(body)
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(ACCENT_LINE_WIDTH))
                    .bg(gpui::rgb(colors.accent_line)),
            )
            .into_any_element()
    }

    /// The session's name, its Stop or exit code, split right and down
    /// (Shift: left and up), and close. A right-click opens the session's
    /// menu.
    fn pane_header(
        &self,
        tab_id: &str,
        pane_id: &str,
        session_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Div {
        let label = session_id.map_or_else(|| "Empty pane".to_owned(), |id| self.session_label(id));
        let ids = (tab_id.to_owned(), pane_id.to_owned());
        let (right, down, close) = (ids.clone(), ids.clone(), ids);
        let menu_session = session_id.map(str::to_owned);
        let name = format!("pane-header-{pane_id}");
        div()
            .debug_selector(|| name)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if let Some(id) = &menu_session {
                        this.open_session_menu(id, event.position, window, cx);
                        cx.stop_propagation();
                    }
                }),
            )
            .flex()
            .flex_none()
            .items_center()
            .gap(px(2.0))
            .h(px(PANE_HEADER_HEIGHT))
            .px(px(6.0))
            .bg(gpui::rgb(BAR_BG))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(MUTED))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(label),
            )
            .children(self.header_stop(pane_id, session_id, cx))
            .child(
                header_button(pane_id, "split-right", "│", "Split right (Shift: left)").on_click(
                    cx.listener(move |this, event: &ClickEvent, _, _| {
                        let (tab, pane) = &right;
                        this.split_pane(
                            tab,
                            pane,
                            SplitDirection::Horizontal,
                            event.modifiers().shift,
                        );
                    }),
                ),
            )
            .child(
                header_button(pane_id, "split-down", "─", "Split down (Shift: up)").on_click(
                    cx.listener(move |this, event: &ClickEvent, _, _| {
                        let (tab, pane) = &down;
                        this.split_pane(
                            tab,
                            pane,
                            SplitDirection::Vertical,
                            event.modifiers().shift,
                        );
                    }),
                ),
            )
            .child(
                header_button(pane_id, "close-pane", "×", "Close pane").on_click(cx.listener(
                    move |this, _: &ClickEvent, _, _| {
                        let (tab_id, pane_id) = close.clone();
                        this.send(ClientMessage::ClosePane { tab_id, pane_id });
                    },
                )),
            )
    }
}

/// A split's two children around their divider, the first sized by `ratio`.
fn split_box(direction: SplitDirection, ratio: f32, parts: [AnyElement; 3]) -> AnyElement {
    let [first, divider, second] = parts;
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

/// Muted text centred in the space it is given.
fn muted_note(text: &'static str) -> Div {
    div()
        .flex()
        .flex_1()
        .size_full()
        .items_center()
        .justify_center()
        .text_size(px(UI_TEXT_SIZE))
        .text_color(gpui::rgb(MUTED))
        .child(text)
}

/// A muted `heading` over a row of `buttons`, centred in the space it is
/// given.
fn spawn_choices(heading: &'static str, buttons: [Stateful<Div>; 2]) -> Div {
    muted_note(heading).flex_col().gap(px(8.0)).child(
        div()
            .flex()
            .flex_wrap()
            .justify_center()
            .gap(px(6.0))
            .children(buttons),
    )
}

/// A small glyph button in a pane header.
fn header_button(
    pane_id: &str,
    action: &str,
    glyph: &'static str,
    tip: &'static str,
) -> Stateful<Div> {
    let name = format!("{action}-{pane_id}");
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .px(px(4.0))
        .rounded(px(3.0))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT)))
        .tooltip(tooltip(tip))
        .child(glyph)
}

/// An invisible layer that records where the grid was laid out, for the
/// divider drags.
fn bounds_probe(cx: &mut Context<RootView>) -> impl IntoElement {
    let root = cx.entity();
    canvas(
        move |bounds, _, cx| {
            root.update(cx, |this, _| this.grid_bounds = Some(bounds));
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
    use super::{RetryGate, unattached_note};
    use protocol::{SessionMode, SessionSnapshot};
    use serde_json::json;

    fn session(mode: SessionMode) -> SessionSnapshot {
        let mut s: SessionSnapshot = serde_json::from_value(json!({
            "id": "s1",
            "label": "s1",
            "kind": "single",
            "members": [],
            "status": "idle",
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": null },
            "recent_actions": [],
            "agent": "claude",
        }))
        .expect("session fixture");
        s.mode = mode;
        s
    }

    #[test]
    fn grid_retry_goes_out_once_per_session_and_attempt() {
        let mut gate = RetryGate::default();
        let first = gate.start("s1");
        let retry = gate
            .claim("s1", Some(&first), 2)
            .expect("the first report of a round");
        assert_ne!(retry, first, "every request gets its own id");
        assert_eq!(
            gate.claim("s1", Some(&first), 2),
            None,
            "a second pane of the same session, still waiting on the first id"
        );
        assert_eq!(
            gate.claim("s1", Some(&retry), 2),
            None,
            "a second pane of the same session, already told the retry's id"
        );
        let other = gate.start("s2");
        assert!(gate.claim("s2", Some(&other), 2).is_some());
        let next = gate.claim("s1", Some(&retry), 3).expect("the next round");
        assert_ne!(next, retry);
        assert_eq!(
            gate.claim("s1", Some(&next), 2),
            None,
            "a late report of an earlier round"
        );
        let again = gate.start("s1");
        assert_ne!(again, first);
        assert!(
            gate.claim("s1", Some(&again), 2).is_some(),
            "a fresh attach counts again"
        );
    }

    #[test]
    fn a_retry_from_before_a_reattach_is_ignored() {
        let mut gate = RetryGate::default();
        let before = gate.start("s1");
        let current = gate.start("s1");
        assert_eq!(gate.claim("s1", Some(&before), 2), None);
        assert_eq!(gate.claim("s1", None, 2), None, "a pane that knows no id");
        assert!(
            gate.claim("s1", Some(&current), 2).is_some(),
            "the ignored retry left the expected id alone"
        );
    }

    #[test]
    fn grid_unattached_pane_says_why() {
        assert_eq!(
            unattached_note(Some(&session(SessionMode::Headless))),
            "Headless session (no terminal)"
        );
        assert_eq!(unattached_note(None), "Waiting for session…");
        assert_eq!(
            unattached_note(Some(&session(SessionMode::Interactive))),
            "Waiting for session…"
        );
    }
}
