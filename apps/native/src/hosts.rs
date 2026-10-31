//! The window's connections, one per host, and the routing that picks which
//! host a message goes to.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque, hash_map};
use std::rc::Rc;

use futures::channel::mpsc::UnboundedSender;
use gpui::{Context, Window};
use protocol::{
    AgentCommandEntry, AgentId, ClientMessage, DaemonMessage, HostRebind, OptionValues,
    PresetTarget, SessionSnapshot, SpawnTarget, SuggestTarget,
};

use crate::RootView;
use crate::connection::Connection;
use crate::connection_picker::SavedHost;
use crate::net::{HandshakeInfo, NetCommand};
use crate::remote_access::RemoteStatusView;
use crate::remote_mode::Capabilities;
use crate::settings_view::{KeepAwake, SettingApply};
use crate::tabs::{PaneBinding, SessionRef};
use crate::target::{HostKey, Target};

/// One host's daemon settings as its daemon last reported them: each is
/// empty until that daemon reports it, and goes with its connection.
#[derive(Default)]
pub(crate) struct HostMirrors {
    /// The daemon's keep-awake state.
    pub(crate) keep_awake: Option<KeepAwake>,
    /// The daemon's remote access status.
    pub(crate) remote_status: Option<RemoteStatusView>,
    /// The daemon's launch commands, by agent.
    pub(crate) agent_commands: Option<HashMap<AgentId, AgentCommandEntry>>,
    /// Each agent's Settings page values.
    pub(crate) agent_settings: BTreeMap<AgentId, OptionValues>,
    /// Whether the daemon has reported its agents' Settings values.
    pub(crate) agent_settings_received: bool,
    /// Each agent's Settings values as changed but not yet echoed back.
    pub(crate) agent_settings_pending: BTreeMap<AgentId, OptionValues>,
    /// The Settings value changes sent and still waiting on an answer.
    pub(crate) agent_setting_applies: VecDeque<SettingApply>,
    /// The daemon's worktrees root, and whether it is an override.
    pub(crate) worktrees_root: Option<(String, bool)>,
}

/// One host's connection: where its commands go, its state machine as its
/// network thread last reported it, its daemon's handshake and its daemon's
/// settings.
pub(crate) struct HostConn {
    tx: UnboundedSender<NetCommand>,
    conn: Connection,
    handshake: Option<HandshakeInfo>,
    /// The id the daemon named itself by in its last `Welcome` on this
    /// connection, if it named one.
    daemon_id: Option<String>,
    /// Which of the window's connections this is: every connection the
    /// window opens gets a new one, so an event its network thread sends
    /// after a newer connection replaced it is told apart.
    generation: u64,
    pub(crate) mirrors: HostMirrors,
}

impl HostConn {
    /// A connection on its way to `target`, its commands going to `tx`.
    pub(crate) fn new(tx: UnboundedSender<NetCommand>, target: Target) -> Self {
        Self {
            tx,
            conn: Connection::for_target(target),
            handshake: None,
            daemon_id: None,
            generation: 0,
            mirrors: HostMirrors::default(),
        }
    }

    /// Which of the window's connections this is.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

/// What a host's terminals send through: one per host, handed to every
/// terminal showing one of its sessions, forwarding to the connection the
/// window holds to that host now. While the window holds none (the host
/// switched off, forgotten or dropped as a duplicate) what is sent is
/// dropped, and no terminal keeps a dropped connection's network thread
/// alive.
#[derive(Clone)]
pub struct HostSender(Rc<RefCell<Option<UnboundedSender<NetCommand>>>>);

impl HostSender {
    fn new(tx: Option<UnboundedSender<NetCommand>>) -> Self {
        Self(Rc::new(RefCell::new(tx)))
    }

    /// Forwards to `tx` from now on; to nothing for `None`.
    fn point_at(&self, tx: Option<UnboundedSender<NetCommand>>) {
        *self.0.borrow_mut() = tx;
    }

    /// Hands `command` to the host's current connection; with none it is
    /// logged and dropped.
    pub(crate) fn send(&self, command: NetCommand) {
        let Some(tx) = self.0.borrow().clone() else {
            tracing::debug!(
                "the terminal's host has no connection; dropped {}",
                command_name(&command)
            );
            return;
        };
        // Fails only once the network thread has exited, which it does only
        // after the window let its connection go.
        let _ = tx.unbounded_send(command);
    }
}

/// The window's connections: This PC's, always present because it is the
/// layout home, and one per remote host by its host key, in the order
/// they joined.
pub(crate) struct Hosts {
    local: HostConn,
    remote: Vec<(String, HostConn)>,
    /// The generation the next connection added gets.
    next_generation: u64,
    /// Each host's terminal sender, made on first ask and kept across its
    /// connections.
    senders: HashMap<HostKey, HostSender>,
}

impl Hosts {
    /// The connections of a window that holds only This PC's, `local`,
    /// which is the first generation.
    pub(crate) fn new(mut local: HostConn) -> Self {
        local.generation = 0;
        Self {
            local,
            remote: Vec::new(),
            next_generation: 1,
            senders: HashMap::new(),
        }
    }

