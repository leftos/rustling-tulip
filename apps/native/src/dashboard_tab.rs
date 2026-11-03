//! The Dashboard tab on the root view: the rail button's open or focus, the
//! `OpenDashboardTab` requests out and the tab one of them waits to
//! activate, and the `WatchDashboard` the client keeps in step with whether
//! the active tab is the Dashboard; and the drawers opened under By status
//! rows, their `GetSessionDetail`s, throttled per drawer, and the replies
//! they draw.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use gpui::{Context, Task, Window};
use protocol::{
    ClientMessage, DaemonMessage, PendingInput, PromptChoice, SessionSnapshot, SessionStatus,
    SessionSummary, TabContent,
};

use crate::RootView;
use crate::connection::Connection;
use crate::dashboard::{self, GroupKind};
use crate::dashboard_facts::HeldTracks;
use crate::dashboard_rows::{DrawerCells, DrawerDetail, diff_target, drawer_cells};
use crate::diff_tab::PENDING_ACTIVATE_TTL;
use crate::notices::ToastKind;
use crate::source_control::ScKey;
use crate::target::HostKey;

/// The toast an `OpenDashboardTab` the daemon refused raises.
const OPEN_FAILED_TITLE: &str = "Couldn't open the Dashboard";
/// The toast an `AnswerPrompt` the daemon refused raises.
const ANSWER_REFUSED_TITLE: &str = "Couldn't answer the prompt";
/// How many Dashboard answers are remembered, so a late refusal of one is
/// known as ours; an accepted answer gets no reply, so the oldest go.
const ANSWERS_CAP: usize = 16;

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

/// The `AnswerPrompt`s the Dashboard's ask blocks sent and the daemon has
/// not refused, by the host each went to, the newest [`ANSWERS_CAP`] of
/// them; and per session the wait its last answer was sent for, whose
/// buttons stay disabled while that wait holds.
#[derive(Debug, Default)]
pub(crate) struct DashboardAnswers {
    next_answer: u64,
    sent: VecDeque<(HostKey, String)>,
    out: HashMap<String, AnswerOut>,
}

/// The wait an answer was sent for: the snapshot's stamp and prompt then.
#[derive(Debug)]
struct AnswerOut {
    host: HostKey,
    request_id: String,
    since: DateTime<Utc>,
    pending: Option<PendingInput>,
}

impl DashboardAnswers {
    /// A fresh `AnswerPrompt` id for `host`, recorded as sent; past
    /// [`ANSWERS_CAP`] the oldest is forgotten.
    fn answer_id(&mut self, host: &HostKey) -> String {
        self.next_answer += 1;
        let id = format!("dashboard-answer-{}", self.next_answer);
        if self.sent.len() == ANSWERS_CAP {
            self.sent.pop_front();
        }
        self.sent.push_back((host.clone(), id.clone()));
        id
    }

    /// Whether `id` is an answer this client sent `host`, which is
    /// forgotten and its session's buttons freed.
    fn refused(&mut self, host: &HostKey, id: &str) -> bool {
        let Some(at) = self
            .sent
            .iter()
            .position(|(sent_to, sent)| sent_to == host && sent == id)
        else {
            return false;
        };
        self.sent.remove(at);
        self.out
            .retain(|_, out| !(out.host == *host && out.request_id == id));
        true
    }

    /// Whether an answer is out for `session`'s wait as its snapshot reads
    /// now: same status stamp, same prompt.
    pub(crate) fn is_out(&self, session: &SessionSnapshot) -> bool {
        self.out.get(&session.id).is_some_and(|out| {
            Some(out.since) == session.status_since && out.pending == session.pending_input
        })
    }

    /// `host`'s connection is new: no refusal of an old answer will come.
    fn reset(&mut self, host: &HostKey) {
        self.sent.retain(|(sent_to, _)| sent_to != host);
        self.out.retain(|_, out| out.host != *host);
    }
}

/// The prefix of every `GetSessionDetail` id a drawer sends, so a late
/// answer to a drawer since closed is still known as a drawer's.
const DETAIL_PREFIX: &str = "dashboard-detail-";
/// The least time between two `GetSessionDetail`s of one drawer.
const DETAIL_REFETCH: Duration = Duration::from_secs(5);

