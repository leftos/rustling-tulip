//! Follow tabs: asking This PC's daemon for one, activating the tab it
//! names, and the tab's body, a note naming what it follows.

use std::collections::HashSet;
use std::time::Instant;

use crate::RootView;
use crate::diff_tab::PENDING_ACTIVATE_TTL;
use crate::driver_id::DriverId as _;
use crate::grid_view::muted_note;
use crate::target::HostKey;
use crate::ui_theme::theme;
use gpui::{AnyElement, Context, IntoElement as _, Window};
use protocol::{ClientMessage, DaemonMessage, FollowSource, TabContent};

/// The `OpenFollowTab`s out, and the tab one of them waits to activate.
#[derive(Debug, Default)]
pub(crate) struct FollowOpens {
    next_open: u64,
    /// The `OpenFollowTab` ids the daemon has not answered.
    sent: HashSet<String>,
    /// A tab the daemon named before its `TabUpdated` arrived, and when.
    pending: Option<(String, Instant)>,
}

impl FollowOpens {
    /// A fresh `OpenFollowTab` id, recorded as sent.
    fn open_id(&mut self) -> String {
        self.next_open += 1;
        let id = format!("follow-open-{}", self.next_open);
        self.sent.insert(id.clone());
        id
    }

    /// `FollowTabOpened`: the tab to activate now, when this client asked
    /// for it and the tab list already holds it (`known`). A tab the list
    /// does not hold yet waits for its `TabUpdated`; another client's open
    /// is ignored.
    fn opened(&mut self, id: &str, tab_id: &str, known: bool, now: Instant) -> Option<String> {
        if !self.sent.remove(id) {
            return None;
        }
        if known {
            self.pending = None;
            return Some(tab_id.to_owned());
        }
        self.pending = Some((tab_id.to_owned(), now));
        None
    }

    /// The waiting tab, once `known` says the tab list holds it; one that
    /// waited [`PENDING_ACTIVATE_TTL`] is dropped instead.
    fn take_ready(&mut self, known: impl Fn(&str) -> bool, now: Instant) -> Option<String> {
        let (tab_id, at) = self.pending.as_ref()?;
        if now.saturating_duration_since(*at) >= PENDING_ACTIVATE_TTL {
            self.pending = None;
            return None;
        }
        if !known(tab_id) {
            return None;
        }
        self.pending.take().map(|(tab_id, _)| tab_id)
    }

    /// Whether `id` is an `OpenFollowTab` of this client, which is
    /// forgotten.
    fn failed(&mut self, id: &str) -> bool {
        self.sent.remove(id)
    }

    /// A new connection: no answer to an old request will come.
    fn reset(&mut self) {
        self.sent.clear();
        self.pending = None;
    }
}

/// What a follow tab's body says it follows.
fn follow_note(source: &FollowSource) -> String {
    match source {
        FollowSource::File { path, .. } => format!("Following {path}"),
        FollowSource::Unknown => "This tab needs a newer app.".to_owned(),
    }
}

impl RootView {
    /// Asks This PC's daemon to open, or focus, a follow tab of `path` in
    /// session `session_id` of `host`. The layout stays here, so the request
    /// goes here; it names the host the session is on, whose daemon the
    /// follow reads the file through. The path is sent as given: the
    /// session's daemon resolves it.
    pub fn open_follow_tab(&mut self, host: &HostKey, session_id: &str, path: &str) {
        let id = self.follow_opens.open_id();
        self.send_to(
            &HostKey::Local,
            ClientMessage::OpenFollowTab {
                id,
                source: FollowSource::File {
                    session_id: session_id.to_owned(),
                    path: path.to_owned(),
                },
                host: host.wire().map(str::to_owned),
            },
        );
    }

    /// The note `tab_id`'s body shows, when it is a follow tab.
    #[must_use]
    pub fn follow_tab_note(&self, tab_id: &str) -> Option<String> {
        match &self.tabs.tab(tab_id)?.content {
            TabContent::Follow { source, .. } => Some(follow_note(source)),
            TabContent::Grid { .. }
            | TabContent::Diff { .. }
            | TabContent::Viewer { .. }
            | TabContent::Dashboard
            | TabContent::Unknown => None,
        }
    }

    /// A follow tab's body: the note naming what it follows, in the
    /// theme's muted text.
    pub(crate) fn follow_tab_element(
        tab_id: &str,
        source: &FollowSource,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let selector = format!("follow-tab-{tab_id}");
        muted_note(crate::fonts::ui_text_size(cx), follow_note(source), &t)
            .driver_id(move || selector)
            .into_any_element()
    }

    /// Folds a follow tab's open answer in; returns whether nothing else is
    /// owed it. A follow tab lives in This PC's layout, so only This PC's
    /// answer names one; the tab it names is activated now when it is
    /// listed, else once its `TabUpdated` lists it.
    pub(crate) fn on_follow_message(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if *host != HostKey::Local {
            return false;
        }
        match msg {
            DaemonMessage::Welcome { .. } => {
                self.follow_opens.reset();
                false
            }
            DaemonMessage::FollowTabOpened { id, tab_id } => {
                let known = self.tabs.tab(tab_id).is_some();
                if let Some(tab_id) = self.follow_opens.opened(id, tab_id, known, (self.now)()) {
                    // Re-opening the active tab focuses it again too.
                    self.last_active_tab = None;
                    self.tabs.activate(&tab_id);
                    self.after_tabs_change(window, cx);
                }
                true
            }
            DaemonMessage::Error {
                message,
                request_id: Some(id),
                ..
            } if self.follow_opens.failed(id) => {
                tracing::warn!("the daemon could not open a follow tab: {message}");
                true
            }
            _ => false,
        }
    }

    /// Activates the tab an `OpenFollowTab` named before the tab list held
    /// it, once it does.
    pub(crate) fn activate_pending_follow_tab(&mut self) {
        let now = (self.now)();
        let tabs = &self.tabs;
        let ready = self
            .follow_opens
            .take_ready(|tab_id| tabs.tab(tab_id).is_some(), now);
        if let Some(tab_id) = ready {
            self.tabs.activate(&tab_id);
        }
    }
}
