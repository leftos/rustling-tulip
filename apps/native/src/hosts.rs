//! The window's connections, one per host, and the routing that picks which
//! host a message goes to.

use futures::channel::mpsc::UnboundedSender;
use gpui::{Context, Window};
use protocol::{ClientMessage, PresetTarget, SpawnTarget, SuggestTarget};

use crate::RootView;
use crate::connection::Connection;
use crate::net::{HandshakeInfo, NetCommand};
use crate::target::{HostKey, Target};

/// One host's connection: where its commands go, its state machine as its
/// network thread last reported it, and its daemon's handshake.
pub(crate) struct HostConn {
    tx: UnboundedSender<NetCommand>,
    conn: Connection,
    handshake: Option<HandshakeInfo>,
}

impl HostConn {
    /// A connection on its way to `target`, its commands going to `tx`.
    pub(crate) fn new(tx: UnboundedSender<NetCommand>, target: Target) -> Self {
        Self {
            tx,
            conn: Connection::for_target(target),
            handshake: None,
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

    /// The host that owns container `id`, a repo or a workspace; This PC
    /// for an id no host registered.
    pub(crate) fn host_of_container(&self, id: &str) -> HostKey {
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