    /// The sender `key`'s terminals send through, forwarding to the
    /// connection the window holds to it, if any.
    pub(crate) fn sender(&mut self, key: &HostKey) -> HostSender {
        if let Some(sender) = self.senders.get(key) {
            return sender.clone();
        }
        let sender = HostSender::new(self.get(key).map(|conn| conn.tx.clone()));
        self.senders.insert(key.clone(), sender.clone());
        sender
    }

    /// Whether `generation` is that of the connection the window holds to
    /// `key`; never for a host it holds none to.
    pub(crate) fn is_current(&self, key: &HostKey, generation: u64) -> bool {
        self.get(key)
            .is_some_and(|conn| conn.generation == generation)
    }

    /// This PC's connection.
    pub(crate) fn local(&self) -> &HostConn {
        &self.local
    }

    /// This PC's connection, to update.
    pub(crate) fn local_mut(&mut self) -> &mut HostConn {
        &mut self.local
    }

    /// `key`'s connection; `None` when the window has none to it.
    pub(crate) fn get(&self, key: &HostKey) -> Option<&HostConn> {
        match key {
            HostKey::Local => Some(&self.local),
            HostKey::Remote(name) => self
                .remote
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, conn)| conn),
        }
    }

    /// `key`'s connection, to update; `None` when the window has none to it.
    pub(crate) fn get_mut(&mut self, key: &HostKey) -> Option<&mut HostConn> {
        match key {
            HostKey::Local => Some(&mut self.local),
            HostKey::Remote(name) => self
                .remote
                .iter_mut()
                .find(|(key, _)| key == name)
                .map(|(_, conn)| conn),
        }
    }

    /// Every host the window holds a connection to: This PC first, then
    /// the remote hosts in the order they joined.
    pub(crate) fn keys(&self) -> Vec<HostKey> {
        std::iter::once(HostKey::Local)
            .chain(
                self.remote
                    .iter()
                    .map(|(name, _)| HostKey::Remote(name.clone())),
            )
            .collect()
    }

    /// Adds remote host `name`'s connection after the others as a new
    /// generation and returns it; `None`, adding nothing, when the window
    /// already holds one to it.
    pub(crate) fn insert(&mut self, name: String, mut conn: HostConn) -> Option<u64> {
        if self.remote.iter().any(|(key, _)| *key == name) {
            return None;
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        conn.generation = generation;
        if let Some(sender) = self.senders.get(&HostKey::Remote(name.clone())) {
            sender.point_at(Some(conn.tx.clone()));
        }
        self.remote.push((name, conn));
        Some(generation)
    }

    /// Takes remote host `key`'s connection out of the window; `None` when
    /// the window holds none to it, and always for This PC, whose
    /// connection the window never gives up.
    pub(crate) fn remove(&mut self, key: &HostKey) -> Option<HostConn> {
        let HostKey::Remote(name) = key else {
            return None;
        };
        let index = self.remote.iter().position(|(held, _)| held == name)?;
        if let Some(sender) = self.senders.get(key) {
            sender.point_at(None);
        }
        Some(self.remote.remove(index).1)
    }
}

impl RootView {
    /// This PC's connection, always to its own daemon.
    pub(crate) fn conn(&self) -> &Connection {
        &self.hosts.local().conn
    }

    /// A remote host's session list: its count is recorded in
    /// `native-ui.json` while the host is saved, so its Connections row
    /// reads "N sessions last time" once the host is switched off.
    pub(crate) fn record_host_session_count(&mut self, host: &HostKey, msg: &DaemonMessage) {
        let (HostKey::Remote(key), DaemonMessage::Sessions { sessions }) = (host, msg) else {
            return;
        };
        let Some(target) = self.conn_of(host).map(|conn| conn.target().clone()) else {
            return;
        };
        let saved = self
            .sidebar
            .ui_state()
            .saved_hosts
            .iter()
            .any(|saved| saved.is_for(&target));
        let count = u32::try_from(sessions.len()).unwrap_or(u32::MAX);
        if saved && self.sidebar.set_host_session_count(key, count) {
            self.save_ui();
        }
    }

    /// Every host the window holds a connection to: This PC first, then
    /// the remote hosts in the order they joined.
    #[must_use]
    pub fn held_hosts(&self) -> Vec<HostKey> {
        self.hosts.keys()
    }

    /// The host whose connection reached the same daemon before `host`'s
    /// did, when `host`'s was dropped as its duplicate.
    #[must_use]
    pub fn duplicate_of(&self, host: &HostKey) -> Option<&HostKey> {
        self.duplicate_of.get(host)
    }

