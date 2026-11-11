//! The updater on screen: the checks it runs, the footer's notice of a ready
//! update and its Restart to update button, the footer's client version, the
//! Settings → General update rows and their Restart to update button, and
//! the toast after an update, and the countdown toast of an update This
//! PC's daemon starts for another device
//! (`docs/auto-update.md`, decisions 3, 5, 6 and 8). The checking and the
//! installing are [`crate::update`]'s.

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::Shared;
use gpui::{AnyElement, ClickEvent, Context, Task, Window, div, prelude::*, px};

use protocol::{ClientMessage, DaemonMessage};

use crate::buttons::{ButtonSize, Layer, outlined_button, primary_button};
use crate::host_update::{
    CLIENT_UPDATE_CANCEL, ClientUpdateCountdown, HOST_UPDATE_CANCEL, HostUpdateCountdown,
    client_update_toast_key, toast_key,
};
use crate::notices::{ToastAction, ToastKind, ToastSpec};
use crate::target::HostKey;
use crate::ui_theme::theme;
use crate::update::{AlreadyRunning, LastResult, Ready, Updater};
use crate::{FOOTER_PILL_HEIGHT, FOOTER_PILL_TEXT_SIZE, RootView, tagged_text};

/// How often the checks repeat after the one at launch.
const CHECK_EVERY: Duration = Duration::from_hours(1);
/// The waits before the retries after scheduled checks that failed in a
/// row; the last one repeats until a check succeeds.
const RETRY_AFTER: [Duration; 3] = [
    Duration::from_mins(5),
    Duration::from_mins(15),
    Duration::from_hours(1),
];
/// This client's version, the one a check compares the feed against.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

const ALREADY_RUNNING_TITLE: &str = "An update is already running";
const START_FAILED_TITLE: &str = "Couldn't start the update";
const RESTART_LABEL: &str = "Restart to update";
const WHATS_NEW_LABEL: &str = "What's new";
/// The key of the toast an installed update raises at launch.
pub(crate) const INSTALLED_TOAST_KEY: &str = "update-installed";
/// The installed toast's button that opens the What's new window.
pub(crate) const WHATS_NEW_ACTION: &str = "whats-new";
const CANCEL_LABEL: &str = "Cancel";
/// How often the host update's countdown redraws its title.
const HOST_UPDATE_TICK: Duration = Duration::from_secs(1);
/// How many seconds this app counts down before it updates itself, when a
/// host asked it to.
const CLIENT_UPDATE_SECONDS: u32 = 10;
/// What the toast says when the countdown's check finds nothing newer.
const ALREADY_NEWEST_TITLE: &str = "This app is already on the newest release.";
/// What the toast says when the build cannot update itself at all.
const CANNOT_UPDATE_TITLE: &str = "This build can't update itself.";
/// What the toast says when a host's request reached no other client.
const NO_OTHER_CLIENTS_TITLE: &str = "No other clients are connected.";

/// What the last check found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LastCheck {
    NotChecked,
    UpToDate,
    Ready(semver::Version),
    Failed(String),
}

/// The updater, the update ready to install and the last check.
pub(crate) struct UpdateUi {
    updater: Arc<dyn Updater>,
    /// The verified installer the footer offers; its file stays held open
    /// until the client quits.
    ready: Option<Ready>,
    last: LastCheck,
    /// The check under way; at most one runs at a time. Shared so the
    /// schedule can wait for its outcome.
    checking: Option<Shared<Task<()>>>,
    /// The launch's result toast, then the checks at launch and every hour,
    /// sooner after a failure.
    schedule: Option<Task<()>>,
    /// The changelog range the installed toast's What's new shows: after
    /// the version replaced, when known, through the one installed.
    installed_range: Option<(Option<semver::Version>, semver::Version)>,
}

impl UpdateUi {
    pub(crate) fn new(updater: Arc<dyn Updater>) -> Self {
        Self {
            updater,
            ready: None,
            last: LastCheck::NotChecked,
            checking: None,
            schedule: None,
            installed_range: None,
        }
    }
}

