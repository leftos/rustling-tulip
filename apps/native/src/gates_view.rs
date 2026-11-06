//! The Gates panel: a header naming the daemon host's machine, then a block
//! per local pool (its header with the count stepper, its slot rows and the
//! gates waiting for it), the critical block, the remote and builder pools
//! and the queue; the busy row's menu and the log its click opens; the
//! footer strip. Every text comes from [`crate::gates`]; this module only
//! renders it, sends the stepper's writes and folds their answers.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, MouseButton, MouseDownEvent, Pixels, Point,
    SharedString, Stateful, StyledText, Task, Window, anchored, deferred, div, prelude::*, px,
};
use protocol::{
    ClientMessage, GateCommandSummary, GateHistoryRun, GateQueueTicket, GateSlotKind,
    GateSlotState, GateState, GateWaiting,
};

use crate::driver_id::DriverId as _;
use crate::ellipsis::{ellipsized, truncating};
use crate::fonts::{self, DEFAULT_FAMILY, ui_px, ui_px_at};
use crate::gates::{self, GateRow};
use crate::needs_you;
use crate::open::OpenJob;
use crate::open_view::Then;
use crate::session_menu::{disabled_row, menu_item, popover_frame};
use crate::sidebar::Activity;
use crate::sidebar_view::{ROW_HEIGHT, ROW_PADDING};
use crate::target::HostKey;
use crate::ui_theme::{UiTheme, theme};
use crate::{
    FOOTER_PILL_HEIGHT, FOOTER_PILL_TEXT_SIZE, RootView, UI_TEXT_SIZE, new_request_id, tagged_text,
    tooltip,
};

/// The header's title, before the machine's name.
const TITLE: &str = "GATES";
/// The header's height, as the Needs You panel's.
const HEADER_HEIGHT: f32 = 26.0;
/// The repaint interval while some hold or wait reads in seconds.
const FAST_REPAINT: Duration = Duration::from_secs(1);
/// The repaint interval once every time reads in minutes or more.
const SLOW_REPAINT: Duration = Duration::from_secs(30);
/// How often the panel asks for the gate's history while it shows.
const HISTORY_INTERVAL: Duration = Duration::from_secs(15);
/// How often the client asks for the gate's history while the panel is
/// hidden, for the footer's failure tail.
const HIDDEN_HISTORY_INTERVAL: Duration = Duration::from_secs(60);
/// The rows' and headers' small text, in px.
const SMALL_TEXT: f32 = 11.5;
/// A caps heading's text, in px.
const CAPS_TEXT: f32 = 10.5;
/// A slot name's fixed width (`heavy 0`), in px.
const NAME_WIDTH: f32 = 48.0;
/// A slot glyph's side, in px.
const GLYPH: f32 = 11.0;
/// The glyph's column and the gap after it, in px.
const GLYPH_COLUMN: f32 = 14.0;
const COLUMN_GAP: f32 = 7.0;
/// Line two's indent under line one's command, in px.
const LINE_TWO_INDENT: f32 = GLYPH_COLUMN + COLUMN_GAP + NAME_WIDTH + COLUMN_GAP;
/// The space between a pool header's heading, count and waiting text, in
/// px.
const HEADER_GAP: f32 = 7.0;
/// The row menu's width, in px.
const MENU_WIDTH: f32 = 200.0;
/// The tip of a menu row whose value the gate did not report.
const NOT_REPORTED: &str = "The gate did not report it";

/// The Gates panel's own state: the stepper writes waiting for an answer,
/// the last refusal per pool, the open row menu and the repaint timer.
#[derive(Default)]
pub(crate) struct GatesUi {
    /// The request id of each pool's pending `SetGateSlotCount`.
    pending: Vec<(GateSlotKind, String)>,
    /// The message of each pool's last refused write, until its next press.
    errors: Vec<(GateSlotKind, String)>,
    menu: Option<GateMenu>,
    /// Repaints the panel while it shows, so its times count up.
    timer: Option<Task<()>>,
    /// How many times that timer has repainted the panel.
    repaints: usize,
    /// The request id of the pending `GateHistory`.
    history_pending: Option<String>,
    /// The last `GateHistory` reply.
    history: Option<History>,
    /// Asks for the history again while the panel shows.
    history_timer: Option<Task<()>>,
    /// Asks for the history every minute while the panel is hidden.
    hidden_history_timer: Option<Task<()>>,
    /// Whether the Last 7 days block is folded; in memory only.
    week_folded: bool,
}

/// A `GateHistory` reply as the panel keeps it.
struct History {
    runs: Vec<GateHistoryRun>,
    open: Vec<GateHistoryRun>,
    summary: Vec<GateCommandSummary>,
}

/// The open row menu: where it opened and the values it copies.
struct GateMenu {
    at: Point<Pixels>,
    command: Option<String>,
    folder: Option<String>,
    log: Option<String>,
}

impl GatesUi {
    fn pending(&self, kind: GateSlotKind) -> bool {
        self.pending.iter().any(|(k, _)| *k == kind)
    }

    fn error(&self, kind: GateSlotKind) -> Option<&str> {
        self.errors
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, message)| message.as_str())
    }

    /// Forgets every pending write, refusal and the menu: the state they
    /// belong to is gone.
    pub(crate) fn clear(&mut self) {
        self.pending.clear();
        self.errors.clear();
        self.menu = None;
        self.history_pending = None;
        self.history = None;
        self.history_timer = None;
        self.hidden_history_timer = None;
    }

    /// Keeps the `GateHistory` reply to `request_id` when it answers the
    /// pending request; returns whether it did.
    pub(crate) fn history_answered(
        &mut self,
        request_id: &str,
        runs: &[GateHistoryRun],
        open: &[GateHistoryRun],
        summary: &[GateCommandSummary],
    ) -> bool {
        if self.history_pending.as_deref() != Some(request_id) {
            return false;
        }
        self.history_pending = None;
        self.history = Some(History {
            runs: runs.to_vec(),
            open: open.to_vec(),
            summary: summary.to_vec(),
        });
        true
    }

    /// Ends the pending `GateHistory` request a refusal echoing
    /// `request_id` answers, keeping any earlier history; returns whether
    /// it did.
    fn history_refused(&mut self, request_id: &str) -> bool {
        if self.history_pending.as_deref() != Some(request_id) {
            return false;
        }
        self.history_pending = None;
        true
    }

    /// Ends the pending write `request_id` answers; returns whether one was.
    pub(crate) fn answered(&mut self, request_id: &str) -> bool {
        let before = self.pending.len();
        self.pending.retain(|(_, id)| id != request_id);
        self.pending.len() != before
    }

    /// Ends the pending write `request_id` refuses and keeps `message` for
    /// its pool; returns whether one was.
    fn refused(&mut self, request_id: &str, message: &str) -> bool {
        let Some(at) = self.pending.iter().position(|(_, id)| id == request_id) else {
            return false;
        };
        let (kind, _) = self.pending.remove(at);
        self.errors.retain(|(k, _)| *k != kind);
        self.errors.push((kind, message.to_owned()));
        true
    }
}

