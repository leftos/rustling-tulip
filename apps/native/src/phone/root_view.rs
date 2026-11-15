//! [`PhoneRoot`], the phone's root view, and its push effect plumbing.

use std::sync::Arc;

use gpui::{
    App, ClickEvent, Context, Div, Entity, FontWeight, IntoElement, Pixels, Render, Stateful,
    Subscription, Window, WindowInsets, WindowVisibility, div, prelude::*, px,
};
use protocol::ClientMessage;

use super::home::Offer;
use super::home_view::{STATUS_TEXT_SIZE, TOUCH_TARGET};
use super::hosts_view::phone_button;
use super::push::{self, Authorization, Effect, PushPlatform, PushState};
use super::session::SessionPage;
use super::sheet::{
    CANCEL_LABEL, NO_RESUME_TEXT, OFFLINE_TEXT, PhoneReply, Sheet, SheetStep, SheetTap,
};
use super::sweep::{self, HostNow};
use super::{content_padding, home, hosts_view};
use crate::RootView;
use crate::buttons::ButtonKind;
use crate::driver_id::DriverId as _;
use crate::target::HostKey;
use crate::ui_theme::theme;

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
    /// The page the shown session is on, while a session is shown.
    pub(super) page: Option<SessionPage>,
    /// Home's Needs you tray: the card it shows and the swipe in progress.
    pub(super) tray: super::tray::Tray,
    /// The sheet a long press on a Home row opened, while it is open.
    sheet: Option<Sheet>,
    /// The sheet's resumes out, each until the reply its request id gets.
    /// The phone's own record: the desktop's `Duplicates` never sees them.
    resumes: Vec<home::PendingResume>,
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
            this.take_replies(cx);
            this.sync_sheet(cx);
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
            page: None,
            tray: super::tray::Tray::default(),
            sheet: None,
            resumes: Vec::new(),
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

    /// Draws `insets` around the content in place of the window's from now
    /// on, while `insets` is set.
    pub fn set_insets_override(&mut self, insets: Option<WindowInsets>, cx: &mut Context<Self>) {
        self.insets_override = insets;
        cx.notify();
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

    /// Shows `host`'s session `session_id` in place of Home, on its chat
    /// when the chat is mirrored, else on its terminal. It sends nothing:
    /// the phone opens no tab and lays out no pane.
    pub(crate) fn open_session(&mut self, host: HostKey, session_id: &str, cx: &mut Context<Self>) {
        tracing::debug!(?host, session_id, "phone: showing a session");
        self.page = Some(SessionPage::opened(
            self.chat_mirrored(&host, session_id, cx),
        ));
        self.shown = Some((host, session_id.to_owned()));
        self.sheet = None;
        cx.notify();
    }

    /// Opens the sheet of `host`'s session `session_id` over Home; nothing
    /// while a session page shows. It sends nothing.
    pub(super) fn open_sheet(&mut self, host: HostKey, session_id: &str, cx: &mut Context<Self>) {
        if self.shown.is_some() {
            return;
        }
        let Some(offer) = self.offer_of(&host, session_id, cx) else {
            return;
        };
        tracing::debug!(?host, session_id, "phone: opening a session's sheet");
        let mut sheet = Sheet::new(host, session_id, offer);
        // A resume already out for the session keeps its row inert.
        if self
            .resumes
            .iter()
            .any(|pending| sheet.is_for(&pending.host, &pending.original))
        {
            sheet.resume_sent();
        }
        self.sheet = Some(sheet);
        cx.notify();
    }

    /// What `host`'s session `session_id`'s sheet offers now: nothing while
    /// its host is closed; `None` once the host no longer lists it.
    fn offer_of(&self, host: &HostKey, session_id: &str, cx: &App) -> Option<Offer> {
        let root = self.root.read(cx);
        let session = root.sidebar.session_on(host, session_id)?;
        Some(home::offer_on(session, hosts_view::host_open(root, host)))
    }

    /// Opens the sheet of the session Home lists as `session_id`, as a long
    /// press on its row does; returns whether Home lists it.
    pub fn open_sheet_on(&mut self, session_id: &str, cx: &mut Context<Self>) -> bool {
        let host = self
            .home(cx)
            .rows()
            .find(|row| row.session_id == session_id)
            .map(|row| row.host.clone());
        let Some(host) = host else {
            return false;
        };
        self.open_sheet(host, session_id, cx);
        true
    }

    /// The id of the session whose sheet is open.
    #[must_use]
    pub fn sheet_session(&self) -> Option<&str> {
        self.sheet.as_ref().map(|sheet| sheet.session_id.as_str())
    }

    /// What the open sheet's Resume row reads, while it has one.
    #[must_use]
    pub fn sheet_resume_label(&self) -> Option<&'static str> {
        self.sheet.as_ref().map(Sheet::resume_label)
    }

    /// Why the daemon refused the open sheet's last resume.
    #[must_use]
    pub fn sheet_refusal(&self) -> Option<&str> {
        self.sheet.as_ref().and_then(Sheet::refusal)
    }

    /// Closes the sheet; a resume out still gets its follow-up.
    pub(super) fn close_sheet(&mut self, cx: &mut Context<Self>) {
        self.sheet = None;
        cx.notify();
    }

    /// A tap in the sheet: Stop's second tap sends a bare stop and closes
    /// it; Resume sends the resume and keeps it open for the reply.
    pub(super) fn sheet_tap(&mut self, tap: SheetTap, cx: &mut Context<Self>) {
        let Some(sheet) = self.sheet.as_mut() else {
            return;
        };
        match sheet.tap(tap) {
            SheetStep::Nothing => {}
            SheetStep::SendStop => {
                let (host, session_id) = (sheet.host.clone(), sheet.session_id.clone());
                self.sheet = None;
                self.send(&host, home::stop(&session_id), cx);
            }
            SheetStep::SendResume => self.send_resume(cx),
        }
        cx.notify();
    }

    /// Sends the open sheet's session its resume, keeping the record its
    /// reply is matched against; a `ResumeAbandoned`, which no reply names,
    /// closes the sheet.
    fn send_resume(&mut self, cx: &mut Context<Self>) {
        let Some(sheet) = self.sheet.as_mut() else {
            return;
        };
        // One resume per session at a time, whatever sheet sent it.
        if self
            .resumes
            .iter()
            .any(|pending| sheet.is_for(&pending.host, &pending.original))
        {
            sheet.resume_sent();
            return;
        }
        let root = self.root.read(cx);
        if !hosts_view::host_open(root, &sheet.host) {
            return;
        }
        let Some(session) = root.sidebar.session_on(&sheet.host, &sheet.session_id) else {
            return;
        };
        let resume_session = root
            .conn_of(&sheet.host)
            .is_some_and(|conn| conn.supports(protocol::FEATURE_RESUME_SESSION));
        let (msg, pending) = home::resume(
            session,
            &sheet.host,
            crate::new_request_id(),
            resume_session,
        );
        let host = sheet.host.clone();
        let Some(pending) = pending else {
            self.sheet = None;
            self.send(&host, msg, cx);
            return;
        };
        sheet.resume_sent();
        self.root.update(cx, |root, _| {
            root.phone_replies.register(&host, &pending.request_id);
            root.send_to(&host, msg);
        });
        self.resumes.push(pending);
    }

    /// Takes the replies to the sheet's requests: a new session's discards
    /// the original it reran and closes that session's sheet; a refusal
    /// shows in the sheet, or on nothing once it is closed. A resume whose
    /// host closed is dropped with its id, as is one whose id its host
    /// forgot (it reconnected).
    fn take_replies(&mut self, cx: &mut Context<Self>) {
        if self.resumes.is_empty() {
            return;
        }
        let replies = self.root.update(cx, |root, _| root.phone_replies.drain());
        for (request_id, reply) in replies {
            self.on_reply(&request_id, reply, cx);
        }
        let closed: Vec<HostKey> = {
            let root = self.root.read(cx);
            self.resumes
                .iter()
                .map(|pending| &pending.host)
                .filter(|host| !hosts_view::host_open(root, host))
                .cloned()
                .collect()
        };
        if !closed.is_empty() {
            tracing::debug!(?closed, "phone: dropping the resumes out to closed hosts");
            self.root.update(cx, |root, _| {
                for host in &closed {
                    root.phone_replies.forget_host(host);
                }
            });
        }
        let root = self.root.read(cx);
        self.resumes
            .retain(|pending| root.phone_replies.is_out(&pending.request_id));
        if let Some(sheet) = self.sheet.as_mut()
            && sheet.resuming()
            && !self
                .resumes
                .iter()
                .any(|pending| sheet.is_for(&pending.host, &pending.original))
        {
            sheet.resume_dropped();
        }
    }

    /// `reply` came for the resume out under `request_id`.
    fn on_reply(&mut self, request_id: &str, reply: PhoneReply, cx: &mut Context<Self>) {
        let Some(at) = self
            .resumes
            .iter()
            .position(|pending| pending.request_id == request_id)
        else {
            return;
        };
        let pending = self.resumes.remove(at);
        let sheet = self
            .sheet
            .as_mut()
            .filter(|sheet| sheet.is_for(&pending.host, &pending.original));
        match reply {
            PhoneReply::Created => {
                if sheet.is_some() {
                    self.sheet = None;
                }
                if let Some(discard) = pending.follow_up() {
                    self.send(&pending.host, discard, cx);
                }
            }
            PhoneReply::Refused(message) => {
                if let Some(sheet) = sheet {
                    sheet.refused(message);
                } else {
                    tracing::debug!(
                        session_id = %pending.original,
                        %message,
                        "phone: a refused resume whose sheet is closed"
                    );
                }
            }
        }
    }

    /// The sheet's session left Home's list: the sheet closes. Else the
    /// sheet takes what the session offers now, so an armed Stop goes when
    /// the session stops on its own or its host closes.
    fn sync_sheet(&mut self, cx: &mut Context<Self>) {
        let Some(sheet) = self.sheet.as_ref() else {
            return;
        };
        let listed = self
            .home(cx)
            .label_of(&sheet.host, &sheet.session_id)
            .is_some();
        let offer = self.offer_of(&sheet.host, &sheet.session_id, cx);
        let Some(offer) = offer.filter(|_| listed) else {
            tracing::debug!("phone: the sheet's session is gone; closing it");
            self.sheet = None;
            return;
        };
        if let Some(sheet) = self.sheet.as_mut() {
            sheet.see(offer);
        }
    }

    /// Back from the shown session to Home.
    pub(super) fn close_session(&mut self, cx: &mut Context<Self>) {
        self.shown = None;
        self.page = None;
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
            self.page = None;
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

/// One line of the sheet's `text`, tagged `id`, in `color`.
fn sheet_line(id: &'static str, text: String, color: u32, cx: &App) -> Div {
    div()
        .driver_id(move || id.to_owned())
        .px(px(4.0))
        .text_size(crate::fonts::ui_px(cx, STATUS_TEXT_SIZE))
        .text_color(gpui::rgb(color))
        .child(text)
}

impl PhoneRoot {
    /// The open sheet over a scrim: the session's label, the rows its
    /// state offers, a refusal, then Cancel, its bottom clear of `bottom`'s
    /// home indicator. `None` while no sheet is open or its session is gone.
    fn sheet_layer(&self, bottom: Pixels, cx: &mut Context<Self>) -> Option<Div> {
        let sheet = self.sheet.as_ref()?;
        let offer = self.offer_of(&sheet.host, &sheet.session_id, cx)?;
        let title = self
            .home(cx)
            .label_of(&sheet.host, &sheet.session_id)
            .map_or_else(|| sheet.session_id.clone(), str::to_owned);
        let t = theme(cx);
        let note = match offer {
            Offer::Nothing => Some(("phone-sheet-note", NO_RESUME_TEXT)),
            Offer::Offline => Some(("phone-sheet-offline", OFFLINE_TEXT)),
            Offer::Stop | Offer::Resume => None,
        }
        .map(|(id, text)| sheet_line(id, text.to_owned(), t.muted, cx));
        let refusal = sheet
            .refusal()
            .map(|text| sheet_line("phone-sheet-error", text.to_owned(), t.danger_text, cx));
        let scrim = div()
            .id("phone-sheet-scrim")
            .driver_id(|| "phone-sheet-scrim".to_owned())
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(gpui::rgba(t.scrim))
            .occlude()
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.close_sheet(cx)));
        let panel = div()
            .id("phone-sheet")
            .driver_id(|| "phone-sheet".to_owned())
            .occlude()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .px(px(16.0))
            .pt(px(16.0))
            .pb(bottom + px(16.0))
            .rounded_t(px(14.0))
            .border_t_1()
            .border_color(gpui::rgb(t.line))
            .bg(gpui::rgb(t.raised))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.sheet_tap(SheetTap::Elsewhere, cx);
            }))
            .child(
                div()
                    .driver_id(|| "phone-sheet-title".to_owned())
                    .flex()
                    .items_center()
                    .min_h(px(TOUCH_TARGET))
                    .px(px(4.0))
                    .truncate()
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(title),
            )
            .children(Self::sheet_action(sheet, offer, cx))
            .children(note)
            .children(refusal)
            .child(
                Self::sheet_button("phone-sheet-cancel", ButtonKind::Outlined, cx)
                    .child(CANCEL_LABEL)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.close_sheet(cx);
                    })),
            );
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

    /// The row `offer` gives: Stop in the danger colour, or Resume.
    fn sheet_action(sheet: &Sheet, offer: Offer, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let (id, kind, label, tap) = match offer {
            Offer::Stop => (
                "phone-sheet-stop",
                ButtonKind::Danger,
                sheet.stop_label(),
                SheetTap::Stop,
            ),
            Offer::Resume => (
                "phone-sheet-resume",
                ButtonKind::Outlined,
                sheet.resume_label(),
                SheetTap::Resume,
            ),
            Offer::Nothing | Offer::Offline => return None,
        };
        Some(
            Self::sheet_button(id, kind, cx)
                .child(label)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.sheet_tap(tap, cx);
                })),
        )
    }

    /// A full-width sheet row of `kind`, at least a touch target tall.
    fn sheet_button(id: &str, kind: ButtonKind, cx: &Context<Self>) -> Stateful<Div> {
        phone_button(id, kind, cx).w_full().min_h(px(TOUCH_TARGET))
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
        // The sheet is Home's: no session page, no hosts screen under it.
        let over_home = self.shown.is_none() && hosts_view::any_host_open(self.root.read(cx));
        let sheet = over_home
            .then(|| self.sheet_layer(padding.bottom, cx))
            .flatten();
        let padded = || {
            div()
                .pt(padding.top)
                .pr(padding.right)
                .pb(padding.bottom)
                .pl(padding.left)
        };
        // The whole window takes a session page's back gesture, so a swipe
        // that starts in the safe area's bands goes back too.
        let session_shown = self.shown.is_some();
        div()
            .relative()
            .size_full()
            .when(session_shown, |root| {
                root.on_back_gesture(cx.listener(Self::session_back_gesture))
            })
            .bg(gpui::rgb(t.ground))
            .text_color(gpui::rgb(t.text))
            .child(padded().size_full().child(body))
            .children(sheet)
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