/// This app's own update while a host's request drives it: the countdown on
/// screen, then the check for a release once it ends.
pub(crate) struct ClientUpdate {
    countdown: ClientUpdateCountdown,
    /// The countdown ended and the app waits on its update check: the
    /// check's answer restarts onto the release, or says the app is newest.
    awaiting_check: bool,
    /// Wakes the view when the countdown's next second is up; dropping it
    /// stops the countdown.
    timer: Option<Task<()>>,
}

impl ClientUpdate {
    fn new(countdown: ClientUpdateCountdown) -> Self {
        Self {
            countdown,
            awaiting_check: false,
            timer: None,
        }
    }
}

/// The toast that says how many clients `Update connected clients` reached.
fn told_title(native: u32, phones: u32, too_old: u32) -> String {
    if native == 0 && phones == 0 && too_old == 0 {
        return NO_OTHER_CLIENTS_TITLE.to_owned();
    }
    let clients = if native == 1 { "client" } else { "clients" };
    let mut title = format!("Told {native} {clients} to update");
    if phones > 0 {
        title = format!("{title}; {phones} phones reload");
    }
    if too_old > 0 {
        title = format!("{title}; {too_old} too old to update this way");
    }
    title
}

impl RootView {
    /// Starts the launch's update work: the last update's result as a toast
    /// at once, then a check straight after it and every hour, retried
    /// after 5, 15 and then every 60 minutes while checks fail. Every launch
    /// checks: nothing records when the last check ran.
    pub(crate) fn start_update_checks(&mut self, cx: &mut Context<Self>) {
        self.update.schedule = Some(cx.spawn(async move |this, cx| {
            if this.update(cx, Self::announce_last_update).is_err() {
                return;
            }
            let mut failed_in_a_row = 0;
            loop {
                let Ok(pending) = this.update(cx, Self::scheduled_update_check) else {
                    return;
                };
                // A skipped check (turned off, or an update ready) counts
                // as no failure: the schedule returns to the hourly cadence.
                let mut failed = false;
                if let Some(pending) = pending {
                    pending.await;
                    let Ok(outcome) = this.update(cx, |root, _| {
                        matches!(root.update.last, LastCheck::Failed(_))
                    }) else {
                        return;
                    };
                    failed = outcome;
                }
                let wait;
                (failed_in_a_row, wait) = next_check(failed_in_a_row, failed);
                cx.background_executor().timer(wait).await;
            }
        }));
    }

    /// The outcome the installer's `apply-update` recorded, as a toast; with
    /// none, a client newer than the version the last launch saved was
    /// installed by hand, and says so too. Either installed toast offers the
    /// changelog from the version replaced. Then this launch's version is
    /// saved as the last one, in a build that checks for updates: an inactive
    /// one leaves the saved version alone, so a dev build over the installed
    /// client's layout never makes it announce an update that never happened.
    fn announce_last_update(&mut self, cx: &mut Context<Self>) {
        let active = self.update.updater.is_active();
        let previous = self
            .sidebar
            .ui_state()
            .last_installed_version
            .as_deref()
            .and_then(|version| semver::Version::parse(version).ok());
        match self.update.updater.read_last_result() {
            Some(LastResult {
                from, to, ok: true, ..
            }) => {
                tracing::info!(?from, %to, "update: installed");
                let range = semver::Version::parse(&to).ok().map(|through| {
                    let after = previous.filter(|previous| *previous < through).or_else(|| {
                        from.as_deref()
                            .and_then(|from| semver::Version::parse(from).ok())
                    });
                    (after, through)
                });
                self.announce_installed(&to, range, cx);
            }
            Some(LastResult {
                from,
                to,
                ok: false,
                message,
            }) => {
                tracing::warn!(?from, %to, %message, "update: did not install");
                let title = format!("Update to {to} didn't install");
                self.push_toast(ToastKind::Error, &title, Some(message), cx);
            }
            None => {
                if active
                    && let (Some(previous), Ok(current)) =
                        (previous, semver::Version::parse(CLIENT_VERSION))
                    && previous < current
                {
                    tracing::info!(%previous, %current, "update: installed over an older version");
                    self.announce_installed(CLIENT_VERSION, Some((Some(previous), current)), cx);
                }
            }
        }
        if active {
            self.sidebar.set_last_installed_version(CLIENT_VERSION);
            self.save_ui();
        }
    }