/// A pool block's makings: its selector key, title, counts and rows.
struct Block<'a> {
    /// `heavy`, `light`, `remote`, `builder-<n>`.
    key: String,
    /// A slot's name before its index: `heavy`, `remote`, `builder`.
    name: &'static str,
    title: String,
    taken: u32,
    count: u32,
    problem: Option<&'a str>,
    /// The local pool's kind and default, which draw the stepper.
    stepper: Option<(GateSlotKind, u32)>,
    rows: Vec<GateRow>,
    waiting: Vec<&'a GateWaiting>,
    remote: bool,
}

impl RootView {
    /// The Gates panel's header text, `GATES · <machine>`, while This PC
    /// has a gate state.
    #[must_use]
    pub fn gates_header(&self) -> Option<String> {
        self.gate_state
            .as_ref()
            .map(|state| format!("{TITLE} · {}", state.machine))
    }

    /// The Gates panel's pool header lines, one per local pool in
    /// [`gates::rows`]'s order; empty with no gate state.
    #[must_use]
    pub fn gates_pool_lines(&self) -> Vec<String> {
        self.gate_state
            .as_ref()
            .map(gates::rows)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| match row {
                GateRow::PoolHeader { text, .. } => Some(text),
                GateRow::Slot { .. } => None,
            })
            .collect()
    }

    /// Drops This PC's gate state and everything the panel kept for it:
    /// the pending stepper writes, the refusals and the row menu, which
    /// no later answer can end once the state is gone.
    pub(crate) fn forget_gate_state(&mut self) {
        self.gate_state = None;
        self.gates_ui.clear();
    }

    /// Whether `kind`'s stepper waits for the daemon's answer.
    #[must_use]
    pub fn gates_step_pending(&self, kind: GateSlotKind) -> bool {
        self.gates_ui.pending(kind)
    }

    /// Whether a Gates row's menu is open.
    #[must_use]
    pub fn gates_menu_open(&self) -> bool {
        self.gates_ui.menu.is_some()
    }

    /// How many times the panel's timer has repainted it.
    #[must_use]
    pub fn gates_repaints(&self) -> usize {
        self.gates_ui.repaints
    }

    /// Shows the Gates panel, unfolding a folded one.
    pub fn show_gates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.change_activity(|sidebar| sidebar.show_activity(Activity::Gates), window, cx);
    }

    /// A daemon refusal from `host` carrying `request_id`: when it answers a
    /// pending stepper write, its pool shows `message`. Returns whether it
    /// did.
    pub(crate) fn on_gates_refused(
        &mut self,
        host: &HostKey,
        request_id: &str,
        message: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if *host == HostKey::Local && self.gates_ui.history_refused(request_id) {
            tracing::info!("gates: the daemon refused a history request: {message}");
            return true;
        }
        let answered = *host == HostKey::Local && self.gates_ui.refused(request_id, message);
        if answered {
            tracing::info!("gates: the daemon refused a slot count");
            cx.notify();
        }
        answered
    }

    /// Whether the Gates panel is the one showing.
    fn gates_shown(&self) -> bool {
        self.shown_activity() == Activity::Gates && !self.sidebar.is_collapsed()
    }

    /// Starts the panel's repaint timer while the panel shows, and drops it
    /// when the panel is hidden.
    pub(crate) fn arm_gates_repaint(&mut self, cx: &mut Context<Self>) {
        let delay = match self.gate_state.as_ref() {
            Some(state) if self.gates_shown() => {
                let now = Utc::now();
                let history_fresh =
                    self.gates_ui.history.as_ref().is_some_and(|history| {
                        gates::history_fresh(&history.runs, &history.open, now)
                    });
                if gates::fresh(state, now) || history_fresh {
                    FAST_REPAINT
                } else {
                    SLOW_REPAINT
                }
            }
            _ => {
                self.gates_ui.timer = None;
                return;
            }
        };
        self.gates_ui.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            // Fails only when the view is gone, and the panel with it.
            this.update(cx, Self::repaint_gates).ok();
        }));
    }

    /// The timer fired: repaint the times and go again while the panel
    /// still shows.
    fn repaint_gates(&mut self, cx: &mut Context<Self>) {
        if !self.gates_shown() {
            self.gates_ui.timer = None;
            return;
        }
        self.gates_ui.repaints += 1;
        cx.notify();
        self.arm_gates_repaint(cx);
    }

    /// Whether a `GateHistory` request waits for its answer.
    #[must_use]
    pub fn gates_history_pending(&self) -> bool {
        self.gates_ui.history_pending.is_some()
    }

    /// How many rows the Recent runs block shows; `None` before the first
    /// answer.
    #[must_use]
    pub fn gates_recent_count(&self) -> Option<usize> {
        self.gates_ui
            .history
            .as_ref()
            .map(|history| gates::recent_runs(&history.runs, &history.open, Utc::now()).len())
    }

    /// How many rows the Last 7 days block holds, folded or not; `None`
    /// before the first answer.
    #[must_use]
    pub fn gates_week_count(&self) -> Option<usize> {
        self.gates_ui
            .history
            .as_ref()
            .map(|history| gates::week_rows(&history.summary).len())
    }

    /// Whether the Last 7 days block is unfolded.
    #[must_use]
    pub fn gates_week_open(&self) -> bool {
        !self.gates_ui.week_folded
    }

    /// Asks This PC for the gate's history when the panel opens and starts
    /// asking every 15 s; while the panel is hidden and a gate state is
    /// known, asks every 60 s instead, for the footer's failure tail. A
    /// running timer of the right cadence is left alone; with no gate state
    /// both stop.
    pub(crate) fn arm_gates_history(&mut self, cx: &mut Context<Self>) {
        if self.gate_state.is_none() {
            self.gates_ui.history_timer = None;
            self.gates_ui.hidden_history_timer = None;
            return;
        }
        if !self.gates_shown() {
            self.gates_ui.history_timer = None;
            if self.gates_ui.hidden_history_timer.is_none() {
                self.schedule_hidden_gate_history(cx);
            }
            return;
        }
        self.gates_ui.hidden_history_timer = None;
        if self.gates_ui.history_timer.is_some() {
            return;
        }
        self.request_gate_history();
        self.schedule_gate_history(cx);
    }

    fn schedule_hidden_gate_history(&mut self, cx: &mut Context<Self>) {
        self.gates_ui.hidden_history_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(HIDDEN_HISTORY_INTERVAL)
                .await;
            // Fails only when the view is gone, and the footer with it.
            this.update(cx, Self::tick_hidden_gate_history).ok();
        }));
    }

    /// The hidden-panel history timer fired: ask again and go on while the
    /// panel is still hidden and the gate state still known.
    fn tick_hidden_gate_history(&mut self, cx: &mut Context<Self>) {
        self.gates_ui.hidden_history_timer = None;
        if self.gate_state.is_none() || self.gates_shown() {
            return;
        }
        self.request_gate_history();
        self.schedule_hidden_gate_history(cx);
    }

    /// The footer pill's failure tail, ` · ✕ failed · {repo} · {ago} ago`,
    /// while This PC has a gate state and its history holds a run that
    /// failed in the last 10 minutes.
    #[must_use]
    pub fn gates_failure_tail(&self) -> Option<String> {
        self.gate_state.as_ref()?;
        let history = self.gates_ui.history.as_ref()?;
        let now = self.wall_now();
        gates::failure_tail(&history.runs, now).map(|tail| tail.text(now))
    }

    /// The summary rows of the last history reply; none before one.
    fn gates_summary(&self) -> &[GateCommandSummary] {
        self.gates_ui
            .history
            .as_ref()
            .map_or(&[], |history| history.summary.as_slice())
    }

    fn schedule_gate_history(&mut self, cx: &mut Context<Self>) {
        self.gates_ui.history_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HISTORY_INTERVAL).await;
            // Fails only when the view is gone, and the panel with it.
            this.update(cx, Self::tick_gate_history).ok();
        }));
    }

    /// The history timer fired: ask again and go on while the panel still
    /// shows.
    fn tick_gate_history(&mut self, cx: &mut Context<Self>) {
        if !self.gates_shown() {
            self.gates_ui.history_timer = None;
            return;
        }
        self.request_gate_history();
        self.schedule_gate_history(cx);
    }

    /// Sends a `GateHistory` to This PC, unless one is unanswered.
    fn request_gate_history(&mut self) {
        if self.gates_ui.history_pending.is_some() {
            return;
        }
        let request_id = new_request_id();
        self.gates_ui.history_pending = Some(request_id.clone());
        self.send_to(
            &HostKey::Local,
            ClientMessage::GateHistory {
                request_id: Some(request_id),
            },
        );
    }

    /// A click on the Last 7 days header: fold or unfold its rows.
    fn toggle_gates_week(&mut self, cx: &mut Context<Self>) {
        self.gates_ui.week_folded = !self.gates_ui.week_folded;
        cx.notify();
    }

    /// The Last 7 days block: its header, which folds it, then the first
    /// four summary rows or a line saying there are none.
    fn week_block(
        &self,
        history: &History,
        offset: i8,
        shift: i8,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let open = !self.gates_ui.week_folded;
        let rows = gates::week_rows(&history.summary);
        let header = section_header(
            "Last 7 days",
            if open { "passed runs" } else { "▸ folded" },
            offset,
            shift,
            &t,
        )
        .id("gates-week-toggle")
        .driver_id(|| "gates-week-toggle".to_owned())
        .cursor_pointer()
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.toggle_gates_week(cx);
        }));
        let body: Vec<AnyElement> = if !open {
            Vec::new()
        } else if rows.is_empty() {
            vec![
                note_line(offset, t.faint)
                    .child(tagged_text("gates-week-empty", "No passed runs in 7 days"))
                    .into_any_element(),
            ]
        } else {
            rows.iter()
                .enumerate()
                .map(|(n, row)| week_row(n, row, offset, &t))
                .collect()
        };
        div()
            .driver_id(|| "gates-week".to_owned())
            .flex()
            .flex_col()
            .pb(px(6.0))
            .child(header)
            .children(body)
            .into_any_element()
    }

    /// A stepper press: sends `kind`'s count one up or down, unless a write
    /// for it is pending or the count is at its bound.
    fn step_gate_count(&mut self, kind: GateSlotKind, up: bool, cx: &mut Context<Self>) {
        if self.gates_ui.pending(kind) {
            return;
        }
        let Some(count) = self
            .gate_state
            .as_ref()
            .and_then(|state| state.pools.iter().find(|pool| pool.kind == kind))
            .map(|pool| pool.count)
        else {
            return;
        };
        let Some(next) = gates::stepped(count, up) else {
            return;
        };
        let request_id = new_request_id();
        self.gates_ui.errors.retain(|(k, _)| *k != kind);
        self.gates_ui.pending.push((kind, request_id.clone()));
        self.send_to(
            &HostKey::Local,
            ClientMessage::SetGateSlotCount {
                kind,
                count: Some(next),
                request_id: Some(request_id),
            },
        );
        cx.notify();
    }

    /// Opens `log` in the OS default app; nothing when the gate reported no
    /// log.
    fn open_gate_log(&mut self, log: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(log) = log else {
            return;
        };
        let job = OpenJob::DefaultApp {
            path: PathBuf::from(log),
            confirmed: false,
        };
        self.dispatch_open(&HostKey::Local, job, Then::Nothing, window, cx);
    }

    fn open_gate_menu(&mut self, menu: GateMenu, cx: &mut Context<Self>) {
        self.gates_ui.menu = Some(menu);
        cx.notify();
    }

    fn close_gate_menu(&mut self, cx: &mut Context<Self>) {
        if self.gates_ui.menu.take().is_some() {
            cx.notify();
        }
    }

    /// The open row menu over a layer that closes it on a press outside.
    pub(crate) fn gates_menu_layer(&self, cx: &mut Context<Self>) -> Option<[AnyElement; 2]> {
        let menu = self.gates_ui.menu.as_ref()?;
        let t = theme(cx);
        let backdrop = div()
            .id("gates-menu-backdrop")
            .driver_id(|| "gates-menu-backdrop".to_owned())
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.close_gate_menu(cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.close_gate_menu(cx);
                    cx.stop_propagation();
                }),
            );
        let rows = [
            ("gates-menu-copy-command", "Copy command", &menu.command),
            ("gates-menu-copy-folder", "Copy folder", &menu.folder),
            ("gates-menu-copy-log", "Copy log path", &menu.log),
        ]
        .into_iter()
        .map(|(selector, label, value)| match value.clone() {
            Some(text) => menu_item(selector, label, false, &t)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.copy_to_clipboard(&text, cx);
                        this.close_gate_menu(cx);
                    }),
                )
                .into_any_element(),
            None => disabled_row(selector, label, NOT_REPORTED, &t).into_any_element(),
        })
        .collect::<Vec<_>>();
        let panel = popover_frame(fonts::ui_text_size(cx), "gates-menu", &t)
            .flex()
            .flex_col()
            .w(px(MENU_WIDTH))
            .p(px(4.0))
            .children(rows);
        let anchored = anchored().position(menu.at).snap_to_window().child(panel);
        Some([
            backdrop.into_any_element(),
            deferred(anchored).with_priority(1).into_any_element(),
        ])
    }

    /// The footer's Gates pill, `Gates <taken>/<total>`, while This PC has
    /// a gate state; a click shows the panel.
    pub(crate) fn gates_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.gate_state.as_ref()?;
        let t = theme(cx);
        let on = self.gates_shown();
        let tail = self.gates_failure_tail().map(|tail| {
            div()
                .driver_id(|| "footer-gates-tail".to_owned())
                .flex_none()
                .text_color(gpui::rgb(t.danger))
                .child(tagged_text("footer-gates-tail", tail))
        });
        Some(
            div()
                .id("footer-gates")
                .driver_id(|| "footer-gates".to_owned())
                .flex()
                .flex_none()
                .items_center()
                .h(ui_px(cx, FOOTER_PILL_HEIGHT))
                .px(px(9.0))
                .rounded(ui_px(cx, FOOTER_PILL_HEIGHT) / 2.0)
                .border_1()
                .border_color(gpui::rgb(if on { t.subtle } else { t.line_strong }))
                .when(on, move |pill| pill.bg(gpui::rgb(t.chip)))
                .text_size(ui_px(cx, FOOTER_PILL_TEXT_SIZE))
                .text_color(gpui::rgb(t.text_2))
                .whitespace_nowrap()
                .cursor_pointer()
                .hover(move |style| style.bg(gpui::rgb(t.hover_bg)))
                .child(tagged_text("footer-gates-text", gates::strip_text(state)))
                .children(tail)
                .tooltip(tooltip(format!("Gates on {}", state.machine)))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.show_gates(window, cx);
                }))
                .into_any_element(),
        )
    }

    /// The panel, `width` wide.
    pub(crate) fn gates_view(&self, width: f32, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        let offset = fonts::ui_text_size(cx);
        let shift = fonts::ui_weight_shift(cx);
        let now = Utc::now();
        let mut blocks: Vec<AnyElement> = Vec::new();
        if let Some(state) = self.gate_state.as_ref() {
            for pool in gates::local_pools(state) {
                let block = Block {
                    key: gates::kind_name(pool.kind).to_owned(),
                    name: gates::kind_name(pool.kind),
                    title: gates::kind_title(pool.kind).to_owned(),
                    taken: gates::taken_count(&pool.slots),
                    count: pool.count,
                    problem: pool.problem.as_deref(),
                    stepper: (pool.kind != GateSlotKind::Unknown)
                        .then_some((pool.kind, pool.default)),
                    rows: gates::slot_rows(pool.kind, &pool.slots),
                    waiting: gates::pool_waiting(pool),
                    remote: false,
                };
                blocks.push(self.pool_block(&block, now, cx));
            }
            blocks.extend(critical_block(state, offset, shift, &t));
            for block in remote_blocks(state) {
                blocks.push(self.pool_block(&block, now, cx));
            }
            blocks.extend(queue_block(
                state,
                self.gates_summary(),
                now,
                offset,
                shift,
                &t,
            ));
            if let Some(history) = self.gates_ui.history.as_ref() {
                blocks.push(recent_block(history, now, offset, shift, &t));
                blocks.push(self.week_block(history, offset, shift, cx));
            }
        }
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.0))
            .h(ui_px_at(offset, HEADER_HEIGHT))
            .px(px(ROW_PADDING))
            .border_b_1()
            .border_color(gpui::rgb(t.border))
            .text_color(gpui::rgb(t.muted))
            .driver_id(|| "gates-header".to_owned())
            .child(
                div()
                    .font_weight(fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
                    .child(self.gates_header().unwrap_or_default()),
            )
            .children(self.gate_state.as_ref().map(|state| {
                div()
                    .ml_auto()
                    .text_size(ui_px_at(offset, 11.0))
                    .text_color(gpui::rgb(t.faint))
                    .child(tagged_text(
                        "gates-cores",
                        format!("{} cores", state.processors),
                    ))
            }));
        let body = div()
            .id("gates-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .pb(px(14.0))
            .children(blocks);
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(width))
            .h_full()
            .driver_id(|| "gates-panel".to_owned())
            .track_focus(&self.sidebar_focus)
            .bg(gpui::rgb(t.panel_bg))
            .text_size(ui_px(cx, UI_TEXT_SIZE))
            .text_color(gpui::rgb(t.text))
            .child(header)
            .child(body)
    }

    /// One pool's block: its header, its problem or refusal line, its slot
    /// rows and the gates waiting for it.
    #[expect(
        clippy::too_many_lines,
        reason = "the header's parts, the note lines and the rows build one block in order"
    )]
    fn pool_block(
        &self,
        block: &Block<'_>,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let offset = fonts::ui_text_size(cx);
        let shift = fonts::ui_weight_shift(cx);
        let key = block.key.clone();
        let waiting = block.waiting.len();
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .px(px(12.0))
            .pt(px(4.0))
            .pb(px(5.0))
            .child(caps(&block.title, offset, shift, &t).driver_id({
                let selector = format!("gates-pool-{key}-title");
                move || selector
            }))
            .child(
                div()
                    .driver_id({
                        let tag = format!(
                            "gates-pool-{key}-count={} of {} busy",
                            block.taken, block.count
                        );
                        move || tag
                    })
                    .flex()
                    .flex_none()
                    .ml(px(HEADER_GAP))
                    .gap(px(3.0))
                    .whitespace_nowrap()
                    .text_size(ui_px_at(offset, SMALL_TEXT))
                    .text_color(gpui::rgb(t.subtle))
                    .child(
                        div()
                            .text_color(gpui::rgb(t.text))
                            .font_weight(fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
                            .child(block.taken.to_string()),
                    )
                    .child(format!("of {} busy", block.count)),
            )
            .when(waiting > 0, |header| {
                let selector = format!("gates-pool-{key}-waiting");
                header
                    .child(
                        div()
                            .driver_id({
                                let selector = format!("gates-pool-{key}-dot");
                                move || selector
                            })
                            .flex_none()
                            .ml(px(HEADER_GAP))
                            .text_size(ui_px_at(offset, SMALL_TEXT))
                            .text_color(gpui::rgb(t.subtle))
                            .child("·"),
                    )
                    .child(
                        div()
                            .driver_id({
                                let selector = selector.clone();
                                move || selector
                            })
                            .flex_none()
                            .ml(px(HEADER_GAP))
                            .whitespace_nowrap()
                            .text_size(ui_px_at(offset, SMALL_TEXT))
                            .text_color(gpui::rgb(t.ochre))
                            .child(tagged_text(selector, format!("{waiting} waiting"))),
                    )
            })
            .children(
                block
                    .stepper
                    .map(|(kind, default)| self.stepper(kind, block.count, default, cx)),
            );
        let error = block
            .stepper
            .and_then(|(kind, _)| self.gates_ui.error(kind).map(|message| (kind, message)))
            .map(|(kind, message)| {
                note_line(offset, t.danger_text).child(tagged_text(
                    format!("gates-error-{}", gates::kind_name(kind)),
                    message.to_owned(),
                ))
            });
        let problem = block.problem.map(|problem| {
            note_line(offset, t.faint).child(tagged_text(
                format!("gates-pool-{key}-problem"),
                problem.to_owned(),
            ))
        });
        let summary = self.gates_summary();
        let rows: Vec<AnyElement> = block
            .rows
            .iter()
            .filter_map(|row| {
                let usual = match row {
                    GateRow::Slot {
                        command_key, repo, ..
                    } => gates::usual(
                        summary,
                        command_key.as_deref(),
                        repo.as_deref(),
                        block.remote,
                    ),
                    GateRow::PoolHeader { .. } => None,
                };
                Self::slot_row(&key, block.name, row, block.remote, usual, now, cx)
            })
            .collect();
        let waits: Vec<AnyElement> = block
            .waiting
            .iter()
            .enumerate()
            .map(|(n, gate)| {
                let usual = gates::usual(
                    summary,
                    gate.command_key.as_deref(),
                    gate.repo.as_deref(),
                    false,
                );
                wait_row(&key, n, gate, usual, now, offset, shift, &t)
            })
            .collect();
        div()
            .driver_id(move || format!("gates-pool-{key}"))
            .flex()
            .flex_col()
            .py(px(6.0))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .child(header)
            .children(error)
            .children(problem)
            .children(rows)
            .children(waits)
            .into_any_element()
    }

    /// A local pool's count stepper: `−`, the count, `+`; dimmed and inert
    /// while its write is pending.
    fn stepper(
        &self,
        kind: GateSlotKind,
        count: u32,
        default: u32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let offset = fonts::ui_text_size(cx);
        let shift = fonts::ui_weight_shift(cx);
        let pending = self.gates_ui.pending(kind);
        let name = gates::kind_name(kind);
        let selector = format!("gates-stepper-{name}");
        div()
            .id(SharedString::from(selector.clone()))
            .driver_id(move || selector)
            .ml_auto()
            .flex()
            .flex_none()
            .items_center()
            .border_1()
            .border_color(gpui::rgb(t.line_strong))
            .rounded(px(4.0))
            .overflow_hidden()
            .text_size(ui_px_at(offset, SMALL_TEXT))
            .when(pending, |stepper| stepper.opacity(0.6))
            .child(Self::step_cell(kind, false, pending, cx))
            .child(
                div()
                    .min_w(px(20.0))
                    .flex()
                    .justify_center()
                    .border_l_1()
                    .border_r_1()
                    .border_color(gpui::rgb(t.line_strong))
                    .text_color(gpui::rgb(t.text))
                    .font_weight(fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
                    .child(tagged_text(
                        format!("gates-count-{name}"),
                        count.to_string(),
                    )),
            )
            .child(Self::step_cell(kind, true, pending, cx))
            .tooltip(tooltip(format!(
                "Slot count, written to slot-counts.json; default for this machine: {default}"
            )))
            .into_any_element()
    }

    /// One of the stepper's two buttons.
    fn step_cell(
        kind: GateSlotKind,
        up: bool,
        pending: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let t = theme(cx);
        let selector = format!(
            "gates-step-{}-{}",
            if up { "up" } else { "down" },
            gates::kind_name(kind)
        );
        let cell = div()
            .id(SharedString::from(selector.clone()))
            .driver_id(move || selector)
            .px(px(6.0))
            .text_color(gpui::rgb(t.text_2))
            .child(if up { "+" } else { "−" });
        if pending {
            cell.cursor_default()
        } else {
            cell.cursor_pointer()
                .hover(move |style| style.bg(gpui::rgb(t.hover_bg)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.step_gate_count(kind, up, cx);
                }))
        }
    }

    /// A slot's row: two lines for a busy slot, one for a held or free one.
    #[expect(
        clippy::too_many_lines,
        reason = "the busy, held and free rows share line one and differ only in the match"
    )]
    fn slot_row(
        key: &str,
        name: &str,
        row: &GateRow,
        remote: bool,
        usual: Option<f64>,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let GateRow::Slot {
            index,
            state,
            command,
            cwd,
            started,
            last_line,
            stale_pid,
            log,
            timeout_seconds,
            processes,
            ..
        } = row
        else {
            return None;
        };
        let t = theme(cx);
        let offset = fonts::ui_text_size(cx);
        let shift = fonts::ui_weight_shift(cx);
        let selector = format!("gates-slot-{key}-{index}");
        let label = format!("{name} {index}");
        let line_one = div()
            .flex()
            .items_center()
            .gap(px(COLUMN_GAP))
            .h(ui_px(cx, ROW_HEIGHT))
            .min_w(px(0.0))
            .child(slot_glyph(*state, remote, &t))
            .child(
                div()
                    .flex_none()
                    .w(px(NAME_WIDTH))
                    .whitespace_nowrap()
                    .text_size(ui_px_at(offset, 11.0))
                    .text_color(gpui::rgb(t.faint))
                    .child(tagged_text(format!("{selector}-name"), label)),
            );
        let frame = div()
            .id(SharedString::from(selector.clone()))
            .driver_id({
                let selector = selector.clone();
                move || selector
            })
            .flex()
            .flex_col()
            .w_full()
            .px(px(12.0))
            .hover(move |style| style.bg(gpui::rgb(t.hover_bg)));
        let row = match state {
            GateSlotState::Busy => {
                let short = command
                    .as_deref()
                    .map(gates::short_command)
                    .unwrap_or_default();
                let held = started
                    .map(|since| needs_you::waited(now, since))
                    .unwrap_or_default();
                let over = started
                    .is_some_and(|since| gates::over_usual((now - since).num_seconds(), usual));
                let detail = [cwd.as_deref().map(gates::folder_label), last_line.clone()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · ");
                let tip = gates::slot_tooltip(
                    command.as_deref(),
                    cwd.as_deref(),
                    log.as_deref(),
                    *timeout_seconds,
                    *processes,
                );
                let line_one = line_one
                    .child(mono(
                        tagged_box(&format!("{selector}-command"), &short)
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .child(truncating(
                                &format!("{selector}-command"),
                                StyledText::new(short.clone()),
                            )),
                        offset,
                    ))
                    .child(timed(
                        &selector,
                        held,
                        if over { t.danger } else { t.text_2 },
                        usual,
                        offset,
                        &t,
                    ));
                // The ellipsized copy is drawn absolutely over its box, which
                // ignores the box's padding, so the indent sits on a wrapper.
                let line_two = div()
                    .flex()
                    .min_w(px(0.0))
                    .pl(px(LINE_TWO_INDENT))
                    .pb(px(4.0))
                    .text_size(ui_px_at(offset, SMALL_TEXT))
                    .text_color(gpui::rgb(t.subtle))
                    .child(
                        tagged_box(&format!("{selector}-detail"), &detail)
                            .relative()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .children(ellipsized(
                                &format!("{selector}-detail"),
                                SharedString::from(detail.clone()),
                                StyledText::new(detail),
                            )),
                    );
                let open = log.clone();
                frame
                    .cursor_pointer()
                    .tooltip(tooltip(tip))
                    .child(line_one)
                    .child(line_two)
                    .on_mouse_down(
                        MouseButton::Right,
                        Self::menu_opener(command.as_deref(), cwd.as_deref(), log.as_deref(), cx),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.open_gate_log(open.clone(), window, cx);
                    }))
            }
            GateSlotState::Held => frame
                .child(
                    line_one
                        .child(
                            div()
                                .flex_none()
                                .text_size(ui_px_at(offset, SMALL_TEXT))
                                .text_color(gpui::rgb(t.ochre))
                                .font_weight(fonts::ui_weight_at(shift, FontWeight::MEDIUM))
                                .child(tagged_text(format!("{selector}-state"), "held")),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_size(ui_px_at(offset, SMALL_TEXT))
                                .text_color(gpui::rgb(t.faint))
                                .child("· by a gate with no claim"),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(ui_px_at(offset, SMALL_TEXT))
                                .text_color(gpui::rgb(t.faint))
                                .child("—"),
                        ),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    Self::menu_opener(command.as_deref(), cwd.as_deref(), log.as_deref(), cx),
                ),
            GateSlotState::Free | GateSlotState::Unknown => {
                let text = stale_pid.map_or_else(
                    || "free".to_owned(),
                    |pid| format!("free · stale pid {pid}"),
                );
                frame.child(
                    line_one.child(
                        div()
                            .flex_none()
                            .text_size(ui_px_at(offset, SMALL_TEXT))
                            .text_color(gpui::rgb(t.faint))
                            .child(tagged_text(format!("{selector}-state"), text)),
                    ),
                )
            }
        };
        Some(row.into_any_element())
    }

    /// The right-press handler that opens a busy or held row's menu.
    fn menu_opener(
        command: Option<&str>,
        cwd: Option<&str>,
        log: Option<&str>,
        cx: &mut Context<Self>,
    ) -> impl Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static {
        let (command, folder, log) = (
            command.map(str::to_owned),
            cwd.map(str::to_owned),
            log.map(str::to_owned),
        );
        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
            this.open_gate_menu(
                GateMenu {
                    at: event.position,
                    command: command.clone(),
                    folder: folder.clone(),
                    log: log.clone(),
                },
                cx,
            );
            cx.stop_propagation();
        })
    }
}