    /// Drops the window's connection to remote host `host`: its network
    /// thread quits once its sender is gone, and what went with the
    /// connection is cleaned up as on a close. Its sidebar slice stays.
    /// Returns whether the window held one.
    pub(crate) fn disconnect_host(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let held = self.hosts.remove(host).is_some();
        if held {
            tracing::info!(?host, "disconnected from the host");
            self.on_host_closed(host, window, cx);
        }
        held
    }

    /// `host`'s connection closed or the window let it go: its agents,
    /// daemon settings, daemon id, fetches, the Fetch file… dialog fetching
    /// from it and remote access status go with it, a remote host's spawns and duplicates in flight end with their
    /// spawning toasts, and each host dropped as its duplicate is dialled
    /// again. Once
    /// the window holds no connection to it, a Pair armed for it is
    /// forgotten and its pairing dialog closes.
    pub(crate) fn on_host_closed(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.lists_since_welcome.remove(host);
        // This PC's next tab list is its next connection's: until then its
        // layout is the old one's.
        if *host == HostKey::Local {
            self.pane_forgets.tabs_in = false;
        }
        self.forget_agents(host);
        self.forget_daemon_settings(host);
        self.forget_daemon_id(host);
        self.end_fetches_on_disconnect(host, window, cx);
        self.close_fetch_dialog_of(host, window, cx);
        self.set_remote_status(host, None, cx);
        // A remote host's spawns and duplicates in flight end with its
        // connection; This PC's end with its reset.
        if *host != HostKey::Local {
            self.drop_pending_of(host, cx);
        }
        if self.hosts.get(host).is_none() {
            self.forget_device_pairing(host, window, cx);
        }
        self.reconnect_duplicates_of(host, window, cx);
    }

    /// `owner` no longer holds its daemon: every host dropped as its
    /// duplicate is freed, and each still saved and switched on is dialled
    /// again, taking the daemon if it says `Welcome` first.
    fn reconnect_duplicates_of(
        &mut self,
        owner: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let freed: Vec<HostKey> = self
            .duplicate_of
            .iter()
            .filter(|(_, held_by)| *held_by == owner)
            .map(|(duplicate, _)| duplicate.clone())
            .collect();
        for duplicate in freed {
            self.duplicate_of.remove(&duplicate);
            if let Some(target) = self.enabled_saved_target(&duplicate) {
                tracing::info!(
                    ?duplicate,
                    ?owner,
                    "the daemon's first connection is gone; dialling its duplicate again"
                );
                self.connect_host(target, window, cx);
            }
        }
    }

    /// The target of saved host `host` while its switch is on.
    fn enabled_saved_target(&self, host: &HostKey) -> Option<Target> {
        self.sidebar
            .ui_state()
            .saved_hosts
            .iter()
            .filter(|saved| saved.enabled)
            .filter_map(|saved| saved.target().ok())
            .find(|target| HostKey::of(target) == *host)
    }