    /// The sticky `Updated to {to}` toast, offering What's new over `range`
    /// when there is one.
    fn announce_installed(
        &mut self,
        to: &str,
        range: Option<(Option<semver::Version>, semver::Version)>,
        cx: &mut Context<Self>,
    ) {
        let actions = if range.is_some() {
            vec![ToastAction {
                id: WHATS_NEW_ACTION,
                label: WHATS_NEW_LABEL,
                danger: false,
            }]
        } else {
            Vec::new()
        };
        self.update.installed_range = range;
        self.push_toast_spec(
            ToastSpec {
                kind: ToastKind::Info,
                title: format!("Updated to {to}"),
                detail: None,
                key: Some(INSTALLED_TOAST_KEY.to_owned()),
                sticky: true,
                actions,
            },
            cx,
        );
    }

    /// The installed toast's What's new: the window opens loading the
    /// changelog of the installed range from the feed.
    pub(crate) fn whats_new_installed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((after, through)) = self.update.installed_range.clone() else {
            return;
        };
        let version = through.to_string();
        let fetch = self.update.updater.changelog(after, through);
        self.open_whats_new_fetching(version, fetch, window, cx);
    }

    /// The ready update's What's new: the window opens on the notes the
    /// check brought.
    pub fn whats_new_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ready) = &self.update.ready else {
            return;
        };
        let (version, notes) = (ready.version.to_string(), ready.notes.clone());
        self.open_whats_new(version, &notes, window, cx);
    }

    /// The check at launch, every hour and on a retry: skipped while
    /// automatic checks are off or an update is already ready to install.
    /// Returns the check under way, if any, for the schedule to wait on.
    fn scheduled_update_check(&mut self, cx: &mut Context<Self>) -> Option<Shared<Task<()>>> {
        if self.update.ready.is_some() {
            tracing::info!("update: an update is ready; skipping the scheduled check");
            return None;
        }
        if !self.sidebar.ui_state().general.check_updates {
            tracing::info!("update: automatic checks are off; skipping");
            return None;
        }
        self.run_update_check(cx);
        self.update.checking.clone()
    }

    /// The updater the checks and installs go through.
    pub(crate) fn updater(&self) -> Arc<dyn Updater> {
        Arc::clone(&self.update.updater)
    }

    /// This client's version, the one a check compares the feed against;
    /// `None` when it is not semver.
    pub(crate) fn client_version() -> Option<semver::Version> {
        semver::Version::parse(CLIENT_VERSION).ok()
    }

    /// Settings' Check now: a check whether automatic checks are on or off.
    pub fn check_for_updates_now(&mut self, cx: &mut Context<Self>) {
        self.run_update_check(cx);
    }

    /// Whether a check is under way.
    pub(crate) fn update_check_running(&self) -> bool {
        self.update.checking.is_some()
    }

    fn run_update_check(&mut self, cx: &mut Context<Self>) {
        if self.update.checking.is_some() {
            return;
        }
        let checked = match semver::Version::parse(CLIENT_VERSION) {
            Ok(current) => self.update.updater.check(&current),
            Err(err) => {
                let err =
                    anyhow::anyhow!("the client's version {CLIENT_VERSION} is not semver: {err}");
                Box::pin(futures::future::ready(Err(err)))
            }
        };
        let task = cx.spawn(async move |this, cx| {
            let found = checked.await;
            // Fails only when the view is gone, and the update with it.
            this.update(cx, |root, cx| root.on_update_checked(found, cx))
                .ok();
        });
        self.update.checking = Some(task.shared());
        cx.notify();
    }

    fn on_update_checked(&mut self, found: anyhow::Result<Option<Ready>>, cx: &mut Context<Self>) {
        self.update.checking = None;
        self.update.last = match found {
            Ok(Some(ready)) => {
                tracing::info!(version = %ready.version, "update: ready to install");
                let version = ready.version.clone();
                self.update.ready = Some(ready);
                LastCheck::Ready(version)
            }
            Ok(None) => LastCheck::UpToDate,
            Err(err) => {
                tracing::warn!("update: the check failed: {err:#}");
                LastCheck::Failed(format!("{err:#}"))
            }
        };
        self.client_update_checked(cx);
        cx.notify();
    }

    /// Restart to update: the installer starts, then the client quits as
    /// Keep sessions running does, leaving the daemon and its sessions up.
    /// A start that fails says why, and the client stays.
    fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        let Some(ready) = &self.update.ready else {
            return;
        };
        match self.update.updater.start_installer(ready) {
            Ok(()) => {
                tracing::info!(version = %ready.version, "update: restarting to install");
                self.request_quit(cx);
            }
            Err(err) if err.is::<AlreadyRunning>() => {
                tracing::warn!("update: another update is running");
                self.push_toast(ToastKind::Warning, ALREADY_RUNNING_TITLE, None, cx);
            }
            Err(err) => {
                tracing::warn!("update: the installer did not start: {err:#}");
                let detail = Some(format!("{err:#}"));
                self.push_toast(ToastKind::Error, START_FAILED_TITLE, detail, cx);
            }
        }
    }

    /// This PC's daemon counting down to an update another device asked
    /// for: its toast shows and counts down, goes when the update is
    /// cancelled, and the client quits as Keep sessions running does when
    /// the update starts. A remote host's notices are ignored.
    pub(crate) fn on_host_update_message(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        cx: &mut Context<Self>,
    ) {
        if *host != HostKey::Local {
            return;
        }
        match msg {
            DaemonMessage::HostUpdateScheduled {
                update_id,
                requested_by,
                version,
                seconds,
            } => {
                let update = HostUpdateCountdown::scheduled(
                    update_id.clone(),
                    requested_by.clone(),
                    version.clone(),
                    *seconds,
                    (self.now)(),
                );
                self.schedule_host_update(update, cx);
            }
            DaemonMessage::HostUpdateCanceled { update_id } => {
                self.host_update_canceled(update_id, cx);
            }
            DaemonMessage::HostUpdateLaunched { update_id } => {
                self.host_update_launched(update_id, cx);
            }
            _ => {}
        }
    }

    /// Shows `update`'s countdown in place of any other; an update that
    /// already ended stays ended.
    fn schedule_host_update(&mut self, update: HostUpdateCountdown, cx: &mut Context<Self>) {
        if self.host_update_ended.as_deref() == Some(update.update_id.as_str()) {
            tracing::debug!(update_id = %update.update_id, "update: a notice for an ended update ignored");
            return;
        }
        let shown = self
            .host_update
            .as_ref()
            .map(|shown| shown.update_id.clone());
        let ticking = shown.as_deref() == Some(update.update_id.as_str());
        if let Some(other) = shown.filter(|id| id != &update.update_id) {
            self.dismiss_toast_key(&toast_key(&other), cx);
        }
        tracing::info!(update_id = %update.update_id, "update: this PC's daemon counts down to an update");
        let update_id = update.update_id.clone();
        self.host_update = Some(update);
        self.show_host_update(cx);
        // One timer per update: another id's countdown replaces this one's,
        // and dropping the old task stops it.
        if !ticking || self.host_update_timer.is_none() {
            self.host_update_timer = Some(Self::tick_host_update(update_id, cx));
        }
    }

    /// The countdown's toast, its title read off the clock.
    fn show_host_update(&mut self, cx: &mut Context<Self>) {
        let Some(update) = &self.host_update else {
            return;
        };
        let spec = ToastSpec {
            kind: ToastKind::Info,
            title: update.title((self.now)()),
            detail: Some(update.body()),
            key: Some(toast_key(&update.update_id)),
            sticky: true,
            actions: vec![ToastAction {
                id: HOST_UPDATE_CANCEL,
                label: CANCEL_LABEL,
                danger: false,
            }],
        };
        self.push_toast_spec(spec, cx);
    }

    /// Redraws the countdown's title every second while update
    /// `update_id` still counts down; dropping the task stops it.
    fn tick_host_update(update_id: String, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(HOST_UPDATE_TICK).await;
                // Fails only when the view is gone, and the countdown with it.
                let counting = this
                    .update(cx, |root, cx| root.host_update_ticked(&update_id, cx))
                    .unwrap_or(false);
                if !counting {
                    break;
                }
            }
        })
    }

    /// A second went by: the toast, while it shows, takes the new title.
    /// Returns whether update `update_id` still counts down.
    fn host_update_ticked(&mut self, update_id: &str, cx: &mut Context<Self>) -> bool {
        if self
            .host_update
            .as_ref()
            .is_none_or(|update| update.update_id != update_id)
        {
            return false;
        }
        // The × closes the countdown's toast for good.
        let key = toast_key(update_id);
        let shown = self
            .notices
            .toasts()
            .iter()
            .any(|toast| toast.key.as_deref() == Some(key.as_str()));
        if shown {
            self.show_host_update(cx);
        }
        true
    }

    /// The daemon cancelled update `update_id`: its countdown goes, and a
    /// late repeat of its notice is ignored.
    fn host_update_canceled(&mut self, update_id: &str, cx: &mut Context<Self>) {
        self.host_update_ended = Some(update_id.to_owned());
        if self
            .host_update
            .as_ref()
            .is_none_or(|update| update.update_id != update_id)
        {
            return;
        }
        tracing::info!(update_id, "update: this PC's update was cancelled");
        self.host_update = None;
        self.host_update_timer = None;
        self.dismiss_toast_key(&toast_key(update_id), cx);
    }

    /// This PC's connection closed while an update counted down: its
    /// countdown and toast go at once, so a reconnect shows only what the
    /// daemon then reports. An update that ended stays ended.
    pub(crate) fn forget_host_update(&mut self, cx: &mut Context<Self>) {
        let Some(update) = self.host_update.take() else {
            return;
        };
        self.host_update_timer = None;
        self.dismiss_toast_key(&toast_key(&update.update_id), cx);
        tracing::info!(
            update_id = %update.update_id,
            "update: this PC's connection closed; dropping its update countdown"
        );
    }

    /// The daemon started update `update_id`, counted down or not: the
    /// client quits as Keep sessions running does, leaving the daemon and
    /// its sessions to the installer.
    fn host_update_launched(&mut self, update_id: &str, cx: &mut Context<Self>) {
        tracing::info!(
            update_id,
            "update: this PC's daemon started an update; quitting"
        );
        self.host_update_ended = Some(update_id.to_owned());
        self.host_update_timer = None;
        if let Some(update) = self.host_update.take() {
            self.dismiss_toast_key(&toast_key(&update.update_id), cx);
        }
        self.dismiss_toast_key(&toast_key(update_id), cx);
        self.request_quit(cx);
    }

    /// The countdown toast's Cancel: asks This PC's daemon to cancel the
    /// update; the toast stays until the daemon says it did.
    pub(crate) fn cancel_host_update(&mut self) {
        let Some(update) = &self.host_update else {
            return;
        };
        tracing::info!(update_id = %update.update_id, "update: asking this PC's daemon to cancel");
        let update_id = update.update_id.clone();
        self.send_to(
            &HostKey::Local,
            ClientMessage::CancelHostUpdate { update_id },
        );
    }

    /// This app's own update, counting down because `host` asked it to
    /// update itself: its toast counts down, its Cancel ends the countdown
    /// here, and the countdown's end restarts onto a ready update or checks
    /// for one. The `Update connected clients` answer shows as a toast too.
    pub(crate) fn on_client_update_message(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        cx: &mut Context<Self>,
    ) {
        match msg {
            DaemonMessage::UpdateYourself { update_id, .. } => {
                self.client_update_scheduled(host, update_id, cx);
            }
            DaemonMessage::ConnectedClientsNotified {
                native,
                phones,
                too_old,
                ..
            } => {
                let title = told_title(*native, *phones, *too_old);
                self.push_toast(ToastKind::Info, &title, None, cx);
            }
            _ => {}
        }
    }

    /// `host` asked this app to update itself: `update_id` counts down, in
    /// place of any countdown already running. A repeat of the id already
    /// counting down is ignored.
    pub(crate) fn client_update_scheduled(
        &mut self,
        host: &HostKey,
        update_id: &str,
        cx: &mut Context<Self>,
    ) {
        if self
            .client_update
            .as_ref()
            .is_some_and(|update| update.countdown.update_id == update_id)
        {
            tracing::debug!(
                update_id,
                "update: a repeat of this app's countdown is ignored"
            );
            return;
        }
        if let Some(old) = self.client_update.take() {
            self.dismiss_toast_key(&client_update_toast_key(&old.countdown.update_id), cx);
        }
        let asked_by = self.host_label(host);
        tracing::info!(
            update_id,
            asked_by,
            "update: a host asked this app to update"
        );
        let countdown = ClientUpdateCountdown::scheduled(
            update_id.to_owned(),
            asked_by,
            CLIENT_UPDATE_SECONDS,
            (self.now)(),
        );
        self.client_update = Some(ClientUpdate::new(countdown));
        self.show_client_update(cx);
        if let Some(update) = &mut self.client_update {
            update.timer = Some(Self::tick_client_update(update_id.to_owned(), cx));
        }
    }

    /// The countdown's toast, its title read off the clock.
    fn show_client_update(&mut self, cx: &mut Context<Self>) {
        let Some(update) = &self.client_update else {
            return;
        };
        let spec = ToastSpec {
            kind: ToastKind::Info,
            title: update.countdown.title((self.now)()),
            detail: Some(update.countdown.body()),
            key: Some(client_update_toast_key(&update.countdown.update_id)),
            sticky: true,
            actions: vec![ToastAction {
                id: CLIENT_UPDATE_CANCEL,
                label: CANCEL_LABEL,
                danger: false,
            }],
        };
        self.push_toast_spec(spec, cx);
    }

    /// Redraws the countdown's title every second until update `update_id`
    /// ends; dropping the task stops it.
    fn tick_client_update(update_id: String, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(HOST_UPDATE_TICK).await;
                // Fails only when the view is gone, and the countdown with it.
                let counting = this
                    .update(cx, |root, cx| root.client_update_ticked(&update_id, cx))
                    .unwrap_or(false);
                if !counting {
                    break;
                }
            }
        })
    }

    /// A second went by on update `update_id`: the toast, while it shows,
    /// takes the new title, and the countdown ends when nothing is left of
    /// it. Returns whether it still counts down.
    fn client_update_ticked(&mut self, update_id: &str, cx: &mut Context<Self>) -> bool {
        let left = self
            .client_update
            .as_ref()
            .filter(|update| update.countdown.update_id == update_id)
            .map(|update| update.countdown.seconds_left((self.now)()));
        let Some(left) = left else {
            return false;
        };
        if left == 0 {
            self.end_client_update(cx);
            return false;
        }
        // The × closes the countdown's toast for good.
        let key = client_update_toast_key(update_id);
        let shown = self
            .notices
            .toasts()
            .iter()
            .any(|toast| toast.key.as_deref() == Some(key.as_str()));
        if shown {
            self.show_client_update(cx);
        }
        true
    }

    /// The countdown ended: its toast goes, and this app restarts onto a
    /// ready update, or checks for one and restarts when the check answers
    /// with a release. A build that cannot update itself says so.
    fn end_client_update(&mut self, cx: &mut Context<Self>) {
        let Some(update) = &mut self.client_update else {
            return;
        };
        update.timer = None;
        update.awaiting_check = true;
        let update_id = update.countdown.update_id.clone();
        self.dismiss_toast_key(&client_update_toast_key(&update_id), cx);
        tracing::info!(update_id = %update_id, "update: this app's countdown ended");
        if !self.update.updater.is_active() {
            self.client_update = None;
            self.push_toast(ToastKind::Warning, CANNOT_UPDATE_TITLE, None, cx);
            return;
        }
        if self.update.ready.is_some() {
            self.client_update = None;
            self.restart_to_update(cx);
            return;
        }
        self.run_update_check(cx);
    }

    /// An update check answered: a countdown that ended waiting on it
    /// restarts onto the release, or says this app is already the newest.
    /// Any other check leaves the countdown alone.
    fn client_update_checked(&mut self, cx: &mut Context<Self>) {
        if !self
            .client_update
            .as_ref()
            .is_some_and(|update| update.awaiting_check)
        {
            return;
        }
        let up_to_date = self.update.last == LastCheck::UpToDate;
        self.client_update = None;
        if self.update.ready.is_some() {
            self.restart_to_update(cx);
        } else if up_to_date {
            self.push_toast(ToastKind::Info, ALREADY_NEWEST_TITLE, None, cx);
        }
    }

    /// The self-update countdown's Cancel: it ends here, and nothing is
    /// sent, since the countdown asked no one for anything.
    pub(crate) fn cancel_client_update(&mut self, cx: &mut Context<Self>) {
        let Some(update) = self.client_update.take() else {
            return;
        };
        tracing::info!(
            update_id = %update.countdown.update_id,
            "update: this app's update was cancelled"
        );
        self.dismiss_toast_key(&client_update_toast_key(&update.countdown.update_id), cx);
    }

    /// The footer's notice, `Update {version} ready`, once a check found one.
    #[must_use]
    pub fn update_footer_text(&self) -> Option<String> {
        let ready = self.update.ready.as_ref()?;
        Some(format!("Update {} ready", ready.version))
    }

    /// The Settings update rows' text: `Version {current}`, then the last
    /// check's result.
    #[must_use]
    pub fn update_settings_text(&self) -> Vec<String> {
        self.update_settings_lines().to_vec()
    }

    pub(crate) fn update_settings_lines(&self) -> [String; 2] {
        let last = match &self.update.last {
            LastCheck::NotChecked => "Not checked yet".to_owned(),
            LastCheck::UpToDate => "Up to date".to_owned(),
            LastCheck::Ready(version) => format!("Update {version} ready"),
            LastCheck::Failed(err) => format!("Last check failed: {err}"),
        };
        [format!("Version {CLIENT_VERSION}"), last]
    }

    /// The footer's right end: `v{version}` of this client, its tooltip
    /// naming This PC's daemon version too once the handshake carries one.
    /// It gives way to the update notice while that shows.
    pub(crate) fn version_footer(&self) -> Option<AnyElement> {
        if self.update.ready.is_some() {
            return None;
        }
        let daemon = self
            .handshake()
            .map(|handshake| handshake.build_version.as_str());
        let tip = version_tooltip(CLIENT_VERSION, daemon);
        Some(
            div()
                .id("footer-version")
                .flex_none()
                // One line: the version is short fixed text, in the footer.
                .whitespace_nowrap()
                .child(tagged_text("footer-version", format!("v{CLIENT_VERSION}")))
                .tooltip(crate::tooltip(tip))
                .into_any_element(),
        )
    }

    /// The footer's notice and its What's new and Restart to update buttons,
    /// at the right edge, once an update is ready.
    pub(crate) fn update_footer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let text = self.update_footer_text()?;
        let whats_new = outlined_button(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "footer-whats-new",
            ButtonSize::Compact,
            true,
            Layer::Base,
            &theme(cx),
        )
        .min_h(crate::fonts::ui_px(cx, FOOTER_PILL_HEIGHT))
        .py(px(0.0))
        .text_size(crate::fonts::ui_px(cx, FOOTER_PILL_TEXT_SIZE))
        .child(WHATS_NEW_LABEL)
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.whats_new_ready(window, cx);
        }));
        let button = outlined_button(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "footer-update",
            ButtonSize::Compact,
            true,
            Layer::Base,
            &theme(cx),
        )
        .min_h(crate::fonts::ui_px(cx, FOOTER_PILL_HEIGHT))
        .py(px(0.0))
        .text_size(crate::fonts::ui_px(cx, FOOTER_PILL_TEXT_SIZE))
        .child(RESTART_LABEL)
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.restart_to_update(cx);
        }));
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.0))
                .child(tagged_text("footer-update-text", text))
                .child(whats_new)
                .child(button)
                .into_any_element(),
        )
    }

    /// Settings' Restart to update button, beside Check now, once an update
    /// is ready; it restarts as the footer's button does.
    pub(crate) fn settings_restart_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.update.ready.as_ref()?;
        let button = primary_button(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "settings-restart-to-update",
            ButtonSize::Regular,
            true,
            Layer::Floating,
            &theme(cx),
        )
        .child(RESTART_LABEL)
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.restart_to_update(cx);
        }));
        Some(button.into_any_element())
    }

    /// Settings' What's new button, beside Restart to update, once an
    /// update is ready; it opens the window as the footer's button does.
    pub(crate) fn settings_whats_new_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.update.ready.as_ref()?;
        let button = outlined_button(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "settings-whats-new",
            ButtonSize::Regular,
            true,
            Layer::Floating,
            &theme(cx),
        )
        .child(WHATS_NEW_LABEL)
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.whats_new_ready(window, cx);
        }));
        Some(button.into_any_element())
    }
}

