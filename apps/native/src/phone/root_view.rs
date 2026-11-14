//! [`PhoneRoot`], the phone's root view, and its push effect plumbing.

use std::sync::Arc;

use gpui::{
    App, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, Subscription,
    Window, WindowInsets, WindowVisibility, div,
};
use protocol::ClientMessage;

use super::push::{self, Authorization, Effect, PushPlatform, PushState};
use super::sweep::{self, HostNow};
use super::{content_padding, home, hosts_view};
use crate::RootView;
use crate::target::HostKey;

/// The phone's root view: the saved hosts or the first-run form, or while
/// a saved host is open the phone's Home list at the window's width, or the
/// session a row's tap opened, and the Pair dialog over them, inside the
/// safe area. It draws only those of a phone-shape [`RootView`], whose
/// sidebar has no This PC group, so This PC's dormant slot never shows as a
/// connection.
pub struct PhoneRoot {
    pub(super) root: Entity<RootView>,
    /// The add form's fields, its refusal and whether it shows.
    pub(super) hosts: hosts_view::PhoneHosts,
    /// The phone's push: permission, device token and install keys.
    pub(super) platform: Arc<dyn PushPlatform>,
    /// The session shown in place of Home: its host and id.
    pub(super) shown: Option<(HostKey, String)>,
    /// The insets drawn around the content in place of the window's, when
    /// set: a window with none of its own (a shot) draws a phone's.
    insets_override: Option<WindowInsets>,
    /// Whether the first render has logged the app's ready line.
    logged_ready: bool,
    /// Whether the window is in front, so delivered notifications are swept.
    in_front: bool,
    /// The hosts the last sweep ran over; an unchanged state sweeps again
    /// only on a return to the foreground.
    swept: Option<Vec<HostNow>>,
    /// How many sweeps have started; a sweep whose delivered list returns
    /// after a newer one started does nothing.
    sweeps: u64,
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
            this.forget_gone_session(cx);
            this.push_changed(cx);
            this.sweep_delivered(cx);
            cx.notify();
        });
        if platform.supported() {
            Self::learn_token(&platform, cx);
        }
        let visibility = cx.observe_window_visibility(window, |this, visibility, window, cx| {
            this.in_front = visibility == WindowVisibility::Visible;
            if this.in_front {
                this.root.update(cx, |root, cx| {
                    root.returned_to_foreground();
                    root.redial_saved_hosts(window, cx);
                    for host in open_saved_hosts(root) {
                        root.send_to(&host, ClientMessage::ListSessions);
                    }
                });
                this.swept = None;
                this.sweep_delivered(cx);
            }
        });
        Self {
            hosts: hosts_view::PhoneHosts::new(&root, cx),
            root,
            platform,
            shown: None,
            insets_override: None,
            logged_ready: false,
            in_front: window.visibility() == WindowVisibility::Visible,
            swept: None,
            sweeps: 0,
            _root_changed: root_changed,
            _visibility: visibility,
        }
    }

    /// The same root drawing `insets` around its content in place of the
    /// window's, while `insets` is set.
    #[must_use]
    pub fn with_insets_override(mut self, insets: Option<WindowInsets>) -> Self {
        self.insets_override = insets;
        self
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

    /// The session shown in place of Home, as its host and id.
    #[must_use]
    pub fn shown_session(&self) -> Option<(&HostKey, &str)> {
        self.shown.as_ref().map(|(host, id)| (host, id.as_str()))
    }

    /// How many session rows Home lists now.
    #[must_use]
    pub fn home_row_count(&self, cx: &App) -> usize {
        self.home(cx).rows().count()
    }

    /// Shows `host`'s session `session_id` in place of Home. It sends
    /// nothing: the phone opens no tab and lays out no pane.
    pub(crate) fn open_session(&mut self, host: HostKey, session_id: &str, cx: &mut Context<Self>) {
        tracing::debug!(?host, session_id, "phone: showing a session");
        self.shown = Some((host, session_id.to_owned()));
        cx.notify();
    }

    /// Back from the shown session to Home.
    pub(super) fn close_session(&mut self, cx: &mut Context<Self>) {
        self.shown = None;
        cx.notify();
    }

    /// What Home lists now: every host's groups in their manual order.
    pub(super) fn home(&self, cx: &App) -> home::Home {
        home::from_sidebar(&self.root.read(cx).sidebar)
    }

    /// The shown session left its host's list: Home shows again.
    fn forget_gone_session(&mut self, cx: &mut Context<Self>) {
        let root = self.root.read(cx);
        let gone = self
            .shown
            .as_ref()
            .is_some_and(|(host, id)| root.sidebar.session_on(host, id).is_none());
        if gone {
            tracing::debug!("phone: the shown session is gone; back to Home");
            self.shown = None;
        }
    }

    /// While the window is in front and the saved hosts' Needs you sets or
    /// loaded state changed since the last sweep, removes the delivered
    /// notifications whose wait ended and sets the badge ([`sweep::sweep`]).
    /// Nothing is asked before any host has loaded.
    fn sweep_delivered(&mut self, cx: &mut Context<Self>) {
        if !self.in_front || !self.platform.supported() {
            return;
        }
        let hosts = sweep_hosts(self.root.read(cx));
        if self.swept.as_ref() == Some(&hosts) {
            return;
        }
        self.swept = Some(hosts.clone());
        if !hosts.iter().any(|host| host.loaded) {
            return;
        }
        self.sweeps += 1;
        let started = self.sweeps;
        let delivered = self.platform.delivered();
        let platform = Arc::clone(&self.platform);
        cx.spawn(async move |this, cx| {
            let delivered = delivered.await;
            // A root gone, or a newer sweep started, leaves this one's answer unused.
            let newest = this
                .read_with(cx, |this, _| this.sweeps == started)
                .unwrap_or(false);
            if !newest {
                tracing::debug!("phone: a newer sweep replaces this one");
                return;
            }
            let result = sweep::sweep(&delivered, &hosts);
            if !result.remove.is_empty() {
                tracing::debug!(
                    count = result.remove.len(),
                    "phone: removing handled notifications"
                );
                platform.remove_delivered(result.remove);
            }
            if let Some(badge) = result.badge {
                platform.set_badge(badge);
            }
        })
        .detach();
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
    pub(super) fn toggle_push(&mut self, host: &HostKey, cx: &mut Context<Self>) {
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

/// The saved hosts whose connection is open, in saved order.
fn open_saved_hosts(root: &RootView) -> Vec<HostKey> {
    root.sidebar
        .ui_state()
        .saved_hosts
        .iter()
        .filter_map(|saved| Some(HostKey::of(&saved.target().ok()?)))
        .filter(|key| hosts_view::host_open(root, key))
        .collect()
}

/// Every saved host the window can name on the wire, as the sweep sees it:
/// loaded when its connection is open and has sent its session list both
/// since its `Welcome` and since the app last came to the foreground, with
/// the ids of its sessions waiting on the user.
fn sweep_hosts(root: &RootView) -> Vec<HostNow> {
    let rows = crate::needs_you::rows(&root.sidebar);
    root.sidebar
        .ui_state()
        .saved_hosts
        .iter()
        .filter_map(|saved| {
            let key = HostKey::of(&saved.target().ok()?);
            let host = key.wire()?.to_owned();
            let loaded = hosts_view::host_open(root, &key)
                && root.listed_since_welcome(&key)
                && root.listed_since_foreground(&key);
            let needs_you = sweep::needs_you_ids(&rows, &key);
            Some(HostNow {
                host,
                loaded,
                needs_you,
            })
        })
        .collect()
}

impl Render for PhoneRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The ready line follows the first frame, not the window's opening.
        if !self.logged_ready {
            self.logged_ready = true;
            tracing::info!("phone app started");
        }
        let t = crate::ui_theme::theme(cx);
        let insets = self
            .insets_override
            .clone()
            .unwrap_or_else(|| window.insets());
        let padding = content_padding(&insets);
        let body = if let Some(page) = self.session_page(cx) {
            page
        } else if hosts_view::any_host_open(self.root.read(cx)) {
            self.list_body(cx)
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