/// The remote pool and the builder pools as blocks, with no stepper.
fn remote_blocks(state: &GateState) -> Vec<Block<'_>> {
    let remote = state.remote.iter().map(|pool| Block {
        key: "remote".to_owned(),
        name: "remote",
        title: "REMOTE".to_owned(),
        taken: gates::taken_count(&pool.slots),
        count: pool.count,
        problem: pool.problem.as_deref(),
        stepper: None,
        rows: gates::slot_rows(GateSlotKind::Unknown, &pool.slots),
        waiting: Vec::new(),
        remote: true,
    });
    let builders = state.builders.iter().enumerate().map(|(n, pool)| {
        let label = pool
            .name
            .as_deref()
            .or(pool.host.as_deref())
            .unwrap_or(&pool.id);
        Block {
            key: format!("builder-{n}"),
            name: "builder",
            title: format!("BUILDER · {}", label.to_uppercase()),
            taken: gates::taken_count(&pool.slots),
            count: pool.count,
            problem: pool.problem.as_deref(),
            stepper: None,
            rows: gates::slot_rows(GateSlotKind::Unknown, &pool.slots),
            waiting: Vec::new(),
            remote: true,
        }
    });
    remote.chain(builders).collect()
}

/// The critical block: its heading and a note naming the holder and the
/// waiting count, only while one holds or waits.
fn critical_block(state: &GateState, offset: i8, shift: i8, t: &UiTheme) -> Option<AnyElement> {
    let note = gates::critical_note(state)?;
    Some(
        div()
            .driver_id(|| "gates-critical".to_owned())
            .flex()
            .flex_col()
            .py(px(6.0))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .child(
                div()
                    .flex()
                    .items_center()
                    .px(px(12.0))
                    .pt(px(4.0))
                    .pb(px(5.0))
                    .child(caps("CRITICAL", offset, shift, t)),
            )
            .child(note_line(offset, t.faint).child(tagged_text("gates-critical-note", note)))
            .into_any_element(),
    )
}

