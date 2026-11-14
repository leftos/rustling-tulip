//! The phone app's shell: one full-screen window padded clear of the notch
//! and the home indicator, drawn by [`PhoneRoot`] over a phone-shape
//! [`RootView`].

mod hosts_view;

use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    App, AppContext as _, Context, Edges, Entity, IntoElement, ParentElement as _, Pixels, Render,
    Styled as _, Subscription, Window, WindowInsets, WindowOptions, div, px,
};

use crate::{
    LiveSeams, NoTaskbarBadge, NoToastPlacer, RootDeps, RootShape, RootView, SilentNotifier,
    SilentSounder,
};

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
            crate::bind_keys(cx);
            crate::fonts::register_bundled(cx);
            let opened = cx.open_window(WindowOptions::default(), |window, cx| {
                let root = cx.new(|cx| RootView::with_transport(phone_deps(), window, cx));
                cx.new(|cx| PhoneRoot::new(root, window, cx))
            });
            if let Err(err) = opened {
                tracing::error!(error = %err, "phone window failed to open");
                cx.quit();
                return;
            }
            cx.activate(true);
        });
}

/// What the phone's root view talks to: the live network thread, token
/// store and config dir, the desktop's notification, sound, toast and badge
/// seams silent, and an updater that never checks (a phone app does not
/// update itself).
fn phone_deps() -> RootDeps {
    let LiveSeams {
        open_host,
        ui_dir,
        paths,
        tokens,
    } = crate::live_seams();
    RootDeps {
        open_host,
        ui_dir,
        paths,
        wanted: None,
        now: Arc::new(Instant::now),
        quit: Box::new(|cx: &mut App| cx.quit()),
        open: crate::open::opener_for(true),
        notify: Arc::new(SilentNotifier),
        toast_placer: Arc::new(NoToastPlacer),
        sound: Arc::new(SilentSounder),
        badge: Rc::new(NoTaskbarBadge),
        pick_folder: crate::folder_picker::offscreen_folder_picker(),
        pick_file: crate::folder_picker::offscreen_file_picker(),
        pick_save: crate::folder_picker::offscreen_save_picker(),
        autostart: crate::remote_access::autostart_for(true),
        tokens,
        updater: Arc::new(crate::update::SystemUpdater::new(
            daemon_client::updates_dir().map_err(|err| format!("{err:#}")),
            false,
        )),
        start_mode: crate::start_guard::StartMode::Normal,
        shape: RootShape::Phone,
    }
}

/// The padding that keeps content clear of the system's UI: the safe area,
/// which covers the notch, the status bar and the home indicator.
#[must_use]
pub fn content_padding(insets: &WindowInsets) -> Edges<Pixels> {
    insets.safe_area
}

/// The phone's root view: the saved hosts or the first-run form, or while
/// a saved host is open the sidebar's session list at the window's width,
/// and the Pair dialog over them, inside the safe area. It draws only that
/// list of a phone-shape [`RootView`], whose sidebar has no This PC group,
/// so This PC's dormant slot never shows as a connection.
pub struct PhoneRoot {
    root: Entity<RootView>,
    /// The add form's fields, its refusal and whether it shows.
    hosts: hosts_view::PhoneHosts,
    /// Whether the first render has logged the app's ready line.
    logged_ready: bool,
    /// Redraws the phone whenever the root view changes.
    _root_changed: Subscription,
}

impl PhoneRoot {
    /// The phone's root over `root`. gpui refreshes the window when its
    /// insets change, and `render` reads them afresh, so the insets need no
    /// subscription of their own.
    pub fn new(root: Entity<RootView>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let root_changed = cx.observe(&root, |this, root, cx| {
            this.hosts.root_changed(root.read(cx));
            cx.notify();
        });
        Self {
            hosts: hosts_view::PhoneHosts::new(&root, cx),
            root,
            logged_ready: false,
            _root_changed: root_changed,
        }
    }

    /// The root view the phone reads and drives.
    #[must_use]
    pub fn root(&self) -> &Entity<RootView> {
        &self.root
    }

    /// Why the add form refused the typed host, while it says so.
    #[must_use]
    pub fn host_error(&self) -> Option<&str> {
        self.hosts.error()
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
        let body = if hosts_view::any_host_open(self.root.read(cx)) {
            let width = (window.viewport_size().width - padding.left - padding.right) / px(1.0);
            self.list_body(width, window, cx)
        } else {
            self.hosts_body(cx)
        };
        let pairing = self
            .root
            .update(cx, |root, cx| root.device_pairing_layer(cx));
        let padded = || {
            div()
                .pt(padding.top)
                .pr(padding.right)
                .pb(padding.bottom)
                .pl(padding.left)
        };
        div()
            .relative()
            .size_full()
            .bg(gpui::rgb(t.ground))
            .text_color(gpui::rgb(t.text))
            .child(padded().size_full().child(body))
            .children(pairing.map(|layer| {
                padded()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .child(div().relative().size_full().child(layer))
            }))
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
