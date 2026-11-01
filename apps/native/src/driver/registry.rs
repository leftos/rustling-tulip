//! Where each named element was painted, per window, fed at prepaint.
//!
//! A window root's render opens a frame ([`begin_frame`]); every element
//! named with [`crate::driver_id::DriverId`] then records its bounds while
//! that frame is prepainted. A lookup answers only for names recorded in the
//! window's latest frame, so an element that left the tree reads as gone.

use std::collections::{HashMap, HashSet};

use gpui::{
    AnyWindowHandle, App, Bounds, Global, IntoElement, Pixels, Size, Styled, Window, WindowId,
    canvas,
};

/// One named element as the latest frame painted it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Entry {
    /// The element's padding box in window coordinates (a border, drawn
    /// outside it, is not included).
    pub bounds: Bounds<Pixels>,
    /// The part of `bounds` inside the content mask it was painted under;
    /// zero-sized at `bounds.origin` when a clipping ancestor hides it all.
    pub visible: Bounds<Pixels>,
    /// The window frame that recorded it.
    pub frame: u64,
}

/// Every window's frame counter and the names recorded in it.
#[derive(Default)]
struct Registry {
    windows: HashMap<WindowId, WindowFrames>,
}

impl Global for Registry {}

#[derive(Default)]
struct WindowFrames {
    frame: u64,
    entries: HashMap<String, Entry>,
}

impl WindowFrames {
    fn current(&self, name: &str) -> Option<Entry> {
        self.entries
            .get(name)
            .filter(|entry| entry.frame == self.frame)
            .copied()
    }
}

fn frames<'a>(window: &Window, cx: &'a mut App) -> &'a mut WindowFrames {
    cx.default_global::<Registry>()
        .windows
        .entry(window.window_handle().window_id())
        .or_default()
}

/// Opens `window`'s next frame. Called from a window root's render, before
/// any of its elements prepaint; drops the names the finished frame did not
/// record, and every window gpui has closed.
pub(crate) fn begin_frame(window: &Window, cx: &mut App) {
    let open: HashSet<WindowId> = cx
        .windows()
        .iter()
        .map(AnyWindowHandle::window_id)
        .collect();
    cx.default_global::<Registry>()
        .windows
        .retain(|id, _| open.contains(id));
    let frames = frames(window, cx);
    let finished = frames.frame;
    frames.entries.retain(|_, entry| entry.frame == finished);
    frames.frame += 1;
}

/// A child that paints nothing and records `name` with its parent's padding
/// box when prepainted: absolutely placed over the whole parent, it takes no
/// flex slot.
pub(crate) fn recorder(name: String) -> impl IntoElement {
    canvas(
        move |bounds, window, cx| record(name, bounds, window, cx),
        |_, (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

fn record(name: String, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    let clipped = bounds.intersect(&window.content_mask().bounds);
    let visible = if clipped.is_empty() {
        Bounds::new(bounds.origin, Size::default())
    } else {
        clipped
    };
    let frames = frames(window, cx);
    let frame = frames.frame;
    frames.entries.insert(
        name,
        Entry {
            bounds,
            visible,
            frame,
        },
    );
}

/// The element named `name` in `window`'s latest frame, or `None` when that
/// frame did not paint it.
pub fn lookup(cx: &App, window: WindowId, name: &str) -> Option<Entry> {
    cx.try_global::<Registry>()?
        .windows
        .get(&window)?
        .current(name)
}

/// Every name `window`'s latest frame painted, sorted.
pub fn names(cx: &App, window: WindowId) -> Vec<String> {
    let Some(frames) = cx
        .try_global::<Registry>()
        .and_then(|registry| registry.windows.get(&window))
    else {
        return Vec::new();
    };
    let mut names: Vec<String> = frames
        .entries
        .iter()
        .filter(|(_, entry)| entry.frame == frames.frame)
        .map(|(name, _)| name.clone())
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use gpui::{
        Context, IntoElement, ParentElement, Render, Styled, TestAppContext, Window, WindowId, div,
    };

    use super::{Registry, begin_frame, lookup};
    use crate::driver_id::DriverId as _;

    /// A window root that opens its frames and names one element.
    struct Named;

    impl Render for Named {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            begin_frame(window, cx);
            div()
                .size_full()
                .child(div().driver_id(|| "named".to_owned()).size_full())
        }
    }

    /// The windows the registry keeps frames for, sorted.
    fn kept(cx: &mut TestAppContext) -> Vec<WindowId> {
        cx.update(|cx| {
            let mut ids: Vec<WindowId> = cx
                .try_global::<Registry>()
                .map(|registry| registry.windows.keys().copied().collect())
                .unwrap_or_default();
            ids.sort();
            ids
        })
    }

    #[gpui::test]
    fn a_closed_windows_frames_are_dropped(cx: &mut TestAppContext) {
        let main = cx.add_window(|_, _| Named);
        let popout = cx.add_window(|_, _| Named);
        cx.run_until_parked();
        let (main_id, popout_id) = (main.window_id(), popout.window_id());
        let mut both = vec![main_id, popout_id];
        both.sort();
        assert_eq!(kept(cx), both, "each window drew a frame");
        assert!(
            cx.update(|cx| lookup(cx, popout_id, "named")).is_some(),
            "the pop-out's element is registered"
        );

        let closed = popout.update(cx, |_, window, _| window.remove_window());
        assert!(closed.is_ok(), "the pop-out closed: {closed:?}");
        let redrawn = main.update(cx, |_, window, _| window.refresh());
        assert!(redrawn.is_ok(), "the main window redraws: {redrawn:?}");
        cx.run_until_parked();

        assert_eq!(
            kept(cx),
            vec![main_id],
            "the closed pop-out's frames are gone"
        );
        assert_eq!(cx.update(|cx| lookup(cx, popout_id, "named")), None);
    }
}