    /// `host`'s connection closed: the daemon id it last named no longer
    /// holds that daemon for it.
    pub(crate) fn forget_daemon_id(&mut self, host: &HostKey) {
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.daemon_id = None;
        }
    }

    /// Records the daemon id `host`'s `Welcome` named and, when another
    /// held host already named it, drops the duplicate connection with its
    /// slice and records which host it duplicates. The duplicate is `host`,
    /// unless `host` is This PC, which always owns its own daemon. Returns
    /// whether `host` itself was dropped.
    pub(crate) fn drop_if_duplicate(
        &mut self,
        host: &HostKey,
        daemon_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.daemon_id = daemon_id.map(str::to_owned);
        }
        let Some(id) = daemon_id else {
            return false;
        };
        let holder = self.hosts.keys().into_iter().find(|other| {
            other != host
                && self
                    .hosts
                    .get(other)
                    .is_some_and(|entry| entry.daemon_id.as_deref() == Some(id))
        });
        let Some(holder) = holder else {
            return false;
        };
        let (duplicate, owner) = if *host == HostKey::Local {
            (holder, HostKey::Local)
        } else {
            (host.clone(), holder)
        };
        tracing::info!(
            ?duplicate,
            ?owner,
            "two connections reach one daemon; dropping the later one"
        );
        self.disconnect_host(&duplicate, window, cx);
        self.clear_remote_slice(&duplicate, window, cx);
        let dropped_self = duplicate == *host;
        self.duplicate_of.insert(duplicate, owner);
        dropped_self
    }

    /// `host`'s connection; `None` when the window has none to it.
    pub(crate) fn conn_of(&self, host: &HostKey) -> Option<&Connection> {
        self.hosts.get(host).map(|entry| &entry.conn)
    }

    /// Whether `host`'s sidebar rows draw dimmed: a remote host's while
    /// its connection is not open, or the window holds none.
    #[must_use]
    pub fn host_dimmed(&self, host: &HostKey) -> bool {
        content_dimmed(host, self.conn_of(host))
    }

    /// Whether the sidebar container keyed `key` draws dimmed, by its host.
    #[must_use]
    pub fn container_dimmed(&self, key: &str) -> bool {
        self.host_dimmed(&self.host_of_container(key))
    }

    /// Whether session `id`'s sidebar row draws dimmed, by its host.
    #[must_use]
    pub fn session_dimmed(&self, id: &str) -> bool {
        self.host_dimmed(&self.host_of_session(id))
    }

    /// The saved hosts whose switch is on, by host key, in saved order.
    fn enabled_saved_hosts(&self) -> Vec<HostKey> {
        self.sidebar
            .ui_state()
            .saved_hosts
            .iter()
            .filter(|host| host.enabled)
            .filter_map(|host| host.target().ok())
            .map(|target| HostKey::of(&target))
            .collect()
    }

    /// Whether `host` is one of the saved hosts, switched on or off.
    fn is_saved_host(&self, host: &HostKey) -> bool {
        self.sidebar
            .ui_state()
            .saved_hosts
            .iter()
            .filter_map(|saved| saved.target().ok())
            .any(|target| HostKey::of(&target) == *host)
    }

    /// The footer's `{n}/{m} hosts online` text; `None` with no enabled
    /// saved host.
    pub(crate) fn hosts_online(&self) -> Option<String> {
        let enabled: Vec<Option<&Connection>> = self
            .enabled_saved_hosts()
            .iter()
            .map(|host| self.conn_of(host))
            .collect();
        crate::footer::hosts_online(self.conn(), &enabled)
    }

    /// What the daemon on `host` can do for this client, by the target the
    /// window's connection to it holds; a remote host the window holds no
    /// connection to still has a remote host's.
    pub(crate) fn capabilities_of(&self, host: &HostKey) -> Capabilities {
        match host {
            HostKey::Local => Capabilities::for_target(self.conn().target()),
            HostKey::Remote(_) => self
                .conn_of(host)
                .map_or_else(Capabilities::remote, |conn| {
                    Capabilities::for_target(conn.target())
                }),
        }
    }

    /// The hosts a spawn can go to now: This PC, then each remote host
    /// whose connection is open (so online and approved) and that is not
    /// another host's duplicate.
    pub(crate) fn connected_hosts(&self) -> Vec<HostKey> {
        self.held_hosts()
            .into_iter()
            .filter(|host| {
                *host == HostKey::Local
                    || (!self.is_remote_offline(host) && self.duplicate_of(host).is_none())
            })
            .collect()
    }

    /// The host of the active tab's focused pane; This PC with no tab or
    /// no focused pane.
    pub(crate) fn focused_host(&self) -> HostKey {
        self.tabs.active_tab().map_or(HostKey::Local, |tab| {
            self.tabs
                .focused_pane(&tab.id)
                .map_or(HostKey::Local, |pane| self.tabs.pane_host(&tab.id, &pane))
        })
    }

    /// `host`'s short name, as its connection's target gives it, else as
    /// the sidebar labels the host.
    pub(crate) fn host_name(&self, host: &HostKey) -> String {
        self.conn_of(host)
            .and_then(|conn| conn.target().short_name())
            .map_or_else(|| crate::sidebar::host_label(host), str::to_owned)
    }

    /// What the host of the focused pane's session lets this client do;
    /// This PC's with no pane focused.
    pub(crate) fn focused_capabilities(&self) -> Capabilities {
        let host = self
            .focused_session()
            .map_or(HostKey::Local, |id| self.host_of_session(&id));
        self.capabilities_of(&host)
    }

    /// This PC's daemon settings, which the Settings modal shows and edits.
    pub(crate) fn mirrors(&self) -> &HostMirrors {
        &self.hosts.local().mirrors
    }

    /// This PC's daemon settings, to update.
    pub(crate) fn mirrors_mut(&mut self) -> &mut HostMirrors {
        &mut self.hosts.local_mut().mirrors
    }

    /// `host`'s daemon settings, to update; `None` when the window has no
    /// connection to it.
    pub(crate) fn mirrors_of_mut(&mut self, host: &HostKey) -> Option<&mut HostMirrors> {
        self.hosts.get_mut(host).map(|entry| &mut entry.mirrors)
    }

    /// Forgets `host`'s daemon's keep-awake state and launch commands, as
    /// a disconnect does.
    pub(crate) fn forget_daemon_settings(&mut self, host: &HostKey) {
        if let Some(mirrors) = self.mirrors_of_mut(host) {
            mirrors.keep_awake = None;
            mirrors.agent_commands = None;
        }
    }

    /// Opens a connection to `target`'s host beside the ones the window
    /// holds and handles its events from then on. This PC's connection is
    /// the window's first, so a `target` on This PC, or a host the window
    /// already holds, opens nothing.
    pub fn connect_host(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        let HostKey::Remote(name) = HostKey::of(&target) else {
            tracing::warn!(
                "connect_host: This PC is the window's first connection; opened nothing"
            );
            return;
        };
        if self.hosts.get(&HostKey::Remote(name.clone())).is_some() {
            tracing::info!("connect_host: the window already holds host {name}");
            return;
        }
        let (tx, events) = (self.open_host)(&target);
        self.duplicate_of.remove(&HostKey::Remote(name.clone()));
        if let Some(generation) = self.hosts.insert(name.clone(), HostConn::new(tx, target)) {
            Self::forward_net_events(HostKey::Remote(name), generation, events, window, cx);
        }
    }

    /// Whether `generation` is that of the connection the window holds to
    /// `host`: an event from any other was sent by a connection since
    /// replaced or let go.
    pub(crate) fn is_current_connection(&self, host: &HostKey, generation: u64) -> bool {
        self.hosts.is_current(host, generation)
    }

    /// Opens a connection to each enabled saved host, in saved order, beside
    /// This PC's.
    pub(crate) fn connect_saved_hosts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let saved: Vec<SavedHost> = self
            .sidebar
            .ui_state()
            .saved_hosts
            .iter()
            .filter(|host| host.enabled)
            .cloned()
            .collect();
        for host in saved {
            match host.target() {
                Ok(target) => self.connect_host(target, window, cx),
                Err(err) => {
                    tracing::warn!(host = %host.name, "the saved host is not valid ({err}); not connected");
                }
            }
        }
    }

    /// The handshake of the daemon the window shows, once it reported one.
    pub(crate) fn handshake(&self) -> Option<&HandshakeInfo> {
        self.hosts.local().handshake.as_ref()
    }

    /// `host`'s network thread reported its connection's state.
    pub(crate) fn set_conn(&mut self, host: &HostKey, conn: Connection) {
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.conn = conn;
        }
    }

    /// `host`'s network thread reported its daemon's handshake.
    pub(crate) fn set_handshake(&mut self, host: &HostKey, info: HandshakeInfo) {
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.handshake = Some(info);
        }
    }

    /// Drops `host`'s handshake: its daemon is going or another one answers.
    pub(crate) fn clear_handshake(&mut self, host: &HostKey) {
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.handshake = None;
        }
    }

    /// Sends `command` to `host`'s network thread; with no connection to
    /// `host` it is logged and dropped.
    pub(crate) fn command_to(&self, host: &HostKey, command: NetCommand) {
        let Some(entry) = self.hosts.get(host) else {
            tracing::warn!(
                "no connection for host {host:?}; dropped {}",
                command_name(&command)
            );
            return;
        };
        // Fails only once the network thread has exited, which it does only
        // after this view drops its sender.
        let _ = entry.tx.unbounded_send(command);
    }

    /// Sends `msg` to `host`'s daemon; with no connection to `host` it is
    /// logged and dropped.
    pub(crate) fn send_to(&self, host: &HostKey, msg: ClientMessage) {
        self.command_to(host, NetCommand::Send(Box::new(msg)));
    }

    /// The host that lists session `id`; This PC for a session no host
    /// lists.
    pub(crate) fn host_of_session(&self, id: &str) -> HostKey {
        self.sidebar
            .find_session(id)
            .map_or(HostKey::Local, |(host, _)| host)
    }

    /// The host that registered repo `id`; This PC for a repo no host
    /// lists.
    pub(crate) fn host_of_repo(&self, id: &str) -> HostKey {
        self.sidebar
            .find_repo(id)
            .map_or(HostKey::Local, |(host, _)| host)
    }

    /// The host that registered workspace `id`; This PC for a workspace no
    /// host lists.
    pub(crate) fn host_of_workspace(&self, id: &str) -> HostKey {
        self.sidebar
            .find_workspace(id)
            .map_or(HostKey::Local, |(host, _)| host)
    }

    /// The host that owns container `id`: a repo or workspace id, or a
    /// sidebar container key. A key carrying the prefix of a remote host
    /// the window holds or has saved (a switched-off host keeps its rows)
    /// names that host, so its DIR and SH containers resolve too; a
    /// `repo:` or `ws:` key resolves by the id it holds. This PC for
    /// anything else.
    pub(crate) fn host_of_container(&self, id: &str) -> HostKey {
        if let Some((name, _)) = id.split_once("::") {
            let host = HostKey::Remote(name.to_owned());
            if self.hosts.get(&host).is_some() || self.is_saved_host(&host) {
                return host;
            }
        }
        let id = id
            .strip_prefix("repo:")
            .or_else(|| id.strip_prefix("ws:"))
            .unwrap_or(id);
        match self.sidebar.find_repo(id) {
            Some((host, _)) => host,
            None => self.host_of_workspace(id),
        }
    }
}

