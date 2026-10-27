//! Which window each overlay opened in, and which window a new toast shows
//! in. A menu or dialog opened from a pop-out draws there, in that window's
//! coordinates; a toast goes to the window the user last pressed a mouse
//! button or a key in, else the main one, and stays there.

use std::collections::HashMap;

use gpui::WindowId;

/// The overlays a window other than the main one can open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Overlay {
    SessionMenu,
    ShellMenu,
    EmptyPaneMenu,
    PaneClose,
}

/// The window each overlay and each toast belongs to, and the window the
/// user last interacted with.
pub(crate) struct OverlayOwners {
    main: WindowId,
    overlays: HashMap<Overlay, WindowId>,
    toasts: HashMap<u64, WindowId>,
    last_interacted: Option<WindowId>,
}

impl OverlayOwners {
    /// Nothing open yet; `main` is the main window.
    pub(crate) fn new(main: WindowId) -> Self {
        Self {
            main,
            overlays: HashMap::new(),
            toasts: HashMap::new(),
            last_interacted: None,
        }
    }

    /// `overlay` opened in `window`.
    pub(crate) fn opened(&mut self, overlay: Overlay, window: WindowId) {
        self.overlays.insert(overlay, window);
    }

    /// The window `overlay` belongs to: the one it last opened in, else the
    /// main one.
    pub(crate) fn owner(&self, overlay: Overlay) -> WindowId {
        self.overlays.get(&overlay).copied().unwrap_or(self.main)
    }

    /// Whether `window` draws `overlay`.
    pub(crate) fn shows(&self, overlay: Overlay, window: WindowId) -> bool {
        self.owner(overlay) == window
    }

    /// The user pressed a mouse button or a key in `window`.
    pub(crate) fn interacted(&mut self, window: WindowId) {
        self.last_interacted = Some(window);
    }

    /// The window a new toast shows in: the last one interacted with, else
    /// the main one.
    pub(crate) fn toast_window(&self) -> WindowId {
        self.last_interacted.unwrap_or(self.main)
    }

    /// The toasts now on screen are `shown`: each one new to this record
    /// goes to [`Self::toast_window`], one already placed stays where it is,
    /// and the ones gone are forgotten.
    pub(crate) fn place_toasts(&mut self, shown: impl IntoIterator<Item = u64>) {
        let target = self.toast_window();
        let mut placed = HashMap::new();
        for id in shown {
            let window = self.toasts.get(&id).copied().unwrap_or(target);
            placed.insert(id, window);
        }
        self.toasts = placed;
    }

    /// The window toast `id` shows in; an unplaced toast shows in the main
    /// one.
    pub(crate) fn toast_owner(&self, id: u64) -> WindowId {
        self.toasts.get(&id).copied().unwrap_or(self.main)
    }

    /// `window` closed: its overlays go, returned in a fixed order so the
    /// caller can close them, its toasts move to the main window, and it is
    /// no longer the window last interacted with. The main window is never
    /// forgotten.
    pub(crate) fn forget_window(&mut self, window: WindowId) -> Vec<Overlay> {
        if window == self.main {
            return Vec::new();
        }
        let mut gone: Vec<Overlay> = self
            .overlays
            .iter()
            .filter(|(_, owner)| **owner == window)
            .map(|(overlay, _)| *overlay)
            .collect();
        gone.sort_unstable();
        for overlay in &gone {
            self.overlays.remove(overlay);
        }
        for owner in self.toasts.values_mut() {
            if *owner == window {
                *owner = self.main;
            }
        }
        if self.last_interacted == Some(window) {
            self.last_interacted = None;
        }
        gone
    }
}

#[cfg(test)]
mod tests {
    use gpui::WindowId;

    use crate::overlay_owner::{Overlay, OverlayOwners};

    fn ids() -> (WindowId, WindowId, WindowId) {
        (
            WindowId::from(1_u64),
            WindowId::from(2_u64),
            WindowId::from(3_u64),
        )
    }

    #[test]
    fn overlay_owner_records_the_window_that_opened_it() {
        let (main, popout, other) = ids();
        let mut owners = OverlayOwners::new(main);
        assert_eq!(owners.owner(Overlay::SessionMenu), main, "unopened: main");
        assert!(owners.shows(Overlay::PaneClose, main));

        owners.opened(Overlay::SessionMenu, popout);
        owners.opened(Overlay::PaneClose, other);
        assert_eq!(owners.owner(Overlay::SessionMenu), popout);
        assert!(owners.shows(Overlay::SessionMenu, popout));
        assert!(!owners.shows(Overlay::SessionMenu, main));
        assert_eq!(owners.owner(Overlay::PaneClose), other);
        assert_eq!(owners.owner(Overlay::ShellMenu), main);

        owners.opened(Overlay::SessionMenu, main);
        assert!(
            owners.shows(Overlay::SessionMenu, main),
            "opening it again in the main window moves it there"
        );
        assert!(!owners.shows(Overlay::SessionMenu, popout));
    }

    #[test]
    fn toasts_go_to_the_last_interacted_window_else_main() {
        let (main, popout, other) = ids();
        let mut owners = OverlayOwners::new(main);
        owners.place_toasts([1]);
        assert_eq!(owners.toast_owner(1), main, "no interaction yet: main");

        owners.interacted(popout);
        owners.place_toasts([1, 2]);
        assert_eq!(owners.toast_owner(1), main, "a shown toast stays put");
        assert_eq!(owners.toast_owner(2), popout, "a new one follows the press");

        owners.interacted(other);
        owners.place_toasts([2, 3]);
        assert_eq!(owners.toast_owner(2), popout);
        assert_eq!(owners.toast_owner(3), other);
        assert_eq!(owners.toast_owner(1), main, "a gone toast reads as main");

        owners.interacted(main);
        owners.place_toasts([2, 3, 4]);
        assert_eq!(owners.toast_owner(4), main);
        assert_eq!(owners.toast_owner(99), main, "an unplaced toast is main's");
    }

    #[test]
    fn closing_a_window_drops_its_overlays_and_hands_toasts_to_main() {
        let (main, popout, other) = ids();
        let mut owners = OverlayOwners::new(main);
        owners.opened(Overlay::ShellMenu, popout);
        owners.opened(Overlay::PaneClose, popout);
        owners.opened(Overlay::SessionMenu, other);
        owners.interacted(popout);
        owners.place_toasts([1]);
        assert_eq!(owners.toast_owner(1), popout);

        assert_eq!(
            owners.forget_window(popout),
            vec![Overlay::ShellMenu, Overlay::PaneClose]
        );
        assert_eq!(owners.owner(Overlay::ShellMenu), main);
        assert_eq!(owners.owner(Overlay::PaneClose), main);
        assert_eq!(
            owners.owner(Overlay::SessionMenu),
            other,
            "another window keeps its own"
        );
        assert_eq!(owners.toast_owner(1), main, "its toast moves to main");
        assert_eq!(
            owners.toast_window(),
            main,
            "a closed window takes no new toast"
        );
        assert_eq!(owners.forget_window(popout), Vec::new(), "twice is nothing");

        owners.opened(Overlay::EmptyPaneMenu, main);
        assert_eq!(
            owners.forget_window(main),
            Vec::new(),
            "main is never forgotten"
        );
        assert_eq!(owners.owner(Overlay::EmptyPaneMenu), main);
    }
}
