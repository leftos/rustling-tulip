//! The phone app's shell: one full-screen window padded clear of the notch
//! and the home indicator, drawn by [`PhoneRoot`] over a phone-shape
//! [`RootView`].

mod hosts_view;
pub(crate) mod lifecycle;
pub mod push;

use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    App, AppContext as _, Context, Edges, Entity, IntoElement, ParentElement as _, Pixels, Render,
    Styled as _, Subscription, Window, WindowInsets, WindowOptions, WindowVisibility, div, px,
};
use protocol::ClientMessage;

use crate::target::HostKey;
use crate::{
    LiveSeams, NoTaskbarBadge, NoToastPlacer, RootDeps, RootShape, RootView, SilentNotifier,
    SilentSounder,
};
use push::{Authorization, Effect, PushPlatform, PushState};

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
                let platform: Arc<dyn PushPlatform> = Arc::new(push::UnsupportedPlatform);
                cx.new(|cx| PhoneRoot::new(root, platform, window, cx))
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
    /// The phone's push: permission, device token and install keys.
    platform: Arc<dyn PushPlatform>,
    /// Whether the first render has logged the app's ready line.
    logged_ready: bool,
    /// Redraws the phone whenever the root view changes.
    _root_changed: Subscription,
    /// Redials the dropped saved hosts when the window comes back to the
    /// foreground.
    _visibility: Subscription,
}