impl RootView {
    /// The host that owns the repo or workspace a spawn `target` names;
    /// This PC for a standalone spawn.
    pub(crate) fn host_of_target(&self, target: &SpawnTarget) -> HostKey {
        match target {
            SpawnTarget::Single { repo_id, .. } => self.host_of_repo(repo_id),
            SpawnTarget::Workspace { workspace_id, .. } => self.host_of_workspace(workspace_id),
            SpawnTarget::Standalone { .. } => HostKey::Local,
        }
    }

    /// The host that owns the repo or workspace a preset `target` names.
    pub(crate) fn host_of_preset_target(&self, target: &PresetTarget) -> HostKey {
        match target {
            PresetTarget::Repo { repo_id } => self.host_of_repo(repo_id),
            PresetTarget::Workspace { workspace_id } => self.host_of_workspace(workspace_id),
        }
    }

    /// The host that owns the repo or workspace a branch-name suggestion's
    /// `target` names.
    pub(crate) fn host_of_suggest_target(&self, target: &SuggestTarget) -> HostKey {
        match target {
            SuggestTarget::Repo { repo_id, .. } => self.host_of_repo(repo_id),
            SuggestTarget::Workspace { workspace_id, .. } => self.host_of_workspace(workspace_id),
            SuggestTarget::Unknown => HostKey::Local,
        }
    }