/// The drawer opened under one By status row: what it last received, the
/// request it waits on, and the change it owes a fetch.
#[derive(Default)]
struct Drawer {
    detail: Option<DrawerDetail>,
    /// The last request's refusal, from the daemon.
    error: Option<String>,
    /// The `GetSessionDetail` whose answer the drawer draws.
    waiting: Option<String>,
    last_fetch: Option<Instant>,
    /// The summary the session's snapshot carried at the last fetch, so a
    /// snapshot that changes it is known.
    summary: Option<SessionSummary>,
    /// A change came inside [`DETAIL_REFETCH`] of the last fetch: the timer
    /// that fetches once the interval is up.
    due: Option<Task<()>>,
}

/// The open drawers, by the host and session each was opened for.
#[derive(Default)]
pub(crate) struct DashboardDrawers {
    next_request: u64,
    open: HashMap<(HostKey, String), Drawer>,
}

impl DashboardDrawers {
    fn request_id(&mut self) -> String {
        self.next_request += 1;
        format!("{DETAIL_PREFIX}{}", self.next_request)
    }

    /// The open drawer on `host` that waits on `request_id`.
    fn waiting_on(&mut self, host: &HostKey, request_id: &str) -> Option<&mut Drawer> {
        self.open
            .iter_mut()
            .find(|((on, _), drawer)| on == host && drawer.waiting.as_deref() == Some(request_id))
            .map(|(_, drawer)| drawer)
    }

    /// `host`'s connection is new: no answer to an old request will come.
    fn reset(&mut self, host: &HostKey) {
        for ((on, _), drawer) in &mut self.open {
            if on == host {
                drawer.waiting = None;
            }
        }
    }
}

