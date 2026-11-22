//! The phone session page's prompt sheet as plain Rust: whether the chat's
//! pending prompt rises as a bottom sheet over the page, the prompt a swipe
//! put away, and whether a downward drag on the sheet puts it away.
//! `session_view` draws it.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    App, ClickEvent, Context, DispatchPhase, Div, Edges, Pixels, ScrollEvent, ScrollSource,
    TouchPhase, Window, canvas, div, prelude::*, px,
};

use super::PhoneRoot;
use super::home_view::{SIDE_PADDING, TOUCH_TARGET};
use super::hosts_view::phone_button;
use super::terminal_view::{HeightSlot, TerminalHeights};
use crate::buttons::ButtonKind;
use crate::driver_id::DriverId as _;
use crate::ui_theme::theme;

/// The sheet's keys: their selectors, labels and the bytes each types.
const SHEET_KEYS: [(&str, &str, &[u8]); 3] = [
    ("phone-prompt-esc", "Esc", b"\x1b"),
    ("phone-prompt-up", "\u{2191}", b"\x1b[A"),
    ("phone-prompt-down", "\u{2193}", b"\x1b[B"),
];
/// The label of the button that raises a put-away prompt again.
const SHOW_PROMPT_LABEL: &str = "Show prompt";

/// How far back from the lift the release velocity looks, as the tray's
/// swipe does.
const VELOCITY_WINDOW: Duration = Duration::from_millis(40);
/// The share of the sheet's height a drag passes to put the sheet away.
const DISMISS_SHARE: f32 = 1.0 / 3.0;
/// The release velocity, in pixels per second, at which a shorter drag
/// still puts the sheet away: a flick.
const FLING_PX_PER_S: f32 = 300.0;
/// The least travel a flick needs, so a tap's jitter never puts it away.
const FLICK_MIN_TRAVEL: f32 = 24.0;

/// Whether a downward drag that travelled `travel` (down above zero) and
/// was released at `velocity` (down above zero) over a sheet `height` tall
/// puts the sheet away: past a third of its height, or a flick of at least
/// [`FLICK_MIN_TRAVEL`] released at [`FLING_PX_PER_S`] or faster downward.
pub(crate) fn commits(travel: f32, velocity: f32, height: f32) -> bool {
    if travel <= 0.0 {
        return false;
    }
    let past_third = height > 0.0 && travel > height * DISMISS_SHARE;
    let flick = travel >= FLICK_MIN_TRAVEL && velocity >= FLING_PX_PER_S;
    past_third || flick
}

/// A touch down on the sheet: how far it has travelled down and its recent
/// movements, for the release velocity.
#[derive(Debug, Default)]
struct Drag {
    /// The prompt whose sheet the touch went down on.
    prompt: String,
    travel: f32,
    samples: Vec<(Instant, f32)>,
}

impl Drag {
    fn moved(&mut self, dy: f32, at: Instant) {
        self.travel += dy;
        self.samples.push((at, dy));
        self.samples
            .retain(|(when, _)| at.saturating_duration_since(*when) <= VELOCITY_WINDOW);
    }

    /// The downward velocity at a release at `at`, in pixels per second:
    /// the movements in the last [`VELOCITY_WINDOW`] over the time they
    /// took; none when the finger rested longer than that or they took no
    /// time.
    fn velocity(&self, at: Instant) -> f32 {
        let recent: Vec<&(Instant, f32)> = self
            .samples
            .iter()
            .filter(|(when, _)| at.saturating_duration_since(*when) <= VELOCITY_WINDOW)
            .collect();
        let (Some((first, _)), Some((last, _))) = (recent.first(), recent.last()) else {
            return 0.0;
        };
        let span = last.saturating_duration_since(*first).as_secs_f32();
        if span <= 0.0 {
            return 0.0;
        }
        let moved: f32 = recent.iter().skip(1).map(|(_, dy)| dy).sum();
        moved / span
    }
}

