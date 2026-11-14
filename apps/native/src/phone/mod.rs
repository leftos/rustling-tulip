//! The phone app's shell: one full-screen window padded clear of the notch and the home indicator.

use gpui::{
    App, AppContext as _, Context, Edges, IntoElement, ParentElement as _, Pixels, Render,
    Styled as _, Window, WindowInsets, WindowOptions, div,
};

/// The text the phone shows while it knows no host.
const EMPTY_TEXT: &str = "No hosts yet";

/// Starts the phone app: logging to stderr, then one full-screen window.
/// `on_start` runs first inside the app, before the window opens.
pub fn run(on_start: impl FnOnce(&mut App) + 'static) {
    // Another subscriber already set is the only error, and it still logs.
    let _already_set = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
    gpui_platform::application()
        .with_assets(crate::assets::Assets)
        .run(move |cx: &mut App| {
            on_start(cx);
            let opened = cx.open_window(WindowOptions::default(), |window, cx| {
                cx.new(|cx| PhoneRoot::new(window, cx))
            });
            if let Err(err) = opened {
                tracing::error!(error = %err, "phone window failed to open");
                cx.quit();
                return;
            }
            cx.activate(true);
        });
}

/// The padding that keeps content clear of the system's UI: the safe area,
/// which covers the notch, the status bar and the home indicator.
#[must_use]
pub fn content_padding(insets: &WindowInsets) -> Edges<Pixels> {
    insets.safe_area
}

/// The phone's root view: the empty state, inside the safe area.
struct PhoneRoot {
    /// Whether the first render has logged the app's ready line.
    logged_ready: bool,
}

impl PhoneRoot {
    /// gpui refreshes the window when its insets change, and `render` reads
    /// them afresh, so the view needs no subscription of its own.
    fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            logged_ready: false,
        }
    }
}

impl Render for PhoneRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The ready line follows the first frame, not the window's opening.
        if !self.logged_ready {
            self.logged_ready = true;
            tracing::info!("phone app started");
        }
        let t = crate::ui_theme::theme(cx);
        let padding = content_padding(&window.insets());
        div()
            .size_full()
            .bg(gpui::rgb(t.ground))
            .pt(padding.top)
            .pr(padding.right)
            .pb(padding.bottom)
            .pl(padding.left)
            .child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(gpui::rgb(t.muted))
                    .child(EMPTY_TEXT),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::px;

    #[test]
    fn padding_is_the_safe_area() {
        let safe_area = Edges {
            top: px(62.0),
            bottom: px(34.0),
            ..Edges::default()
        };
        let insets = WindowInsets {
            safe_area,
            ..WindowInsets::default()
        };
        assert_eq!(content_padding(&insets), safe_area);
    }

    #[test]
    fn no_insets_means_no_padding() {
        assert_eq!(
            content_padding(&WindowInsets::default()),
            Edges::<Pixels>::default()
        );
    }
}
