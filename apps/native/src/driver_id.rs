//! Names an element for the UI specs and the headless driver.

use gpui::{InteractiveElement, ParentElement};

/// Names an element with the selector the UI specs look it up by.
pub(crate) trait DriverId: Sized {
    /// Tags this element `name()`: a gpui debug selector for the UI specs
    /// and, in a `driver` build, also an entry in the driver's registry
    /// ([`crate::driver::registry`]), recorded by an extra child that paints
    /// nothing.
    #[must_use]
    fn driver_id(self, name: impl FnOnce() -> String) -> Self;
}

impl<E: InteractiveElement + ParentElement> DriverId for E {
    #[cfg(not(feature = "driver"))]
    fn driver_id(self, name: impl FnOnce() -> String) -> Self {
        self.debug_selector(name)
    }

    #[cfg(feature = "driver")]
    fn driver_id(self, name: impl FnOnce() -> String) -> Self {
        let name = name();
        let selector = name.clone();
        self.debug_selector(move || selector)
            .child(crate::driver::registry::recorder(name))
    }
}