/// What the sheet remembers while a session is shown: the prompt a swipe
/// put away and the drag in progress.
#[derive(Debug, Default)]
pub(crate) struct SheetState {
    /// The id of the prompt a swipe or back put away.
    dismissed: Option<String>,
    /// The touch down on the sheet, while there is one.
    drag: Option<Drag>,
    /// The sheet's height as last drawn, which a drag's lift is measured
    /// against.
    height: Rc<Cell<f32>>,
    /// Whether the rest of the pan whose lift put the sheet away, its
    /// momentum included, is swallowed rather than scrolling the page.
    swallow_pan: bool,
}

impl SheetState {
    /// Whether the sheet rises for the chat's current prompt
    /// `current_prompt`: a prompt is up and was not put away.
    pub(crate) fn shows(&self, current_prompt: Option<&str>) -> bool {
        current_prompt.is_some_and(|id| self.dismissed.as_deref() != Some(id))
    }

    /// Whether `current_prompt` is up and was put away, so `Show prompt`
    /// shows.
    pub(crate) fn put_away(&self, current_prompt: Option<&str>) -> bool {
        current_prompt.is_some_and(|id| self.dismissed.as_deref() == Some(id))
    }

    /// A swipe or back put the sheet of prompt `prompt_id` away.
    pub(crate) fn swiped(&mut self, prompt_id: &str) {
        self.dismissed = Some(prompt_id.to_owned());
        self.drag = None;
    }

    /// `Show prompt`'s tap: the put-away prompt rises again.
    pub(crate) fn show_again(&mut self) {
        self.dismissed = None;
    }

    /// A touch went down on the sheet of prompt `prompt_id`, already `dy`
    /// down, at `at`.
    pub(crate) fn drag_started(&mut self, prompt_id: &str, dy: f32, at: Instant) {
        let mut drag = Drag {
            prompt: prompt_id.to_owned(),
            ..Drag::default()
        };
        drag.moved(dy, at);
        self.drag = Some(drag);
    }

    /// Drops the drag unless it began on `current_prompt`, the prompt
    /// drawn now.
    pub(crate) fn keep_drag_of(&mut self, current_prompt: Option<&str>) {
        if self
            .drag
            .as_ref()
            .is_some_and(|drag| Some(drag.prompt.as_str()) != current_prompt)
        {
            self.drag = None;
        }
    }

    /// The touch moved `dy` down at `at` while `current_prompt` is drawn;
    /// nothing while no drag of that prompt is down.
    pub(crate) fn drag_moved(&mut self, current_prompt: Option<&str>, dy: f32, at: Instant) {
        self.keep_drag_of(current_prompt);
        if let Some(drag) = self.drag.as_mut() {
            drag.moved(dy, at);
        }
    }

    /// The touch lifted after a last `dy` at `at` over a sheet `height`
    /// tall while `current_prompt` is drawn: when it commits ([`commits`])
    /// the prompt is put away and the rest of the pan is swallowed; else the
    /// sheet snaps back. Returns whether it put the prompt away.
    pub(crate) fn drag_ended(
        &mut self,
        current_prompt: Option<&str>,
        dy: f32,
        at: Instant,
        height: f32,
    ) -> bool {
        self.keep_drag_of(current_prompt);
        let Some(mut drag) = self.drag.take() else {
            return false;
        };
        drag.travel += dy;
        let committed = commits(drag.travel, drag.velocity(at), height);
        if committed {
            self.swiped(&drag.prompt);
            self.swallow_pan = true;
        }
        committed
    }

    /// A touch pan event in `phase` reaches the page: whether it is the
    /// rest of the pan whose lift put the sheet away, and so is swallowed.
    /// That pan's last event (its momentum's end) and the next pan's start
    /// end the swallowing; a new pan is never swallowed.
    pub(crate) fn pan_captured(&mut self, phase: TouchPhase) -> bool {
        match phase {
            TouchPhase::Started => {
                self.swallow_pan = false;
                false
            }
            TouchPhase::Moved => self.swallow_pan,
            TouchPhase::Ended | TouchPhase::Cancelled => std::mem::take(&mut self.swallow_pan),
        }
    }

