//! The updater on screen: the checks it runs, the footer's notice of a ready
//! update and its Restart to update button, the footer's client version, the
//! Settings → General update rows, and the toast after an update
//! (`docs/auto-update.md`, decisions 3, 5, 6 and 8). The checking and the
//! installing are [`crate::update`]'s.

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::Shared;
use gpui::{AnyElement, ClickEvent, Context, Task, div, prelude::*, px};

use crate::buttons::{ButtonSize, Layer, outlined_button};
use crate::notices::ToastKind;
use crate::ui_theme::theme;
use crate::update::{AlreadyRunning, LastResult, Ready, Updater};
use crate::{FOOTER_PILL_HEIGHT, FOOTER_PILL_TEXT_SIZE, RootView, tagged_text};

/// How often the checks repeat after the one at launch.
const CHECK_EVERY: Duration = Duration::from_hours(24);
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
    /// The launch's result toast, then the checks at launch and every 24 h,
    /// sooner after a failure.
    schedule: Option<Task<()>>,
}

impl UpdateUi {
    pub(crate) fn new(updater: Arc<dyn Updater>) -> Self {
        Self {
            updater,
            ready: None,
            last: LastCheck::NotChecked,
            checking: None,
            schedule: None,
        }
    }
}

impl RootView {
    /// Starts the launch's update work: the last update's result as a toast
    /// at once, then a check straight after it and every 24 h, retried
    /// after 5, 15 and then every 60 minutes while checks fail.
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
                // as no failure: the schedule returns to the daily cadence.
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

    /// The outcome the installer's `apply-update` recorded, as a toast.
    fn announce_last_update(&mut self, cx: &mut Context<Self>) {
        let Some(LastResult {
            from,
            to,
            ok,
            message,
        }) = self.update.updater.read_last_result()
        else {
            return;
        };
        if ok {
            tracing::info!(?from, %to, "update: installed");
            self.push_toast(ToastKind::Info, &format!("Updated to {to}"), None, cx);
        } else {
            tracing::warn!(?from, %to, %message, "update: did not install");
            let title = format!("Update to {to} didn't install");
            self.push_toast(ToastKind::Error, &title, Some(message), cx);
        }
    }

    /// The check at launch, every 24 h and on a retry: skipped while
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
                .whitespace_nowrap()
                .child(tagged_text("footer-version", format!("v{CLIENT_VERSION}")))
                .tooltip(crate::tooltip(tip))
                .into_any_element(),
        )
    }

    /// The footer's notice and its Restart to update button, at the right
    /// edge, once an update is ready.
    pub(crate) fn update_footer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let text = self.update_footer_text()?;
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
                .child(button)
                .into_any_element(),
        )
    }
}

/// The schedule's next step after a scheduled check: the failures in a row
/// counting this one, and the wait before the next check. A success, or a
/// skipped check, resets the count and returns to the daily cadence.
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

    use super::{CHECK_EVERY, next_check, version_tooltip};

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
    fn a_success_returns_to_the_daily_cadence() {
        assert_eq!(next_check(0, false), (0, CHECK_EVERY));
        assert_eq!(
            next_check(3, false),
            (0, CHECK_EVERY),
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