/// The schedule's next step after a scheduled check: the failures in a row
/// counting this one, and the wait before the next check. A success, or a
/// skipped check, resets the count and returns to the hourly cadence.
fn next_check(failed_before: usize, this_one_failed: bool) -> (usize, Duration) {
    if !this_one_failed {
        return (0, CHECK_EVERY);
    }
    let failed = failed_before.saturating_add(1);
    let step = (failed - 1).min(RETRY_AFTER.len() - 1);
    (failed, RETRY_AFTER[step])
}

/// The footer version's tooltip: the client's version, then This PC's
/// daemon's when its handshake carries one (an empty one counts as none).
fn version_tooltip(client: &str, daemon: Option<&str>) -> String {
    match daemon.filter(|version| !version.is_empty()) {
        Some(daemon) => format!("Client v{client}, daemon v{daemon} on This PC"),
        None => format!("Client v{client}"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{next_check, version_tooltip};

    #[test]
    fn after_a_failed_check_the_next_waits_5_minutes() {
        assert_eq!(next_check(0, true), (1, Duration::from_mins(5)));
    }

    #[test]
    fn the_retry_backs_off_to_an_hour() {
        assert_eq!(next_check(1, true), (2, Duration::from_mins(15)));
        assert_eq!(next_check(2, true), (3, Duration::from_hours(1)));
        assert_eq!(
            next_check(3, true),
            (4, Duration::from_hours(1)),
            "the hour repeats while checks keep failing"
        );
    }

    #[test]
    fn a_success_waits_an_hour_for_the_next_check() {
        assert_eq!(next_check(0, false), (0, Duration::from_hours(1)));
        assert_eq!(
            next_check(3, false),
            (0, Duration::from_hours(1)),
            "a success resets the backoff"
        );
    }

    #[test]
    fn version_tooltip_names_the_daemon_on_this_pc() {
        assert_eq!(
            version_tooltip("0.6.1-alpha", Some("0.6.0-alpha")),
            "Client v0.6.1-alpha, daemon v0.6.0-alpha on This PC"
        );
    }

    #[test]
    fn version_tooltip_without_a_daemon_names_the_client_only() {
        assert_eq!(version_tooltip("0.6.1-alpha", None), "Client v0.6.1-alpha");
    }

    #[test]
    fn version_tooltip_counts_an_empty_daemon_version_as_none() {
        assert_eq!(
            version_tooltip("0.6.1-alpha", Some("")),
            "Client v0.6.1-alpha"
        );
    }
}