    /// Another sheet's swipe (the Linear card's) committed: the rest of its
    /// pan, its momentum included, is swallowed as this sheet's own is.
    pub(crate) fn swallow_rest_of_pan(&mut self) {
        self.swallow_pan = true;
    }

    /// The system took the touch: the sheet snaps back.
    pub(crate) fn drag_cancelled(&mut self) {
        self.drag = None;
    }

    /// How far down the sheet is drawn off its place: the drag's travel,
    /// never up.
    pub(crate) fn offset(&self) -> f32 {
        self.drag.as_ref().map_or(0.0, |drag| drag.travel.max(0.0))
    }
}

impl PhoneRoot {
    /// The shown session's current prompt card's id, while its loaded chat
    /// has one ([`crate::RootView::phone_current_prompt`]): the one rule
    /// the draw, Back, `Show prompt` and the drag all read.
    fn current_prompt(&self, cx: &App) -> Option<String> {
        let (_, session_id) = self.shown.as_ref()?;
        self.root
            .read(cx)
            .phone_current_prompt(session_id)
            .map(str::to_owned)
    }

    /// Before a frame: the sheet's drag is dropped unless its prompt is
    /// still the one drawn.
    pub(super) fn sync_prompt_sheet(&mut self, cx: &App) {
        let current = self.current_prompt(cx);
        self.prompt_sheet.keep_drag_of(current.as_deref());
    }