    /// The host a spawn, preset or repo-read request built away from its
    /// call site goes to: the owner of the repo, workspace or spawn target
    /// it names, else This PC.
    pub(crate) fn host_of_request(&self, msg: &ClientMessage) -> HostKey {
        match msg {
            ClientMessage::SpawnSession(request) => self.host_of_target(&request.target),
            ClientMessage::PreviewSpawn { repo_id, .. }
            | ClientMessage::ListBranches { repo_id, .. }
            | ClientMessage::ListWorktrees { repo_id, .. }
            | ClientMessage::FetchRepo { repo_id, .. }
            | ClientMessage::SetRepoWorktreeDefault { repo_id, .. }
            | ClientMessage::ListCommits { repo_id, .. }
            | ClientMessage::GetCommit { repo_id, .. }
            | ClientMessage::GetRemoteUrl { repo_id, .. } => self.host_of_repo(repo_id),
            ClientMessage::PreviewWorkspaceSpawn { workspace_id, .. }
            | ClientMessage::SetWorkspaceWorktreeDefault { workspace_id, .. } => {
                self.host_of_workspace(workspace_id)
            }
            ClientMessage::ListPresets { target, .. }
            | ClientMessage::LaunchPreset { target, .. }
            | ClientMessage::PreviewPreset { target, .. }
            | ClientMessage::ResolvePresetScripts { target, .. } => {
                self.host_of_preset_target(target)
            }
            ClientMessage::SuggestBranchName { target, .. } => self.host_of_suggest_target(target),
            _ => HostKey::Local,
        }
    }
}

/// The forgets and rebinds of remote hosts' panes sent to This PC on its
/// current connection, and the host forgets waiting for it to come back.
#[derive(Default)]
pub(crate) struct PaneForgets {
    /// The sessions a forget of their panes went out for, kept while a pane
    /// still shows one.
    sessions: HashSet<SessionRef>,
    /// The rebinds that went out, each a session and the id of its
    /// successor, with the request that carried it, kept while a pane still
    /// shows that session and until This PC refuses the request.
    rebinds: HashMap<(SessionRef, String), String>,
    /// The hosts a forget of every pane went out for, kept while a pane
    /// still names one.
    hosts: HashSet<HostKey>,
    /// The hosts forgotten while This PC was offline, in order, sent after
    /// its next tab list.
    queued: Vec<HostKey>,
    /// Whether This PC's tab list has arrived on its current connection;
    /// cleared when that connection closes and on its next `Welcome`.
    tabs_in: bool,
    /// Whether the current connection's sweep of unsaved hosts has run.
    swept: bool,
}

impl PaneForgets {
    /// This PC's new connection: what went out on the old one may be lost,
    /// so nothing counts as in flight; the queued host forgets stay.
    pub(crate) fn reset_connection(&mut self) {
        self.sessions.clear();
        self.rebinds.clear();
        self.hosts.clear();
        self.tabs_in = false;
        self.swept = false;
    }

    /// This PC refused `request_id`: its rebinds no longer count as in
    /// flight, so the next prune asks again.
    pub(crate) fn rebind_refused(&mut self, request_id: &str) {
        self.rebinds.retain(|_, sent_as| sent_as != request_id);
    }
}

