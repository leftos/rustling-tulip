//! The updater on screen: the checks it runs, the footer's notice of a ready
//! update and its Restart to update button, the Settings → General update
//! rows, and the toast after an update (`docs/auto-update.md`,
//! decisions 3, 5, 6 and 8). The checking and the installing are
//! [`crate::update`]'s.

use std::sync::Arc;
use std::time::Duration;

use gpui::{AnyElement, ClickEvent, Context, Task, div, prelude::*, px};

use crate::buttons::{ButtonSize, Layer, outlined_button};
use crate::notices::ToastKind;
use crate::update::{AlreadyRunning, LastResult, Ready, Updater};
use crate::{FOOTER_PILL_HEIGHT, FOOTER_PILL_TEXT_SIZE, RootView, tagged_text};

/// How long after launch the first check runs.
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(30);
/// How often the checks repeat after the first.
const CHECK_EVERY: Duration = Duration::from_hours(24);
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
    /// The check under way; at most one runs at a time.
    checking: Option<Task<()>>,
    /// The launch's result toast, then the checks 30 s after launch and
    /// every 24 h.
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
    /// at once, then a check 30 s after launch and every 24 h.
    pub(crate) fn start_update_checks(&mut self, cx: &mut Context<Self>) {
        self.update.schedule = Some(cx.spawn(async move |this, cx| {
            if this.update(cx, Self::announce_last_update).is_err() {
                return;
            }
            let mut delay = FIRST_CHECK_AFTER;
            loop {
                cx.background_executor().timer(delay).await;
                if this.update(cx, Self::scheduled_update_check).is_err() {
                    return;
                }
                delay = CHECK_EVERY;
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

    /// The check 30 s after launch and every 24 h: skipped while automatic
    /// checks are off or an update is already ready to install.
    fn scheduled_update_check(&mut self, cx: &mut Context<Self>) {
        if self.update.ready.is_some() {
            tracing::info!("update: an update is ready; skipping the scheduled check");
        } else if self.sidebar.ui_state().general.check_updates {
            self.run_update_check(cx);
        } else {
            tracing::info!("update: automatic checks are off; skipping");
        }
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
        self.update.checking = Some(cx.spawn(async move |this, cx| {
            let found = checked.await;
            // Fails only when the view is gone, and the update with it.
            this.update(cx, |root, cx| root.on_update_checked(found, cx))
                .ok();
        }));
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

    /// The footer's notice and its Restart to update button, at the right
    /// edge, once an update is ready.
    pub(crate) fn update_footer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let text = self.update_footer_text()?;
        let button = outlined_button("footer-update", ButtonSize::Compact, true, Layer::Base)
            .min_h(px(FOOTER_PILL_HEIGHT))
            .py(px(0.0))
            .text_size(px(FOOTER_PILL_TEXT_SIZE))
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