/// The queue block, oldest ticket first, one line each, each with its
/// usual time from `summary`; only with tickets.
fn queue_block(
    state: &GateState,
    summary: &[GateCommandSummary],
    now: DateTime<Utc>,
    offset: i8,
    shift: i8,
    t: &UiTheme,
) -> Option<AnyElement> {
    let tickets = gates::queue(state);
    if tickets.is_empty() {
        return None;
    }
    let rows = tickets.into_iter().enumerate().map(|(n, ticket)| {
        let usual = gates::usual(
            summary,
            ticket.command_key.as_deref(),
            ticket.repo.as_deref(),
            ticket.remote || ticket.pool.is_some(),
        );
        queue_row(n, ticket, usual, now, offset, t)
    });
    Some(
        div()
            .driver_id(|| "gates-queue".to_owned())
            .flex()
            .flex_col()
            .py(px(6.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .px(px(12.0))
                    .pt(px(4.0))
                    .pb(px(5.0))
                    .child(caps("QUEUE", offset, shift, t)),
            )
            .children(rows)
            .into_any_element(),
    )
}

/// A history block's header: `title` on the left, `note` faint on the
/// right.
fn section_header(title: &str, note: &str, offset: i8, shift: i8, t: &UiTheme) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(HEADER_GAP))
        .px(px(12.0))
        .pt(px(8.0))
        .pb(px(4.0))
        .child(caps(title, offset, shift, t))
        .child(
            div()
                .ml_auto()
                .flex_none()
                .whitespace_nowrap()
                .text_size(ui_px_at(offset, 11.0))
                .text_color(gpui::rgb(t.faint))
                .child(SharedString::from(note.to_owned())),
        )
}

