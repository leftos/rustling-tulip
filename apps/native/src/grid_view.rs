//! The active tab's split tree: a terminal (or an empty placeholder) under a
//! small header in every pane, with draggable dividers between them. Also
//! keeps a terminal per pane attached to the pane's session.

use std::collections::{BTreeSet, HashMap, HashSet};

use gpui::{
    AnyElement, Bounds, ClickEvent, Context, Div, ElementId, Entity, MouseButton, MouseDownEvent,
    Pixels, SharedString, Stateful, Subscription, Window, canvas, div, prelude::*, px, relative,
};
use protocol::{ClientMessage, GridNode, SessionSnapshot, SplitDirection, SplitPlace, TabContent};

use crate::sidebar::can_attach;
use crate::tabs::{self, PaneBinding, TabsModel};
use crate::term_view::{PaneEvent, TerminalPane};
use crate::{
    BAR_BG, BORDER, DIVIDER_WIDTH, Drag, HOVER_BG, MUTED, RootView, TEXT, UI_TEXT_SIZE,
    drag_handle, tooltip,
};

const PANE_HEADER_HEIGHT: f32 = 20.0;
/// The border of the pane that has its tab's focus.
const FOCUS_BORDER: u32 = 0x0045_6a9a;

/// A pane's terminal and the session it shows.
pub(crate) struct PaneSlot {
    view: Entity<TerminalPane>,
    tab_id: String,
    /// The pane's session; attached once the session list names it.
    session: Option<String>,
    attached: bool,
    _focus_in: Subscription,
    _events: Subscription,
}

impl PaneSlot {
    pub(crate) fn view(&self) -> &Entity<TerminalPane> {
        &self.view
    }
}

/// One scrollback retry per session and attempt. The panes showing a
/// session time out together, and every `LoadScrollback` restarts the
/// daemon's output stream for it, so only the first report goes out.
#[derive(Debug, Default)]
pub(crate) struct RetryGate {
    /// The last attempt sent, by session.
    sent: HashMap<String, usize>,
}

impl RetryGate {
    /// Whether a pane's retry `attempt` for the session is the one to send.
    pub(crate) fn claim(&mut self, session_id: &str, attempt: usize) -> bool {
        if self
            .sent
            .get(session_id)
            .is_some_and(|&last| last >= attempt)
        {
            return false;
        }
        self.sent.insert(session_id.to_owned(), attempt);
        true
    }

    /// A fresh attach of the session counts its retries from the start.
    pub(crate) fn reset(&mut self, session_id: &str) {
        self.sent.remove(session_id);
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
        self.update_size_drivers(cx);
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
        let view = cx.new(|cx| TerminalPane::new(tx, now, cx));
        let handle = view.read(cx).focus_handle();
        let id = pane_id.to_owned();
        let focus_in = cx.on_focus_in(&handle, window, move |this, _, cx| {
            this.pane_focused(&id, cx);
            cx.notify();
        });
        let events = cx.subscribe(&view, |this, _, event: &PaneEvent, _| {
            this.on_pane_event(event);
        });
        PaneSlot {
            view,
            tab_id: String::new(),
            session: None,
            attached: false,
            _focus_in: focus_in,
            _events: events,
        }
    }

    fn pane_focused(&mut self, pane_id: &str, cx: &mut Context<Self>) {
        if let Some(tab_id) = self.panes.get(pane_id).map(|slot| slot.tab_id.clone()) {
            self.tabs.set_focused(&tab_id, pane_id);
            self.update_size_drivers(cx);
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
        self.retries.reset(session_id);
        let mut first = None;
        for slot in self
            .panes
            .values_mut()
            .filter(|slot| slot.session.as_deref() == Some(session_id))
        {
            slot.view.update(cx, |pane, cx| pane.attach(&session, cx));
            slot.attached = true;
            first.get_or_insert_with(|| slot.view.clone());
        }
        if let Some(view) = first {
            view.read(cx).request_scrollback();
        }
    }

    /// Tells every pane whether it drives its session's PTY size.
    fn update_size_drivers(&self, cx: &mut Context<Self>) {
        let active = self.tabs.active_id();
        let focused = active.and_then(|tab_id| self.tabs.focused_pane(tab_id));
        let drivers = tabs::size_drivers(&self.tabs.bindings(), active, focused.as_deref());
        for (pane_id, slot) in &self.panes {
            let drives = slot
                .session
                .as_ref()
                .is_some_and(|session| drivers.get(session) == Some(pane_id));
            slot.view.update(cx, |pane, _| pane.set_drives_size(drives));
        }
    }

    /// A pane's scrollback retry: sent once per session and attempt, and the
    /// reply reaches every pane showing the session.
    fn on_pane_event(&mut self, event: &PaneEvent) {
        let PaneEvent::ScrollbackRetry {
            session_id,
            attempt,
        } = event;
        if self.retries.claim(session_id, *attempt) {
            tracing::warn!(
                "scrollback request for session {session_id} timed out; retry {attempt}"
            );
            self.send(ClientMessage::LoadScrollback {
                session_id: session_id.clone(),
            });
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

    /// Hands daemon output for `session_id` to every pane showing it.
    pub(crate) fn feed_panes(
        &mut self,
        session_id: &str,
        cx: &mut Context<Self>,
        mut feed: impl FnMut(&mut TerminalPane) -> Result<(), base64::DecodeError>,
    ) {
        for view in self.pane_views(Some(session_id)) {
            let fed = view.update(cx, |pane, cx| {
                cx.notify();
                feed(pane)
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

    pub(crate) fn focus_active_pane(&self, window: &mut Window, cx: &Context<Self>) {
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

    /// The active tab: its split tree, a notice for a diff tab, or a hint
    /// when there is no tab.
    pub(crate) fn grid_area(&self, cx: &mut Context<Self>) -> Div {
        let content = match self.tabs.active_tab() {
            None => muted_note("No tab open").into_any_element(),
            Some(tab) => match &tab.content {
                TabContent::Diff { .. } => {
                    muted_note("Diff tab (not yet supported)").into_any_element()
                }
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
                muted_note("Empty pane")
                    .track_focus(&handle)
                    .on_mouse_down(MouseButton::Left, move |_, window, _| {
                        handle.focus(window);
                    })
                    .into_any_element()
            }
            (None, None) => muted_note("Empty pane").into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .border_1()
            .border_color(gpui::rgb(if focused { FOCUS_BORDER } else { BORDER }))
            .child(self.pane_header(tab_id, pane_id, session_id, cx))
            .child(body)
            .into_any_element()
    }

    /// The session's name, split right and down (Shift: left and up), and
    /// close.
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
        div()
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
        assert!(gate.claim("s1", 2));
        assert!(!gate.claim("s1", 2), "a second pane of the same session");
        assert!(gate.claim("s2", 2));
        assert!(gate.claim("s1", 3));
        assert!(!gate.claim("s1", 2), "a late report of an earlier round");
        gate.reset("s1");
        assert!(gate.claim("s1", 2), "a fresh attach counts again");
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
