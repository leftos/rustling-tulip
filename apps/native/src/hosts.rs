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
use crate::connection_picker::SavedHost;
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
    /// The id the daemon named itself by in its last `Welcome` on this
    /// connection, if it named one.
    daemon_id: Option<String>,
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

    /// Takes remote host `key`'s connection out of the window; `None` when
    /// the window holds none to it, and always for This PC, whose
    /// connection the window never gives up.
    pub(crate) fn remove(&mut self, key: &HostKey) -> Option<HostConn> {
        let HostKey::Remote(name) = key else {
            return None;
        };
        let index = self.remote.iter().position(|(held, _)| held == name)?;
        Some(self.remote.remove(index).1)
    }
}

impl RootView {
    /// The connection the window shows. The window holds one connection,
    /// under [`HostKey::Local`] whatever its target; this is the one place
    /// that assumes so, and it goes once the window holds one per host.
    pub(crate) fn conn(&self) -> &Connection {
        &self.hosts.local().conn
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
    /// thread quits once its sender is gone. Its sidebar slice stays.
    /// Returns whether the window held one.
    pub(crate) fn disconnect_host(&mut self, host: &HostKey) -> bool {
        let held = self.hosts.remove(host).is_some();
        if held {
            tracing::info!(?host, "disconnected from the host");
        }
        held
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
        self.disconnect_host(&duplicate);
        self.clear_remote_slice(&duplicate, window, cx);
        let dropped_self = duplicate == *host;
        self.duplicate_of.insert(duplicate, owner);
        dropped_self
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
        self.duplicate_of.remove(&HostKey::Remote(name.clone()));
        self.hosts.insert(name.clone(), HostConn::new(tx, target));
        Self::forward_net_events(HostKey::Remote(name), events, window, cx);
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

#[cfg(test)]
mod tests {
    use futures::channel::mpsc::unbounded;

    use super::{HostConn, Hosts};
    use crate::target::{HostKey, Target};

    const BOX: &str = "box.tail1234.ts.net";

    fn conn(target: Target) -> HostConn {
        HostConn::new(unbounded().0, target)
    }

    #[test]
    fn removing_a_remote_host_drops_it_and_local_cannot_be_removed() {
        let mut hosts = Hosts::new(conn(Target::Local));
        let remote = HostKey::Remote(BOX.to_owned());
        assert!(hosts.insert(BOX.to_owned(), conn(Target::tailnet(BOX, None))));

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