/// The Recent runs block: its header, then a row per recent run or a line
/// saying there are none.
fn recent_block(
    history: &History,
    now: DateTime<Utc>,
    offset: i8,
    shift: i8,
    t: &UiTheme,
) -> AnyElement {
    let runs = gates::recent_runs(&history.runs, &history.open, now);
    let body: Vec<AnyElement> = if runs.is_empty() {
        vec![
            note_line(offset, t.faint)
                .child(tagged_text("gates-recent-empty", "No runs in the last 2 h"))
                .into_any_element(),
        ]
    } else {
        runs.into_iter()
            .enumerate()
            .map(|(n, run)| recent_row(n, run, now, offset, t))
            .collect()
    };
    div()
        .driver_id(|| "gates-recent".to_owned())
        .flex()
        .flex_col()
        .pb(px(6.0))
        .border_b_1()
        .border_color(gpui::rgb(t.line))
        .child(section_header("Recent runs", "last 2 h", offset, shift, t))
        .children(body)
        .into_any_element()
}

/// One recent run: its result glyph, short command and held time, then
/// its repo, slot, failure, fallback tag and age; a bad run is washed and
/// barred in the danger colour.
#[expect(
    clippy::too_many_lines,
    reason = "the row's two lines and its danger bar build one element in order"
)]
fn recent_row(
    n: usize,
    run: &GateHistoryRun,
    now: DateTime<Utc>,
    offset: i8,
    t: &UiTheme,
) -> AnyElement {
    let selector = format!("gates-recent-{n}");
    let result = gates::run_result(run);
    let glyph_color = match result {
        gates::RunResult::Ok => t.waiting,
        gates::RunResult::Running => t.working,
        gates::RunResult::Failed | gates::RunResult::TimedOut | gates::RunResult::Abandoned => {
            t.danger
        }
        gates::RunResult::Other => t.subtle,
    };
    let short = gates::short_command(&run.command);
    let held = gates::held_text(run.held_seconds);
    let ago = gates::run_end(run)
        .map(|end| format!("{} ago", needs_you::waited(now, end)))
        .unwrap_or_default();
    let line_one = div()
        .flex()
        .items_center()
        .gap(px(COLUMN_GAP))
        .min_w(px(0.0))
        .child(
            div()
                .flex()
                .flex_none()
                .justify_center()
                .w(px(GLYPH_COLUMN))
                .font_weight(FontWeight::BOLD)
                .text_color(gpui::rgb(glyph_color))
                .child(tagged_text(format!("{selector}-result"), result.glyph())),
        )
        .child(mono(
            tagged_box(&format!("{selector}-command"), &short)
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .text_color(gpui::rgb(t.text))
                .child(truncating(
                    &format!("{selector}-command"),
                    StyledText::new(short.clone()),
                )),
            offset,
        ))
        .child(
            div()
                .flex_none()
                .text_size(ui_px_at(offset, SMALL_TEXT))
                .text_color(gpui::rgb(t.subtle))
                .child(tagged_text(format!("{selector}-held"), held)),
        );
    let fallback = run.fallback.as_ref().map(|_| {
        div()
            .flex_none()
            .px(px(5.0))
            .rounded(px(3.0))
            .border_1()
            .border_color(gpui::rgb(t.ochre))
            .text_color(gpui::rgb(t.ochre))
            .child(tagged_text(format!("{selector}-fallback"), "fell back"))
    });
    let line_two = div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .min_w(px(0.0))
        .pl(px(GLYPH_COLUMN + COLUMN_GAP))
        .whitespace_nowrap()
        .text_size(ui_px_at(offset, SMALL_TEXT))
        .text_color(gpui::rgb(t.subtle))
        .child(div().min_w(px(0.0)).overflow_hidden().child(tagged_text(
            format!("{selector}-detail"),
            gates::run_detail(run),
        )))
        .children(fallback)
        .child(
            div()
                .ml_auto()
                .flex_none()
                .text_color(gpui::rgb(t.faint))
                .child(tagged_text(format!("{selector}-ago"), ago)),
        );
    let bad = result.bad();
    div()
        .driver_id({
            let selector = selector.clone();
            move || selector
        })
        .relative()
        .flex()
        .flex_col()
        .px(px(12.0))
        .py(px(4.0))
        .when(bad, |row| {
            row.bg(gpui::rgba(t.danger_wash)).child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(2.0))
                    .bg(gpui::rgb(t.danger)),
            )
        })
        .child(line_one)
        .child(line_two)
        .into_any_element()
}