    /// An empty layer that, before any element sees a touch pan, swallows
    /// the rest of the pan whose lift put the sheet away, its momentum
    /// included, so it never scrolls the page under the sheet.
    pub(super) fn pan_guard(cx: &Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        canvas(
            |_, _, _| {},
            move |_, (), window, _| {
                window.on_mouse_event(move |event: &ScrollEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture
                        || !matches!(event.source, ScrollSource::Touch)
                    {
                        return;
                    }
                    let swallowed = this
                        .update(cx, |this, _| {
                            this.prompt_sheet.pan_captured(event.touch_phase)
                        })
                        .unwrap_or(false);
                    if swallowed {
                        cx.stop_propagation();
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    /// Whether the prompt sheet is up over the shown session's page.
    pub(super) fn prompt_sheet_up(&self, cx: &App) -> bool {
        self.prompt_sheet.shows(self.current_prompt(cx).as_deref())
    }

    /// Puts the prompt sheet away, as a swipe does.
    pub(super) fn put_prompt_sheet_away(&mut self, cx: &mut Context<Self>) {
        if let Some(prompt_id) = self.current_prompt(cx) {
            self.prompt_sheet.swiped(&prompt_id);
        }
        cx.notify();
    }

    /// Whether `Show prompt` draws at the page's bottom: the current prompt
    /// was put away.
    pub(super) fn show_prompt_drawn(&self, cx: &App) -> bool {
        self.prompt_sheet
            .put_away(self.current_prompt(cx).as_deref())
    }

    /// `Show prompt` at the page's bottom, while the current prompt was put
    /// away, its height measured for the Terminal page.
    pub(super) fn show_prompt_button(&self, cx: &mut Context<Self>) -> Option<Div> {
        if !self.show_prompt_drawn(cx) {
            return None;
        }
        let button = phone_button("phone-prompt-show", ButtonKind::Primary, cx)
            .w_full()
            .min_h(px(TOUCH_TARGET))
            .child(SHOW_PROMPT_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.prompt_sheet.show_again();
                cx.notify();
            }));
        Some(
            div()
                .relative()
                .flex_none()
                .px(px(SIDE_PADDING))
                .py(px(8.0))
                .child(button)
                .child(TerminalHeights::measure(
                    &self.terminal_heights,
                    HeightSlot::Prompt,
                )),
        )
    }

    /// While the sheet is up: a scrim over the page that takes no tap, and
    /// the sheet at the bottom, clear of `padding.bottom`'s home indicator or
    /// keyboard: a grab handle, the chat's prompt card as the desktop draws
    /// it, then the Esc, up and down keys. Its top never passes
    /// `padding.top`'s band, so a card too tall for the room scrolls between
    /// the handle and the keys. A drag down on it puts it away.
    pub(super) fn prompt_sheet_layer(
        &self,
        padding: Edges<Pixels>,
        screen_height: Pixels,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        if !self.prompt_sheet_up(cx) {
            return None;
        }
        let bottom = padding.bottom;
        let cap = screen_height - padding.top - px(8.0);
        let (_, session_id) = self.shown.as_ref()?;
        let id = session_id.clone();
        let card = self
            .root
            .update(cx, |root, cx| root.phone_card_area(&id, cx))?;
        let t = theme(cx);
        let scrim = div()
            .id("phone-prompt-scrim")
            .driver_id(|| "phone-prompt-scrim".to_owned())
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(gpui::rgba(t.scrim))
            .occlude()
            // The scrim hides the page's root from the gesture, so it takes
            // the back gesture itself, and carries on a drag the sheet began
            // once the finger runs past the sheet's top.
            .on_back_gesture(cx.listener(Self::session_back_gesture))
            .on_scroll(cx.listener(Self::prompt_scrim_scrolled));
        let height = Rc::clone(&self.prompt_sheet.height);
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
                .driver_id(|| "phone-prompt-handle".to_owned())
                .w(px(36.0))
                .h(px(5.0))
                .rounded_full()
                .bg(gpui::rgb(t.muted)),
        );
        let card_area = div()
            .id("phone-prompt-card")
            .driver_id(|| "phone-prompt-card".to_owned())
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .child(card);
        let panel = div()
            .id("phone-prompt-sheet")
            .driver_id(|| "phone-prompt-sheet".to_owned())
            .occlude()
            .relative()
            .top(px(self.prompt_sheet.offset()))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .pt(px(4.0))
            .pb(bottom + px(16.0))
            .max_h(cap)
            .rounded_t(px(14.0))
            .border_t_1()
            .border_color(gpui::rgb(t.line))
            .bg(gpui::rgb(t.raised))
            .on_back_gesture(cx.listener(Self::session_back_gesture))
            .on_scroll(cx.listener(Self::prompt_sheet_scrolled))
            .child(measure)
            .child(handle)
            .child(card_area)
            .child(Self::sheet_keys(cx));
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
                .child(panel),
        )
    }

    /// The sheet's row of Outlined keys, each typing its bytes.
    fn sheet_keys(cx: &mut Context<Self>) -> Div {
        div()
            .flex_none()
            .flex()
            .gap(px(8.0))
            .px(px(SIDE_PADDING))
            .children(SHEET_KEYS.map(|(id, label, bytes)| {
                phone_button(id, ButtonKind::Outlined, cx)
                    .flex_1()
                    .min_h(px(TOUCH_TARGET))
                    .child(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.type_sheet_key(bytes, cx);
                    }))
            }))
    }

    /// One of the sheet's keys: `bytes` typed into the shown session.
    fn type_sheet_key(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        let Some((_, session_id)) = self.shown.clone() else {
            return;
        };
        self.root
            .update(cx, |root, _| root.send_phone_input(&session_id, bytes));
    }

    /// A pan over the sheet: its downward travel feeds the drag, and a lift
    /// that commits ([`commits`]) puts the sheet away.
    fn prompt_sheet_scrolled(
        &mut self,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dy = f32::from(event.delta.pixel_delta(window.line_height()).y);
        let at = cx.background_executor().now();
        let current = self.current_prompt(cx);
        let current = current.as_deref();
        match event.touch_phase {
            TouchPhase::Started => {
                if let Some(prompt_id) = current {
                    self.prompt_sheet.drag_started(prompt_id, dy, at);
                }
            }
            TouchPhase::Moved => self.prompt_sheet.drag_moved(current, dy, at),
            TouchPhase::Ended => {
                let height = self.prompt_sheet.height.get();
                self.prompt_sheet.drag_ended(current, dy, at, height);
            }
            TouchPhase::Cancelled => self.prompt_sheet.drag_cancelled(),
        }
        cx.notify();
    }

    /// A pan over the scrim: it carries on the sheet's drag in progress,
    /// but never starts one.
    fn prompt_scrim_scrolled(
        &mut self,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.touch_phase != TouchPhase::Started {
            self.prompt_sheet_scrolled(event, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_current_prompt_shows_the_sheet() {
        let state = SheetState::default();
        assert!(state.shows(Some("p1")));
        assert!(!state.put_away(Some("p1")));
    }

    #[test]
    fn no_prompt_shows_no_sheet() {
        let mut state = SheetState::default();
        assert!(!state.shows(None));
        state.swiped("p1");
        assert!(!state.shows(None));
        assert!(!state.put_away(None), "no prompt, no Show prompt");
    }

    #[test]
    fn a_swiped_prompt_stays_down() {
        let mut state = SheetState::default();
        state.swiped("p1");
        assert!(!state.shows(Some("p1")));
        assert!(state.put_away(Some("p1")));
    }

    #[test]
    fn a_different_prompt_raises_it_again() {
        let mut state = SheetState::default();
        state.swiped("p1");
        assert!(state.shows(Some("p2")));
        assert!(!state.put_away(Some("p2")));
    }

    #[test]
    fn show_again_raises_a_dismissed_prompt() {
        let mut state = SheetState::default();
        state.swiped("p1");
        state.show_again();
        assert!(state.shows(Some("p1")));
        assert!(!state.put_away(Some("p1")));
    }

    #[test]
    fn a_short_slow_drag_does_not_commit() {
        assert!(!commits(40.0, 50.0, 300.0));
        assert!(!commits(10.0, 900.0, 300.0), "too short for a flick");
        assert!(
            !commits(-200.0, -900.0, 300.0),
            "an upward drag never commits"
        );
        assert!(!commits(40.0, -900.0, 300.0), "released moving back up");
    }

    #[test]
    fn a_long_drag_commits() {
        assert!(commits(101.0, 0.0, 300.0));
        assert!(!commits(99.0, 0.0, 300.0));
    }

    #[test]
    fn a_drag_is_dropped_when_its_prompt_leaves() {
        let at = Instant::now();
        let mut state = SheetState::default();
        state.drag_started("p1", 0.0, at);
        state.drag_moved(Some("p1"), 50.0, at);
        assert!(state.offset() > 0.0, "the sheet follows the drag");

        state.keep_drag_of(None);

        assert!(state.offset() <= 0.0, "the drag is dropped");
        state.drag_moved(Some("p1"), 200.0, at);
        assert!(state.offset() <= 0.0, "a dropped drag moves nothing");
        assert!(
            !state.drag_ended(Some("p1"), 0.0, at, 300.0),
            "nor does its lift put anything away"
        );
        assert!(state.shows(Some("p1")));
    }

    #[test]
    fn a_drag_never_dismisses_a_different_prompt() {
        let at = Instant::now();
        let mut state = SheetState::default();
        state.drag_started("p1", 0.0, at);
        state.drag_moved(Some("p1"), 200.0, at);

        assert!(!state.drag_ended(Some("p2"), 0.0, at, 300.0));

        assert!(state.shows(Some("p2")), "p2 is not put away");
        assert!(state.shows(Some("p1")), "nor is p1");
        state.drag_started("p1", 0.0, at);
        state.drag_moved(Some("p2"), 200.0, at);
        assert!(
            state.offset() <= 0.0,
            "a move under another prompt drops the drag"
        );
    }

    #[test]
    fn a_flick_commits() {
        assert!(commits(30.0, 400.0, 300.0));
    }
}
