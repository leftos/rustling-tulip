//! Where each window last laid out its pane grid. The main window and every
//! pop-out that draws a grid keep their own entry, so a divider drag maps the
//! pointer through the grid of the window it started in.

use std::collections::HashMap;

use gpui::{Bounds, Pixels, WindowId};

/// Each window's grid bounds, by window, and which window is the main one.
pub(crate) struct WindowLayouts {
    main: WindowId,
    grids: HashMap<WindowId, Bounds<Pixels>>,
}

impl WindowLayouts {
    /// No grid laid out yet; `main` is the main window.
    pub(crate) fn new(main: WindowId) -> Self {
        Self {
            main,
            grids: HashMap::new(),
        }
    }

    /// The main window's id.
    pub(crate) fn main(&self) -> WindowId {
        self.main
    }

    /// `window` laid its grid out in `bounds`.
    pub(crate) fn set_grid(&mut self, window: WindowId, bounds: Bounds<Pixels>) {
        self.grids.insert(window, bounds);
    }

    /// Where `window` last laid its grid out, if it has.
    pub(crate) fn grid(&self, window: WindowId) -> Option<Bounds<Pixels>> {
        self.grids.get(&window).copied()
    }

    /// Where the main window last laid its grid out, if it has.
    pub(crate) fn main_grid(&self) -> Option<Bounds<Pixels>> {
        self.grid(self.main)
    }

    /// `window` closed: its grid is gone.
    pub(crate) fn forget(&mut self, window: WindowId) {
        self.grids.remove(&window);
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, Pixels, WindowId, point, px, size};

    use crate::window_layouts::WindowLayouts;

    fn rect(x: f32, width: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(0.0)), size(px(width), px(400.0)))
    }

    fn ids() -> (WindowId, WindowId) {
        (WindowId::from(1_u64), WindowId::from(2_u64))
    }

    #[test]
    fn window_layouts_keep_bounds_per_window() {
        let (main, popout) = ids();
        let mut layouts = WindowLayouts::new(main);
        assert_eq!(layouts.main_grid(), None);
        layouts.set_grid(main, rect(0.0, 800.0));
        layouts.set_grid(popout, rect(10.0, 500.0));
        assert_eq!(layouts.main_grid(), Some(rect(0.0, 800.0)));
        assert_eq!(layouts.grid(popout), Some(rect(10.0, 500.0)));
        layouts.set_grid(popout, rect(20.0, 600.0));
        assert_eq!(layouts.grid(popout), Some(rect(20.0, 600.0)));
        assert_eq!(layouts.grid(main), Some(rect(0.0, 800.0)));
    }

    #[test]
    fn a_divider_drag_reads_its_own_windows_bounds() {
        let (main, popout) = ids();
        let mut layouts = WindowLayouts::new(main);
        layouts.set_grid(main, rect(0.0, 800.0));
        layouts.set_grid(popout, rect(10.0, 500.0));
        assert_eq!(layouts.grid(popout), Some(rect(10.0, 500.0)));
        assert_eq!(layouts.grid(main), Some(rect(0.0, 800.0)));
        layouts.forget(popout);
        assert_eq!(
            layouts.grid(popout),
            None,
            "a drag in a window with no grid reads none, not the main one's"
        );
    }

    #[test]
    fn closing_a_window_drops_its_bounds() {
        let (main, popout) = ids();
        let mut layouts = WindowLayouts::new(main);
        layouts.set_grid(main, rect(0.0, 800.0));
        layouts.set_grid(popout, rect(10.0, 500.0));
        layouts.forget(popout);
        assert_eq!(layouts.grid(popout), None);
        assert_eq!(layouts.main_grid(), Some(rect(0.0, 800.0)));
        layouts.forget(popout);
        assert_eq!(layouts.main_grid(), Some(rect(0.0, 800.0)));
    }
}