/// One Last 7 days row: its short command, run count and p90 held time.
fn week_row(n: usize, row: &GateCommandSummary, offset: i8, t: &UiTheme) -> AnyElement {
    let selector = format!("gates-week-{n}");
    let short = gates::short_command(&row.command);
    div()
        .driver_id({
            let selector = selector.clone();
            move || selector
        })
        .flex()
        .items_center()
        .gap(px(10.0))
        .px(px(12.0))
        .py(px(2.0))
        .text_size(ui_px_at(offset, SMALL_TEXT))
        .text_color(gpui::rgb(t.text_2))
        .child(mono(
            tagged_box(&format!("{selector}-command"), &short)
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .child(truncating(
                    &format!("{selector}-command"),
                    StyledText::new(short.clone()),
                )),
            offset,
        ))
        .child(div().flex_none().child(tagged_text(
            format!("{selector}-runs"),
            gates::runs_text(row.count),
        )))
        .child(div().flex_none().child(tagged_text(
            format!("{selector}-p90"),
            format!("p90 {}", gates::held_text(Some(row.p90_seconds))),
        )))
        .into_any_element()
}

/// One queue ticket: its kind, short command, folder and time waited, then
/// its `usual` time when known, the wait in the danger colour past it.
fn queue_row(
    n: usize,
    ticket: &GateQueueTicket,
    usual: Option<f64>,
    now: DateTime<Utc>,
    offset: i8,
    t: &UiTheme,
) -> AnyElement {
    let selector = format!("gates-queue-{n}");
    let short = ticket
        .command_line
        .as_deref()
        .map(gates::short_command)
        .unwrap_or_default();
    let folder = ticket
        .cwd
        .as_deref()
        .map(gates::folder_label)
        .unwrap_or_default();
    let waited = ticket
        .arrived
        .map(|since| needs_you::waited(now, since))
        .unwrap_or_default();
    let over = ticket
        .arrived
        .is_some_and(|since| gates::over_usual((now - since).num_seconds(), usual));
    div()
        .driver_id({
            let selector = selector.clone();
            move || selector
        })
        .flex()
        .items_center()
        .gap(px(6.0))
        .h(px(ROW_HEIGHT))
        .px(px(12.0))
        .text_size(ui_px_at(offset, SMALL_TEXT))
        .child(
            div()
                .flex_none()
                .w(px(NAME_WIDTH))
                .text_color(gpui::rgb(t.faint))
                .child(ticket.kind.clone()),
        )
        .child(mono(
            tagged_box(&format!("{selector}-command"), &short)
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .child(truncating(
                    &format!("{selector}-command"),
                    StyledText::new(short.clone()),
                )),
            offset,
        ))
        .child(
            div()
                .flex_none()
                .text_color(gpui::rgb(t.subtle))
                .child(tagged_text(format!("{selector}-folder"), folder)),
        )
        .child(timed(
            &selector,
            waited,
            if over { t.danger } else { t.ochre },
            usual,
            offset,
            t,
        ))
        .into_any_element()
}

