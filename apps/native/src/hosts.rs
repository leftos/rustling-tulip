//! The window's connections, one per host, and the routing that picks which
//! host a message goes to.

use std::collections::{BTreeMap, HashMap, VecDeque};

use futures::channel::mpsc::UnboundedSender;
use gpui::{Context, Window};
use protocol::{
    AgentCommandEntry, AgentId, ClientMessage, OptionValues, PresetTarget, SpawnTarget,
    SuggestTarget,
};

use crate::RootView;
use crate::connection::Connection;
use crate::net::{HandshakeInfo, NetCommand};
use crate::remote_access::RemoteStatusView;
use crate::remote_mode::Capabilities;
use crate::settings_view::{KeepAwake, SettingApply};
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
    pub(crate) mirrors: HostMirrors,
}

impl HostConn {
    /// A connection on its way to `target`, its commands going to `tx`.
    pub(crate) fn new(tx: UnboundedSender<NetCommand>, target: Target) -> Self {
        Self {
            tx,
            conn: Connection::for_target(target),
            handshake: None,
            mirrors: HostMirrors::default(),
        }
    }
}

/// The window's connections: This PC's, always present because it is the
/// layout home, and one per remote host by its host key, in the order
/// they joined.
pub(crate) struct Hosts {
    local: HostConn,
    remote: Vec<(String, HostConn)>,
}

impl Hosts {
    /// The connections of a window that holds only This PC's, `local`.
    pub(crate) fn new(local: HostConn) -> Self {
        Self {
            local,
            remote: Vec::new(),
        }
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

    /// Adds remote host `name`'s connection after the others; returns
    /// false, adding nothing, when the window already holds one to it.
    pub(crate) fn insert(&mut self, name: String, conn: HostConn) -> bool {
        if self.remote.iter().any(|(key, _)| *key == name) {
            return false;
        }
        self.remote.push((name, conn));
        true
    }
}

impl RootView {
    /// The connection the window shows. The window holds one connection,
    /// under [`HostKey::Local`] whatever its target; this is the one place
    /// that assumes so, and it goes once the window holds one per host.
    pub(crate) fn conn(&self) -> &Connection {
        &self.hosts.local().conn
    }

    /// `host`'s connection; `None` when the window has none to it.
    pub(crate) fn conn_of(&self, host: &HostKey) -> Option<&Connection> {
        self.hosts.get(host).map(|entry| &entry.conn)
    }

    /// What the daemon on `host` can do for this client, by the target the
    /// window's connection to it holds; This PC's for a host the window
    /// has no connection to.
    pub(crate) fn capabilities_of(&self, host: &HostKey) -> Capabilities {
        let conn = self.conn_of(host).unwrap_or_else(|| self.conn());
        Capabilities::for_target(conn.target())
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
        self.hosts.insert(name.clone(), HostConn::new(tx, target));
        Self::forward_net_events(HostKey::Remote(name), events, window, cx);
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

    /// The sender `host`'s commands go to, for a terminal pane to keep;
    /// `None` when the window has no connection to it.
    pub(crate) fn host_tx(&self, host: &HostKey) -> Option<UnboundedSender<NetCommand>> {
        self.hosts.get(host).map(|entry| entry.tx.clone())
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
    /// sidebar container key. A key carrying a connected remote host's
    /// prefix names that host, so its DIR and SH containers resolve too; a
    /// `repo:` or `ws:` key resolves by the id it holds. This PC for
    /// anything else.
    pub(crate) fn host_of_container(&self, id: &str) -> HostKey {
        if let Some((name, _)) = id.split_once("::") {
            let host = HostKey::Remote(name.to_owned());
            if self.hosts.get(&host).is_some() {
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