impl RootView {
    /// This PC's tab list arrived: the host forgets queued while it was
    /// offline go out, then, once per connection, each host a pane names
    /// that is no longer saved has every pane forgotten.
    pub(crate) fn after_this_pc_tabs(&mut self) {
        self.pane_forgets.tabs_in = true;
        for host in std::mem::take(&mut self.pane_forgets.queued) {
            self.send_host_forget(host);
        }
        if std::mem::replace(&mut self.pane_forgets.swept, true) {
            return;
        }
        let mut stale: Vec<HostKey> = Vec::new();
        for binding in self.tabs.bindings() {
            let host = binding.host;
            if host != HostKey::Local
                && !self.is_saved_host(&host)
                && !self.pane_forgets.hosts.contains(&host)
                && !stale.contains(&host)
            {
                stale.push(host);
            }
        }
        for host in stale {
            self.send_host_forget(host);
        }
    }

    /// Forget in Connections on remote host `host`: every pane of it on
    /// This PC is forgotten, now, or after This PC's next tab list while
    /// This PC is offline.
    pub(crate) fn forget_host_panes(&mut self, host: HostKey) {
        if self.conn().is_open() {
            self.send_host_forget(host);
        } else if !self.pane_forgets.queued.contains(&host) {
            self.pane_forgets.queued.push(host);
        }
    }

    /// Asks This PC to forget every pane of `host`, which then counts as in
    /// flight.
    fn send_host_forget(&mut self, host: HostKey) {
        let Some(wire) = host.wire().map(str::to_owned) else {
            return;
        };
        tracing::info!(?host, "forgetting every pane of the host on This PC");
        self.send_to(
            &HostKey::Local,
            ClientMessage::ForgetHostSessions {
                host: wire,
                session_ids: Vec::new(),
                all: true,
                request_id: None,
            },
        );
        self.pane_forgets.hosts.insert(host);
    }

    /// Asks This PC to show each pane's successor in place of a resumed
    /// session, then to forget each pane whose remote session its host's
    /// fresh list no longer names and has no successor for, one message of
    /// each per host, ids sorted; a session or host whose forget is in
    /// flight, or a rebind in flight, is not asked again.
    /// Nothing is pruned until This PC is open with its tab list in, and
    /// until that list is in on its current connection no forget in flight
    /// is let go against the old layout.
    pub(crate) fn prune_remote_panes(&mut self) {
        if !self.pane_forgets.tabs_in {
            return;
        }
        let bindings = self.tabs.bindings();
        self.pane_forgets
            .sessions
            .retain(|session| bindings.iter().any(|binding| binding.shows(session)));
        self.pane_forgets
            .rebinds
            .retain(|(session, _), _| bindings.iter().any(|binding| binding.shows(session)));
        self.pane_forgets
            .hosts
            .retain(|host| bindings.iter().any(|binding| binding.host == *host));
        if !self.conn().is_open() {
            return;
        }
        for host in self.held_hosts() {
            if !self.prunable(&host) {
                continue;
            }
            self.send_rebinds(&host, &bindings);
            let missing = self.missing_sessions(&host, &bindings);
            let Some(wire) = host
                .wire()
                .map(str::to_owned)
                .filter(|_| !missing.is_empty())
            else {
                continue;
            };
            tracing::info!(
                ?host,
                ?missing,
                "forgetting the panes of sessions the host no longer lists"
            );
            for id in &missing {
                self.pane_forgets
                    .sessions
                    .insert(SessionRef::new(host.clone(), id.clone()));
            }
            self.send_to(
                &HostKey::Local,
                ClientMessage::ForgetHostSessions {
                    host: wire,
                    session_ids: missing,
                    all: false,
                    request_id: None,
                },
            );
        }
    }

    /// Asks This PC to show the successor of each session of `host` a pane
    /// in `bindings` shows, old ids sorted, unless that rebind is in flight.
    fn send_rebinds(&mut self, host: &HostKey, bindings: &[PaneBinding]) {
        let Some(wire) = host.wire().map(str::to_owned) else {
            return;
        };
        let mut shown: Vec<String> = bindings
            .iter()
            .filter(|binding| binding.host == *host)
            .filter_map(|binding| binding.session_id.clone())
            .collect();
        shown.sort();
        shown.dedup();
        let request_id = crate::new_request_id();
        let mut rebinds = Vec::new();
        for old_id in shown {
            let Some(new_id) = self.successor_of(host, &old_id).map(|s| s.id.clone()) else {
                continue;
            };
            let pair = (
                SessionRef::new(host.clone(), old_id.clone()),
                new_id.clone(),
            );
            if let hash_map::Entry::Vacant(slot) = self.pane_forgets.rebinds.entry(pair) {
                slot.insert(request_id.clone());
                rebinds.push(HostRebind { old_id, new_id });
            }
        }
        if rebinds.is_empty() {
            return;
        }
        tracing::info!(
            ?host,
            ?rebinds,
            "showing resumed sessions' successors in their panes"
        );
        self.send_to(
            &HostKey::Local,
            ClientMessage::RebindHostSessions {
                host: wire,
                rebinds,
                request_id: Some(request_id),
            },
        );
    }