/// The `n`th gate waiting for pool `key`: its ordinal, command and time
/// waited, then its `usual` time when known, the wait in the danger colour
/// past it; then its folder.
#[expect(
    clippy::too_many_arguments,
    reason = "the row's data, its usual time and the text sizes and theme every row takes"
)]
fn wait_row(
    key: &str,
    n: usize,
    gate: &GateWaiting,
    usual: Option<f64>,
    now: DateTime<Utc>,
    offset: i8,
    shift: i8,
    t: &UiTheme,
) -> AnyElement {
    let selector = format!("gates-wait-{key}-{n}");
    let short = gates::short_command(&gate.command_line);
    let folder = gate
        .cwd
        .as_deref()
        .map(gates::folder_label)
        .unwrap_or_default();
    let waited = gate
        .since
        .map(|since| needs_you::waited(now, since))
        .unwrap_or_default();
    let over = gate
        .since
        .is_some_and(|since| gates::over_usual((now - since).num_seconds(), usual));
    let tip = [Some(gate.command_line.as_str()), gate.cwd.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
    let line_one = div()
        .flex()
        .items_center()
        .gap(px(COLUMN_GAP))
        .h(px(ROW_HEIGHT))
        .min_w(px(0.0))
        .child(
            div()
                .flex_none()
                .size(px(GLYPH))
                .rounded(px(2.0))
                .border_1()
                .border_color(gpui::rgb(t.ochre)),
        )
        .child(
            div()
                .flex_none()
                .w(px(NAME_WIDTH))
                .text_size(ui_px_at(offset, 11.0))
                .text_color(gpui::rgb(t.ochre))
                .font_weight(fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
                .child(tagged_text(
                    format!("{selector}-name"),
                    gates::ordinal(n + 1),
                )),
        )
        .child(mono(
            tagged_box(&format!("{selector}-command"), &short)
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .child(truncating(
                    &format!("{selector}-command"),
                    StyledText::new(short.clone()),
                )),
            offset,
        ))
        .child(timed(
            &selector,
            waited,
            if over { t.danger } else { t.ochre },
            usual,
            offset,
            t,
        ));
    let line_two = div()
        .pl(px(LINE_TWO_INDENT))
        .pb(px(4.0))
        .whitespace_nowrap()
        .overflow_hidden()
        .text_size(ui_px_at(offset, SMALL_TEXT))
        .text_color(gpui::rgb(t.subtle))
        .child(tagged_text(format!("{selector}-folder"), folder));
    div()
        .id(SharedString::from(selector.clone()))
        .driver_id(move || selector)
        .flex()
        .flex_col()
        .w_full()
        .px(px(12.0))
        .tooltip(tooltip(tip))
        .child(line_one)
        .child(line_two)
        .into_any_element()
}

/// A row's time: `text` tagged `{selector}-time` in `color`, then, with a
/// `usual` time, a faint ` / {usual}` in its own element tagged
/// `{selector}-usual`.
fn timed(
    selector: &str,
    text: String,
    color: u32,
    usual: Option<f64>,
    offset: i8,
    t: &UiTheme,
) -> Div {
    let usual = usual.map(|usual| {
        let tag = format!("{selector}-usual");
        div()
            .driver_id({
                let tag = tag.clone();
                move || tag
            })
            .text_color(gpui::rgb(t.faint))
            .child(tagged_text(tag, gates::usual_text(usual)))
    });
    div()
        .flex()
        .flex_none()
        .whitespace_nowrap()
        .text_size(ui_px_at(offset, SMALL_TEXT))
        .child(
            div()
                .text_color(gpui::rgb(color))
                .child(tagged_text(format!("{selector}-time"), text)),
        )
        .children(usual)
}

/// A caps heading: `text` in the small semibold heading style.
fn caps(text: &str, offset: i8, shift: i8, t: &UiTheme) -> Div {
    div()
        .flex_none()
        .whitespace_nowrap()
        .text_size(ui_px_at(offset, CAPS_TEXT))
        .font_weight(fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
        .text_color(gpui::rgb(t.text_2))
        .child(SharedString::from(text.to_owned()))
}

/// A muted line under a pool header, in `color`.
fn note_line(offset: i8, color: u32) -> Div {
    div()
        .px(px(12.0))
        .pb(px(2.0))
        .text_size(ui_px_at(offset, SMALL_TEXT))
        .text_color(gpui::rgb(color))
}

/// A box a spec finds by `<selector>=<text>`, for text drawn by a
/// truncating or ellipsized child.
fn tagged_box(selector: &str, text: &str) -> Div {
    let tag = format!("{selector}={text}");
    div().driver_id(move || tag)
}

/// `element` in the mono family the toasts' command uses.
fn mono(element: Div, offset: i8) -> Div {
    element
        .font_family(DEFAULT_FAMILY)
        .font_weight(fonts::mono_weight())
        .text_size(ui_px_at(offset, SMALL_TEXT))
}

/// A slot's glyph: a working (or, remote, lilac) box with a bar across its
/// top while busy, an ochre box while held, a faint outline while free.
fn slot_glyph(state: GateSlotState, remote: bool, t: &UiTheme) -> Div {
    let glyph = div()
        .flex_none()
        .size(px(GLYPH))
        .rounded(px(2.0))
        .mx(px((GLYPH_COLUMN - GLYPH) / 2.0));
    match state {
        GateSlotState::Busy => {
            let color = if remote { t.lilac } else { t.working };
            glyph
                .border_2()
                .border_color(gpui::rgb(color))
                .child(div().h(px(2.0)).bg(gpui::rgb(color)))
        }
        GateSlotState::Held => glyph
            .border_2()
            .border_color(gpui::rgb(t.ochre))
            .bg(gpui::rgba((t.ochre << 8) | 0x55)),
        GateSlotState::Free | GateSlotState::Unknown => {
            glyph.border_1().border_color(gpui::rgb(t.line_strong))
        }
    }
}