impl PhoneRoot {
    /// The phone's root over `root`, its push through `platform`. gpui
    /// refreshes the window when its insets change, and `render` reads them
    /// afresh, so the insets need no subscription of their own.
    pub fn new(
        root: Entity<RootView>,
        platform: Arc<dyn PushPlatform>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let root_changed = cx.observe(&root, |this, root, cx| {
            this.hosts.root_changed(root.read(cx));
            this.push_changed(cx);
            cx.notify();
        });
        if platform.supported() {
            Self::learn_token(&platform, cx);
        }
        let visibility = cx.observe_window_visibility(window, |this, visibility, window, cx| {
            if visibility == WindowVisibility::Visible {
                this.root
                    .update(cx, |root, cx| root.redial_saved_hosts(window, cx));
            }
        });
        Self {
            hosts: hosts_view::PhoneHosts::new(&root, cx),
            root,
            platform,
            logged_ready: false,
            _root_changed: root_changed,
            _visibility: visibility,
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

    /// With the permission already granted, reads this phone's device token
    /// so each host's push state compares against it.
    fn learn_token(platform: &Arc<dyn PushPlatform>, cx: &mut Context<Self>) {
        let platform = Arc::clone(platform);
        cx.spawn(async move |this, cx| {
            if platform.authorization().await != Authorization::Granted {
                return;
            }
            match platform.device_token().await {
                Ok(token) => {
                    // Fails only when the phone's root is gone.
                    this.update(cx, |this, cx| this.token_read(&token, cx)).ok();
                }
                Err(err) => tracing::warn!(error = %err, "phone: no device token"),
            }
        })
        .detach();
    }

    /// This phone's device token is `token`: a host on for another token
    /// re-registers.
    fn token_read(&mut self, token: &str, cx: &mut Context<Self>) {
        self.root.update(cx, |root, cx| {
            root.push_hosts_mut().set_current(token);
            cx.notify();
        });
    }

    /// The root view changed: the queued effects run, then the saved
    /// tailnet hosts open and taking relay pushes, with their connection
    /// generations, go to the switches' model, and the effects it makes run.
    fn push_changed(&mut self, cx: &mut Context<Self>) {
        if !self.platform.supported() {
            return;
        }
        let open = hosts_view::relay_push_hosts(self.root.read(cx));
        let effects = self.root.update(cx, |root, _| {
            let push = root.push_hosts_mut();
            let mut effects = push.take_effects();
            effects.extend(push.hosts_open(&open));
            effects
        });
        self.run_effects(effects, cx);
    }

    /// Runs `step` on `host`'s switch, then the effects it and earlier steps
    /// queued, redrawing the switch.
    fn step_push(
        &mut self,
        host: &HostKey,
        step: impl FnOnce(&mut PushState, Option<&str>) -> Vec<Effect>,
        cx: &mut Context<Self>,
    ) {
        let effects = self.root.update(cx, |root, cx| {
            let push = root.push_hosts_mut();
            push.step(host, step);
            cx.notify();
            push.take_effects()
        });
        self.run_effects(effects, cx);
    }

    /// Runs each of `effects` for its host.
    fn run_effects(&mut self, effects: Vec<(HostKey, Effect)>, cx: &mut Context<Self>) {
        for (host, effect) in effects {
            match effect {
                Effect::SendGetStatus { request_id } => {
                    self.send(&host, ClientMessage::GetPushStatus { request_id }, cx);
                }
                Effect::Unregister { request_id } => {
                    self.send(&host, ClientMessage::UnregisterPush { request_id }, cx);
                }
                Effect::RequestAuthorization => self.request_authorization(host, cx),
                Effect::Register { request_id } => self.register(host, request_id, cx),
            }
        }
    }

    /// Sends `msg` to `host`'s daemon.
    fn send(&self, host: &HostKey, msg: ClientMessage, cx: &mut Context<Self>) {
        self.root.read(cx).send_to(host, msg);
    }

    /// Asks the owner for the notification permission, then folds the
    /// answer into `host`'s switch.
    fn request_authorization(&self, host: HostKey, cx: &mut Context<Self>) {
        let asked = self.platform.request_authorization();
        cx.spawn(async move |this, cx| {
            let answer = asked.await;
            // Fails only when the phone's root is gone.
            this.update(cx, |this, cx| {
                this.step_push(&host, |state, _| state.authorization_answered(answer), cx);
            })
            .ok();
        })
        .detach();
    }

    /// Reads the device token, then registers this phone with `host` for
    /// all three kinds of push.
    fn register(&self, host: HostKey, request_id: String, cx: &mut Context<Self>) {
        let token = self.platform.device_token();
        cx.spawn(async move |this, cx| {
            let token = token.await;
            // Fails only when the phone's root is gone.
            this.update(cx, |this, cx| {
                this.send_registration(&host, request_id, token, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Sends `host` the registration for `token`, or, when the platform gave
    /// no token or keys, logs why and marks the switch failed; either only
    /// while `request_id` is still the switch's registration out, else the
    /// completion is dropped.
    fn send_registration(
        &mut self,
        host: &HostKey,
        request_id: String,
        token: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        if !self
            .root
            .read(cx)
            .push_hosts()
            .token_ready(host, &request_id)
        {
            tracing::debug!(
                ?host,
                %request_id,
                "phone: dropping a push registration no longer out"
            );
            return;
        }
        let registration = token.and_then(|token| {
            push::apns_registration(self.platform.as_ref(), token.clone()).map(|reg| (token, reg))
        });
        match registration {
            Ok((token, registration)) => self.root.update(cx, |root, cx| {
                root.push_hosts_mut().set_current(&token);
                root.send_to(
                    host,
                    ClientMessage::RegisterPush {
                        registration,
                        needs_you: true,
                        finished: true,
                        hold_at_desk: true,
                        request_id,
                    },
                );
                cx.notify();
            }),
            Err(err) => {
                tracing::warn!(error = %err, ?host, "phone: push registration failed");
                self.root.update(cx, |root, cx| {
                    if root.push_hosts_mut().failed(host, &request_id) {
                        cx.notify();
                    }
                });
            }
        }
    }

    /// `host`'s Notifications switch tapped: while a request is out nothing
    /// happens; on turns it off; anything else reads the permission and
    /// turns it on.
    fn toggle_push(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        let Some((busy, on)) = self
            .root
            .read(cx)
            .push_hosts()
            .state(host)
            .map(|state| (state.busy(), state.switched_on()))
        else {
            return;
        };
        if busy {
            return;
        }
        if on {
            self.step_push(host, |state, _| state.switch_off(), cx);
            return;
        }
        let asked = self.platform.authorization();
        let host = host.clone();
        cx.spawn(async move |this, cx| {
            let auth = asked.await;
            // Fails only when the phone's root is gone.
            this.update(cx, |this, cx| {
                this.step_push(&host, |state, _| state.switch_on(auth), cx);
            })
            .ok();
        })
        .detach();
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