    /// The session of `host` that replaced `id`, the newest start and then
    /// the larger id among several.
    pub(crate) fn successor_of(&self, host: &HostKey, id: &str) -> Option<&SessionSnapshot> {
        self.sidebar
            .sessions_of(host)
            .iter()
            .filter(|session| session.resumed_from.iter().any(|old| old == id))
            .max_by(|a, b| (a.started_at, &a.id).cmp(&(b.started_at, &b.id)))
    }

    /// Whether a forget of `session`'s panes, or of every pane of its host,
    /// is on its way to This PC.
    pub(crate) fn forget_in_flight(&self, session: &SessionRef) -> bool {
        self.pane_forgets.sessions.contains(session)
            || self.pane_forgets.hosts.contains(&session.host)
    }

    /// Whether `host`'s panes may be pruned: a remote host held, open, and
    /// listed since its latest `Welcome`, with no forget of all its panes
    /// in flight.
    fn prunable(&self, host: &HostKey) -> bool {
        *host != HostKey::Local
            && self.conn_of(host).is_some_and(Connection::is_open)
            && self.listed_since_welcome(host)
            && !self.pane_forgets.hosts.contains(host)
    }

    /// The ids, sorted, of `host`'s sessions a pane in `bindings` shows
    /// that the host's list does not name and names no successor of, and
    /// whose forget is not in flight.
    fn missing_sessions(&self, host: &HostKey, bindings: &[PaneBinding]) -> Vec<String> {
        let listed = self.sidebar.sessions_of(host);
        let mut missing: Vec<String> = bindings
            .iter()
            .filter(|binding| binding.host == *host)
            .filter_map(|binding| binding.session_id.clone())
            .filter(|id| !listed.iter().any(|session| session.id == *id))
            .filter(|id| self.successor_of(host, id).is_none())
            .filter(|id| {
                !self
                    .pane_forgets
                    .sessions
                    .contains(&SessionRef::new(host.clone(), id.clone()))
            })
            .collect();
        missing.sort();
        missing.dedup();
        missing
    }
}

/// The command's name for a log line: a message's wire type, else the
/// command's own.
fn command_name(command: &NetCommand) -> String {
    let text = match command {
        NetCommand::Send(msg) => format!("{msg:?}"),
        other => format!("{other:?}"),
    };
    text.split([' ', '(', '{'])
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Whether `host`'s rows draw dimmed, `conn` being the window's connection
/// to it: a remote host's do while that connection is not open, or when the
/// window holds none (switched off, or dropped as a duplicate). This PC's
/// never do.
pub(crate) fn content_dimmed(host: &HostKey, conn: Option<&Connection>) -> bool {
    *host != HostKey::Local && !conn.is_some_and(Connection::is_open)
}

#[cfg(test)]
mod tests {
    use futures::channel::mpsc::unbounded;

    use super::{HostConn, Hosts, content_dimmed};
    use crate::connection::Connection;
    use crate::target::{HostKey, Target};

    const BOX: &str = "box.tail1234.ts.net";

    fn conn(target: Target) -> HostConn {
        HostConn::new(unbounded().0, target)
    }

    #[test]
    fn a_remote_hosts_content_dims_unless_its_connection_is_open() {
        let remote = HostKey::Remote(BOX.to_owned());
        let mut box_conn = Connection::for_target(Target::tailnet(BOX, None));
        assert!(content_dimmed(&remote, None), "no connection: dimmed");
        assert!(content_dimmed(&remote, Some(&box_conn)), "connecting");
        box_conn.on_connecting();
        box_conn.on_socket_open(8787);
        box_conn.on_welcome(23, Vec::new());
        assert!(!content_dimmed(&remote, Some(&box_conn)), "open");
        let _ = box_conn.on_closed("dropped".to_owned());
        assert!(content_dimmed(&remote, Some(&box_conn)), "dropped");

        let local = Connection::new();
        assert!(
            !content_dimmed(&HostKey::Local, Some(&local)),
            "This PC's rows never dim, even while it connects"
        );
        assert!(!content_dimmed(&HostKey::Local, None));
    }

    #[test]
    fn removing_a_remote_host_drops_it_and_local_cannot_be_removed() {
        let mut hosts = Hosts::new(conn(Target::Local));
        let remote = HostKey::Remote(BOX.to_owned());
        assert!(
            hosts
                .insert(BOX.to_owned(), conn(Target::tailnet(BOX, None)))
                .is_some()
        );

        assert!(
            hosts.remove(&remote).is_some(),
            "the remote host is taken out"
        );
        assert_eq!(hosts.keys(), [HostKey::Local]);
        assert!(hosts.get(&remote).is_none());
        assert!(hosts.remove(&remote).is_none(), "nothing left to take");
        assert!(hosts.remove(&HostKey::Local).is_none(), "This PC stays");
        assert_eq!(hosts.keys(), [HostKey::Local]);
    }
}