impl std::fmt::Debug for DashboardDrawers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DashboardDrawers")
            .field("next_request", &self.next_request)
            .field("open", &self.open.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Whether `session` has ended, a session its host no longer lists
/// included: its drawer keeps what it last showed.
fn ended(session: Option<&SessionSnapshot>) -> bool {
    session.is_none_or(|session| {
        matches!(
            session.status,
            SessionStatus::Stopped | SessionStatus::Error
        )
    })
}

/// Whether a reply's `request_id` is a drawer's `GetSessionDetail`'s.
fn is_drawer_request(request_id: Option<&str>) -> bool {
    request_id.is_some_and(|id| id.starts_with(DETAIL_PREFIX))
}

impl RootView {
    /// The drawer key of `session_id`: its host while one lists it, else
    /// the host of a drawer open for it, so a vanished session's drawer is
    /// still found.
    fn drawer_key(&self, session_id: &str) -> (HostKey, String) {
        let host = self
            .sidebar
            .find_session(session_id)
            .map(|(host, _)| host)
            .or_else(|| {
                self.dashboard_drawers
                    .open
                    .keys()
                    .find(|(_, id)| id == session_id)
                    .map(|(host, _)| host.clone())
            })
            .unwrap_or(HostKey::Local);
        (host, session_id.to_owned())
    }

    /// Asks again for the detail of every drawer open on `host` that has
    /// none yet: the new connection will not answer the old request.
    fn refetch_empty_drawers(&mut self, host: &HostKey) {
        let empty: Vec<(HostKey, String)> = self
            .dashboard_drawers
            .open
            .iter()
            .filter(|((on, _), drawer)| on == host && drawer.detail.is_none())
            .map(|(key, _)| key.clone())
            .collect();
        for key in &empty {
            self.fetch_drawer(key);
        }
    }

    /// A By status row's click: opens the drawer under `session_id`'s row
    /// and asks for its detail, or closes the open one.
    pub(crate) fn toggle_dashboard_drawer(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let key = self.drawer_key(session_id);
        if self.dashboard_drawers.open.remove(&key).is_none() {
            let summary = self
                .sidebar
                .session(session_id)
                .and_then(|session| session.summary.clone());
            self.dashboard_drawers.open.insert(
                key.clone(),
                Drawer {
                    summary,
                    ..Drawer::default()
                },
            );
            self.fetch_drawer(&key);
        }
        cx.notify();
    }

    /// What the drawer under `session_id`'s row draws; `None` while it is
    /// closed.
    #[must_use]
    pub fn dashboard_drawer(&self, session_id: &str) -> Option<DrawerCells> {
        let key = self.drawer_key(session_id);
        let drawer = self.dashboard_drawers.open.get(&key)?;
        Some(drawer_cells(
            session_id,
            drawer.detail.as_ref(),
            drawer.error.as_deref(),
            ended(self.sidebar.session(session_id)),
            Utc::now(),
        ))
    }

    /// The drawer's `Open diff`: the first changed file's diff, in the
    /// tree of the member it lies in.
    pub(crate) fn open_drawer_diff(&mut self, session_id: &str) {
        let key = self.drawer_key(session_id);
        let Some(path) = self
            .dashboard_drawers
            .open
            .get(&key)
            .and_then(|drawer| drawer.detail.as_ref())
            .and_then(|detail| detail.work.files.first())
            .map(|file| file.path.clone())
        else {
            return;
        };
        let Some(session) = self.sidebar.session(session_id) else {
            return;
        };
        let Some((member, in_member)) = diff_target(&session.members, &path) else {
            tracing::warn!(session_id, path, "no member of the session holds the file");
            return;
        };
        let target = ScKey::for_member(member, self.sidebar.repos_of(&key.0));
        self.open_diff(&target, &in_member, None);
    }

    /// Sends the drawer at `key` a `GetSessionDetail` now.
    fn fetch_drawer(&mut self, key: &(HostKey, String)) {
        let request_id = self.dashboard_drawers.request_id();
        let now = (self.now)();
        let Some(drawer) = self.dashboard_drawers.open.get_mut(key) else {
            return;
        };
        drawer.waiting = Some(request_id.clone());
        drawer.last_fetch = Some(now);
        drawer.due = None;
        self.send_to(
            &key.0,
            ClientMessage::GetSessionDetail {
                session_id: key.1.clone(),
                request_id: Some(request_id),
            },
        );
    }

    /// `session_id`'s facts or summary changed on `host`: its open drawer
    /// fetches again, at once when [`DETAIL_REFETCH`] has passed since its
    /// last fetch, else once it has. An ended session's drawer keeps what
    /// it shows.
    fn drawer_session_changed(&mut self, host: &HostKey, session_id: &str, cx: &mut Context<Self>) {
        if ended(self.sidebar.session(session_id)) {
            return;
        }
        let key = (host.clone(), session_id.to_owned());
        let now = (self.now)();
        let Some(drawer) = self.dashboard_drawers.open.get_mut(&key) else {
            return;
        };
        if drawer.due.is_some() {
            return;
        }
        let since = drawer
            .last_fetch
            .map_or(DETAIL_REFETCH, |at| now.saturating_duration_since(at));
        if since >= DETAIL_REFETCH {
            self.fetch_drawer(&key);
            return;
        }
        let delay = DETAIL_REFETCH.saturating_sub(since);
        drawer.due = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            // Fails only when the view is gone, and the drawer with it.
            this.update(cx, |this, _| this.fetch_drawer(&key)).ok();
        }));
    }

    /// A snapshot of `session` arrived from `host`: a summary other than
    /// the one its drawer last fetched for is a change.
    fn drawer_snapshot(
        &mut self,
        host: &HostKey,
        session: &SessionSnapshot,
        cx: &mut Context<Self>,
    ) {
        let key = (host.clone(), session.id.clone());
        let Some(drawer) = self.dashboard_drawers.open.get_mut(&key) else {
            return;
        };
        if drawer.summary == session.summary {
            return;
        }
        drawer.summary.clone_from(&session.summary);
        self.drawer_session_changed(host, &session.id, cx);
    }

    /// Folds a drawer's `SessionDetail` or refusal in; a reply to a drawer
    /// since closed, or to a request since replaced, is dropped. Returns
    /// whether the message was a drawer's.
    fn on_drawer_reply(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        cx: &mut Context<Self>,
    ) -> bool {
        match msg {
            DaemonMessage::SessionDetail {
                session_id,
                lines,
                prompt,
                work,
                request_id: Some(id),
            } if is_drawer_request(Some(id)) => {
                let Some(drawer) = self.dashboard_drawers.waiting_on(host, id) else {
                    tracing::debug!(session_id, id, "dropping a detail no open drawer waits on");
                    return true;
                };
                drawer.waiting = None;
                drawer.error = None;
                drawer.detail = Some(DrawerDetail {
                    lines: lines.clone(),
                    prompt: prompt.clone(),
                    work: work.clone(),
                });
                cx.notify();
                true
            }
            DaemonMessage::Error {
                message,
                request_id: Some(id),
                ..
            } if is_drawer_request(Some(id)) => {
                let Some(drawer) = self.dashboard_drawers.waiting_on(host, id) else {
                    tracing::debug!(id, message, "dropping a refusal no open drawer waits on");
                    return true;
                };
                tracing::warn!(id, message, "the daemon refused a drawer's detail");
                drawer.waiting = None;
                drawer.error = Some(message.clone());
                cx.notify();
                true
            }
            _ => false,
        }
    }

    /// An ask block's choice button: sends `choice` for `session_id`'s
    /// prompt, chosen against the snapshot's status stamp `since`, to the
    /// session's host, and holds the session's buttons until that wait
    /// changes or the answer is refused. Nothing is sent while an answer
    /// for the same wait is out.
    pub(crate) fn answer_from_dashboard(
        &mut self,
        session_id: &str,
        since: DateTime<Utc>,
        choice: PromptChoice,
        cx: &mut Context<Self>,
    ) {
        let pending = match self.sidebar.session(session_id) {
            Some(session) if self.dashboard_answers.is_out(session) => return,
            Some(session) => session.pending_input.clone(),
            None => None,
        };
        let host = self.host_of_session(session_id);
        let request_id = self.dashboard_answers.answer_id(&host);
        self.dashboard_answers.out.insert(
            session_id.to_owned(),
            AnswerOut {
                host: host.clone(),
                request_id: request_id.clone(),
                since,
                pending,
            },
        );
        self.send_to(
            &host,
            ClientMessage::AnswerPrompt {
                session_id: session_id.to_owned(),
                since,
                choice,
                request_id: Some(request_id),
            },
        );
        self.refresh_app_toasts(cx);
        cx.notify();
    }

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
        if self.on_drawer_reply(host, msg, cx) {
            return true;
        }
        match msg {
            DaemonMessage::SessionUpdated { session, .. } => {
                self.drawer_snapshot(host, session, cx);
                false
            }
            DaemonMessage::Sessions { sessions } => {
                for session in sessions {
                    self.drawer_snapshot(host, session, cx);
                }
                false
            }
            DaemonMessage::Welcome { .. } => {
                // A new connection holds no watch, and the facts held are the
                // old connection's. The open requests are the layout home's.
                if *host == HostKey::Local {
                    self.dashboard_opens.reset();
                }
                self.dashboard_answers.reset(host);
                self.dashboard_drawers.reset(host);
                self.refetch_empty_drawers(host);
                self.dashboard_watch.remove(host);
                self.dashboard_facts.clear(host);
                self.dashboard_tracks.clear(host);
                self.sync_dashboard_watch();
                false
            }
            DaemonMessage::DashboardFacts { facts, .. } => {
                // Facts for a watch this client ended are the daemon's answer
                // to nothing: held, they would show a tab nobody is watching.
                if !self.dashboard_watch.contains(host) {
                    return true;
                }
                self.dashboard_facts.apply(host, msg);
                self.prune_dashboard_facts(host);
                // The daemon sends only the facts that changed.
                for changed in facts {
                    self.drawer_session_changed(host, &changed.session_id, cx);
                }
                cx.notify();
                true
            }
            DaemonMessage::ActivityTracks {
                now,
                hours,
                segments,
            } => {
                // As with the facts: a watch this client ended is owed nothing.
                if !self.dashboard_watch.contains(host) {
                    return true;
                }
                self.dashboard_tracks.apply(
                    host,
                    HeldTracks {
                        now: *now,
                        hours: *hours,
                        segments: segments.clone(),
                    },
                );
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
            DaemonMessage::Error {
                message,
                request_id: Some(id),
                ..
            } if self.dashboard_answers.refused(host, id) => {
                tracing::warn!("the daemon refused a Dashboard answer: {message}");
                self.refresh_app_toasts(cx);
                self.push_toast(
                    ToastKind::Error,
                    ANSWER_REFUSED_TITLE,
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
                self.dashboard_tracks.clear(&host);
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
        self.dashboard_answers.reset(host);
        self.dashboard_drawers.reset(host);
        self.dashboard_watch.remove(host);
        self.dashboard_facts.clear(host);
        self.dashboard_tracks.clear(host);
    }
}

#[cfg(test)]
mod tests {
    use super::{ANSWERS_CAP, DashboardAnswers, is_drawer_request};
    use crate::target::HostKey;

    #[test]
    fn only_a_drawers_own_request_ids_are_claimed() {
        assert!(is_drawer_request(Some("dashboard-detail-3")));
        assert!(!is_drawer_request(Some("someone-else-1")));
        assert!(!is_drawer_request(None));
    }

    #[test]
    fn the_answers_remembered_are_capped_dropping_the_oldest() {
        let mut answers = DashboardAnswers::default();
        let ids: Vec<String> = (0..=ANSWERS_CAP)
            .map(|_| answers.answer_id(&HostKey::Local))
            .collect();
        assert!(
            !answers.refused(&HostKey::Local, &ids[0]),
            "the oldest is forgotten once {ANSWERS_CAP} newer ones were sent"
        );
        assert!(answers.refused(&HostKey::Local, &ids[1]));
        assert!(answers.refused(&HostKey::Local, &ids[ANSWERS_CAP]));
    }
}
