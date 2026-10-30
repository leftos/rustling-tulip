//! The Dashboard tab on the root view: the rail button's open or focus, the
//! `OpenDashboardTab` requests out and the tab one of them waits to
//! activate, and the `WatchDashboard` the client keeps in step with whether
//! the active tab is the Dashboard.

use std::collections::HashSet;
use std::time::Instant;

use gpui::{Context, Window};
use protocol::{ClientMessage, DaemonMessage, TabContent};

use crate::RootView;
use crate::connection::Connection;
use crate::dashboard::{self, GroupKind};
use crate::diff_tab::PENDING_ACTIVATE_TTL;
use crate::notices::ToastKind;
use crate::target::HostKey;

/// The toast an `OpenDashboardTab` the daemon refused raises.
const OPEN_FAILED_TITLE: &str = "Couldn't open the Dashboard";

/// The `OpenDashboardTab`s out, and the tab one of them waits to activate.
#[derive(Debug, Default)]
pub(crate) struct DashboardOpens {
    next_open: u64,
    /// The `OpenDashboardTab` ids the daemon has not answered.
    sent: HashSet<String>,
    /// A tab the daemon named before its `TabUpdated` arrived, and when.
    pending: Option<(String, Instant)>,
}

impl DashboardOpens {
    /// A fresh `OpenDashboardTab` id, recorded as sent.
    fn open_id(&mut self) -> String {
        self.next_open += 1;
        let id = format!("dashboard-open-{}", self.next_open);
        self.sent.insert(id.clone());
        id
    }

    /// `DashboardTabOpened`: the tab to activate now, when this client asked
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

    /// Whether `id` is an `OpenDashboardTab` of this client, which is
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

impl RootView {
    /// The listed Dashboard tab's id, if there is one this client has not
    /// asked to close.
    pub(crate) fn dashboard_tab_id(&self) -> Option<&str> {
        let closing = self.tabs.closing();
        self.tabs
            .tabs()
            .iter()
            .find(|tab| matches!(tab.content, TabContent::Dashboard) && !closing.contains(&tab.id))
            .map(|tab| tab.id.as_str())
    }

    /// Whether the active tab is a Dashboard tab.
    pub(crate) fn dashboard_active(&self) -> bool {
        self.tabs
            .active_id()
            .and_then(|id| self.tabs.tab(id))
            .is_some_and(|tab| matches!(tab.content, TabContent::Dashboard))
    }

    /// The rail's Dashboard button: shows the listed Dashboard tab, or asks
    /// the daemon for one when none is listed.
    pub(crate) fn open_or_focus_dashboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drag = None;
        if let Some(tab_id) = self.dashboard_tab_id().map(str::to_owned) {
            self.tabs.activate(&tab_id);
            self.after_tabs_change(window, cx);
            return;
        }
        let id = self.dashboard_opens.open_id();
        self.send_to(&HostKey::Local, ClientMessage::OpenDashboardTab { id });
        cx.notify();
    }

    /// Folds a Dashboard tab message in; returns whether nothing else is
    /// owed it.
    pub(crate) fn on_dashboard_message(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match msg {
            DaemonMessage::Welcome { .. } => {
                // A new connection holds no watch, and the facts held are the
                // old connection's. The open requests are the layout home's.
                if *host == HostKey::Local {
                    self.dashboard_opens.reset();
                }
                self.dashboard_watch.remove(host);
                self.dashboard_facts.clear(host);
                self.sync_dashboard_watch();
                false
            }
            DaemonMessage::DashboardFacts { .. } => {
                // Facts for a watch this client ended are the daemon's answer
                // to nothing: held, they would show a tab nobody is watching.
                if !self.dashboard_watch.contains(host) {
                    return true;
                }
                self.dashboard_facts.apply(host, msg);
                self.prune_dashboard_facts(host);
                cx.notify();
                true
            }
            DaemonMessage::DashboardTabOpened { id, tab_id } => {
                let known = self.tabs.tab(tab_id).is_some();
                if let Some(tab_id) = self.dashboard_opens.opened(id, tab_id, known, (self.now)()) {
                    self.tabs.activate(&tab_id);
                    self.after_tabs_change(window, cx);
                }
                true
            }
            DaemonMessage::Error {
                message,
                request_id: Some(id),
                ..
            } if self.dashboard_opens.failed(id) => {
                tracing::warn!("the daemon could not open the Dashboard tab: {message}");
                self.push_toast(
                    ToastKind::Error,
                    OPEN_FAILED_TITLE,
                    Some(message.clone()),
                    cx,
                );
                true
            }
            _ => false,
        }
    }

    /// Activates the tab an `OpenDashboardTab` named before the tab list
    /// held it, once it does.
    pub(crate) fn activate_pending_dashboard_tab(&mut self) {
        let now = (self.now)();
        let tabs = &self.tabs;
        let ready = self
            .dashboard_opens
            .take_ready(|tab_id| tabs.tab(tab_id).is_some(), now);
        if let Some(tab_id) = ready {
            self.tabs.activate(&tab_id);
        }
    }

    /// Drops the facts of every session the Dashboard no longer lists: the
    /// daemon never resends an entry that did not change, so a removed
    /// session's would stay held for good.
    fn prune_dashboard_facts(&mut self, host: &HostKey) {
        let model = dashboard::model(&self.sidebar, &self.dashboard_facts.facts());
        let listed: Vec<String> = GroupKind::ALL
            .into_iter()
            .flat_map(|kind| model.rows(kind))
            .map(|row| row.session_id.clone())
            .collect();
        self.dashboard_facts
            .prune(host, listed.iter().map(String::as_str));
    }

    /// Sends `WatchDashboard` to each host where whether the active tab is
    /// a Dashboard differs from the last one sent on that host's
    /// connection: This PC always, a remote host while it is connected.
    pub(crate) fn sync_dashboard_watch(&mut self) {
        let on = self.dashboard_active();
        for host in self.hosts.keys() {
            let reachable =
                host == HostKey::Local || self.conn_of(&host).is_some_and(Connection::is_open);
            if !reachable || on == self.dashboard_watch.contains(&host) {
                continue;
            }
            if on {
                self.dashboard_watch.insert(host.clone());
            } else {
                // The daemon drops its cache when the last watcher leaves, so
                // what is held is stale; the next watch is sent every fact.
                self.dashboard_watch.remove(&host);
                self.dashboard_facts.clear(&host);
            }
            self.send_to(&host, ClientMessage::WatchDashboard { on });
        }
    }

    /// Forgets `host`'s watch and facts, and for This PC the open requests,
    /// as the connection leaves that daemon; sends nothing.
    pub(crate) fn reset_dashboard_for_target(&mut self, host: &HostKey) {
        if *host == HostKey::Local {
            self.dashboard_opens = DashboardOpens::default();
        }
        self.dashboard_watch.remove(host);
        self.dashboard_facts.clear(host);
    }
}
