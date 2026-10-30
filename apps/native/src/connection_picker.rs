//! The host picker's model: the rows it offers (this PC, the saved tailnet
//! and SSH hosts and the owner's online tailnet peers), the typed-host
//! checks, the Add SSH host form's Test connection states, forgetting a
//! saved host, and the target the client boots onto. Plain Rust: the view
//! draws these rows, the Tailscale scan reaches it through [`PeerSource`]
//! and Test connection through [`SshTester`].

use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tailnet::{PeerStatus, Status, TailnetError};

use crate::sidebar::UiState;
use crate::target::{DEFAULT_TAILNET_PORT, HostNameError, SshHost, SshHostError, Target};

/// The label of the row that connects to this PC's own daemon.
const THIS_PC: &str = "This PC";

/// How a saved host is reached: `"tailnet"` (the default when a record has
/// no `kind`) or `"ssh"` in `native-ui.json`. An unknown kind fails the
/// record's decode, so it is dropped on load.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
    #[default]
    Tailnet,
    Ssh,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_tailnet(kind: &HostKind) -> bool {
    *kind == HostKind::Tailnet
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_zero(port: &u16) -> bool {
    *port == 0
}

/// A host the user added, as `native-ui.json` stores it: a tailnet host's
/// full `MagicDNS` name and the port it was added with, or an SSH host's
/// string, exactly as typed, and its optional remote command, with no port.
/// A tailnet record is written without its `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedHost {
    #[serde(default, skip_serializing_if = "is_tailnet")]
    pub kind: HostKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_command: Option<String>,
}

impl SavedHost {
    /// The record of `target`, as a switch to it saves it; `None` for the
    /// local daemon.
    #[must_use]
    pub fn from_target(target: &Target) -> Option<Self> {
        match target {
            Target::Local => None,
            Target::Tailnet(host) => Some(Self {
                kind: HostKind::Tailnet,
                name: host.name().to_owned(),
                port: host.port(),
                remote_command: None,
            }),
            Target::Ssh(host) => Some(Self {
                kind: HostKind::Ssh,
                name: host.name().to_owned(),
                port: 0,
                remote_command: host.remote_command().map(str::to_owned),
            }),
        }
    }

    /// The target this host dials, or why its name or port is not one.
    ///
    /// # Errors
    ///
    /// For a tailnet host, [`TypedHostError::Name`] when the name fails the
    /// DNS-name check, [`TypedHostError::Port`] when the port is 0; for an
    /// SSH host, [`TypedHostError::SshName`] when its string is refused.
    pub fn target(&self) -> Result<Target, TypedHostError> {
        match self.kind {
            HostKind::Tailnet => {
                if self.port == 0 {
                    return Err(TypedHostError::Port);
                }
                Target::parse_tailnet(&self.name, Some(self.port)).map_err(TypedHostError::Name)
            }
            HostKind::Ssh => Target::parse_ssh(&self.name, self.remote_command.as_deref())
                .map_err(TypedHostError::SshName),
        }
    }

    /// Whether this record is `target`'s: the same kind and name.
    #[must_use]
    pub fn is_for(&self, target: &Target) -> bool {
        Self::from_target(target)
            .is_some_and(|host| host.kind == self.kind && host.name == self.name)
    }

    /// This host in its normal form: a tailnet name as
    /// [`Target::parse_tailnet`] keeps it (lower-cased, without a trailing
    /// dot); an SSH host as typed, without a port and without a blank
    /// remote command.
    ///
    /// # Errors
    ///
    /// As [`SavedHost::target`].
    pub fn normalised(&self) -> Result<Self, TypedHostError> {
        let target = self.target()?;
        Ok(Self::from_target(&target).unwrap_or_else(|| self.clone()))
    }
}

/// Where the picker's peer list comes from: `tailscale status --json` in
/// the product, a fake in tests.
pub trait PeerSource: Send + Sync {
    /// The tailnet as this node sees it. Blocks until the answer is in.
    ///
    /// # Errors
    ///
    /// Any [`TailnetError`]; the picker shows its `Display`.
    fn status(&self) -> Result<Status, TailnetError>;
}

/// The system's Tailscale: `tailnet::Cli::locate().status()`, run on a
/// one-shot thread with its own current-thread tokio runtime, so neither
/// the UI's executor nor the network thread waits on it.
pub struct SystemPeers;

impl PeerSource for SystemPeers {
    fn status(&self) -> Result<Status, TailnetError> {
        let scan = std::thread::Builder::new()
            .name("tailnet-status".to_owned())
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| {
                        TailnetError::Failed(format!("starting a runtime for status: {err}"))
                    })?;
                runtime.block_on(tailnet::Cli::locate().status())
            })
            .map_err(|err| TailnetError::Failed(format!("starting the status thread: {err}")))?;
        scan.join().unwrap_or_else(|_| {
            Err(TailnetError::Failed(
                "the status thread panicked".to_owned(),
            ))
        })
    }
}

/// Resolves when a running Test connection is cancelled: its sender sent,
/// or dropped with the form that held it.
pub type SshTestCancel = oneshot::Receiver<()>;

/// Where Test connection runs: `ssh` to the host once in the product, a
/// fake in tests.
pub trait SshTester: Send + Sync {
    /// Runs the host's remote command once and resolves to `Ok` when its
    /// daemon answers `Welcome`, else to the text that says why not. The
    /// child is killed once `cancel` resolves.
    fn test(&self, host: SshHost, cancel: SshTestCancel) -> BoxFuture<'static, Result<(), String>>;
}

/// The system's `ssh`: [`crate::ssh::test_connection`], run on a one-shot
/// thread with its own current-thread tokio runtime, so neither the UI's
/// executor nor the network thread waits on it.
pub struct SystemSsh;

impl SshTester for SystemSsh {
    fn test(&self, host: SshHost, cancel: SshTestCancel) -> BoxFuture<'static, Result<(), String>> {
        let (answer_tx, answer_rx) = oneshot::channel();
        let started = std::thread::Builder::new()
            .name("ssh-test".to_owned())
            .spawn(move || {
                let outcome = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        runtime.block_on(crate::ssh::test_connection(&host, cancel.map(|_| ())))
                    }
                    Err(err) => Err(format!("starting a runtime for the SSH test: {err}")),
                };
                if answer_tx.send(outcome).is_err() {
                    tracing::debug!("the SSH test ended after the picker stopped waiting");
                }
            });
        match started {
            Ok(_) => async move {
                answer_rx.await.unwrap_or_else(|_| {
                    Err("the SSH test thread ended without an answer".to_owned())
                })
            }
            .boxed(),
            Err(err) => {
                futures::future::ready(Err(format!("starting the SSH test thread: {err}"))).boxed()
            }
        }
    }
}

/// What the Add SSH host form's Test connection last did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SshTest {
    /// Not run since the form opened or a field changed.
    #[default]
    Idle,
    /// Running: the button reads "Cancel".
    Testing,
    /// The host's daemon answered; this is the host's short name.
    Passed(String),
    /// It did not; this is why, `ssh`'s own text unchanged.
    Failed(String),
}

/// The line Test connection shows while it runs.
pub const SSH_TESTING: &str = "Testing the connection…";

impl SshTest {
    /// The state a finished test of `host` leaves.
    #[must_use]
    pub fn finished(host: &SshHost, outcome: Result<(), String>) -> Self {
        match outcome {
            Ok(()) => Self::Passed(host.short_name().to_owned()),
            Err(text) => Self::Failed(text),
        }
    }

    /// The Test connection button's label: "Cancel" while it runs.
    #[must_use]
    pub fn button_label(&self) -> &'static str {
        if *self == Self::Testing {
            "Cancel"
        } else {
            "Test connection"
        }
    }

    /// The line under the form; `None` before a test.
    #[must_use]
    pub fn text(&self) -> Option<String> {
        match self {
            Self::Idle => None,
            Self::Testing => Some(SSH_TESTING.to_owned()),
            Self::Passed(short) => Some(format!("Connected: {short}")),
            Self::Failed(text) => Some(text.clone()),
        }
    }
}

/// The SSH host the user typed into the Add SSH host form: the host string
/// exactly as typed, and the remote command, a blank one being none.
///
/// # Errors
///
/// [`TypedHostError::SshName`] when [`Target::parse_ssh`] refuses the host.
pub fn typed_ssh_target(host: &str, remote_command: &str) -> Result<Target, TypedHostError> {
    Target::parse_ssh(host, Some(remote_command)).map_err(TypedHostError::SshName)
}

/// Whether a switch to `target` waits for a "not approved" to open the
/// pairing dialog: only a tailnet host pairs.
#[must_use]
pub fn pairs_on_switch(target: &Target) -> bool {
    matches!(target, Target::Tailnet(_))
}

/// Which kind of choice a row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// This PC's own daemon.
    Local,
    /// A host the user saved.
    Saved,
    /// An online peer from the tailnet scan that is not saved.
    Peer,
}

/// One choice the picker offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    /// "This PC", else a tailnet host's full `MagicDNS` name or an SSH
    /// host's string.
    pub label: String,
    /// What picking the row connects to: its host's kind and name.
    pub target: Target,
}

impl Row {
    /// Whether the row is an SSH host.
    #[must_use]
    pub fn is_ssh(&self) -> bool {
        matches!(self.target, Target::Ssh(_))
    }

    /// The row's key in its selectors: an SSH host's string after `ssh:`,
    /// so it never shares one with a tailnet host of that name; else its
    /// label.
    #[must_use]
    pub fn key(&self) -> String {
        if self.is_ssh() {
            format!("ssh:{}", self.label)
        } else {
            self.label.clone()
        }
    }
}

/// Why a typed host was refused: its `Display` is the message the picker
/// shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedHostError {
    /// The name failed the DNS-name check.
    Name(HostNameError),
    /// The SSH host string failed its check.
    SshName(SshHostError),
    /// The port is not a number from 1 to 65535.
    Port,
}

impl std::fmt::Display for TypedHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Name(err) => err.fmt(f),
            Self::SshName(err) => err.fmt(f),
            Self::Port => f.write_str("The port is a number from 1 to 65535."),
        }
    }
}

/// The host the user typed into the name and Port fields. The name is
/// checked first; the port's surrounding whitespace is ignored.
///
/// # Errors
///
/// [`TypedHostError::Name`] when the name fails [`Target::parse_tailnet`],
/// else [`TypedHostError::Port`] when the port is not 1 to 65535.
pub fn typed_target(name: &str, port: &str) -> Result<Target, TypedHostError> {
    Target::parse_tailnet(name, None).map_err(TypedHostError::Name)?;
    let port = port
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(TypedHostError::Port)?;
    Target::parse_tailnet(name, Some(port)).map_err(TypedHostError::Name)
}

/// The target the client connects to at start: the last target when it is
/// still a saved host of its kind and name, dialled as it was saved; else
/// this PC. Tailscale and `ssh` are never asked, and a saved host that
/// cannot be reached is still the boot target.
#[must_use]
pub fn resolve_boot_target(ui: &UiState) -> Target {
    let Some(last) = &ui.last_target else {
        return Target::Local;
    };
    let Some(host) = ui
        .saved_hosts
        .iter()
        .find(|host| host.kind == last.kind && host.name == last.name)
    else {
        tracing::info!(
            host = %last.name,
            "the last target is not a saved host; starting on this PC"
        );
        return Target::Local;
    };
    host.target().unwrap_or_else(|err| {
        tracing::warn!(host = %host.name, "the saved host is not valid ({err}); starting on this PC");
        Target::Local
    })
}

/// The last tailnet scan's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scan {
    /// No scan has answered yet.
    Pending,
    /// The offered peers' full names, sorted.
    Peers(Vec<String>),
    /// The scan failed with this text.
    Failed(String),
}

/// The picker's state: the saved hosts and the last scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionPicker {
    saved: Vec<SavedHost>,
    scan: Scan,
}

impl ConnectionPicker {
    /// A picker over `saved`, in saved order, before any scan.
    #[must_use]
    pub fn new(saved: Vec<SavedHost>) -> Self {
        Self {
            saved,
            scan: Scan::Pending,
        }
    }

    #[must_use]
    pub fn saved(&self) -> &[SavedHost] {
        &self.saved
    }

    /// Takes a scan's answer: the owner's online, untagged, unshared peers,
    /// or the error's text.
    pub fn apply_scan(&mut self, scan: Result<Status, TailnetError>) {
        self.scan = match scan {
            Ok(status) => Scan::Peers(offered_peers(&status)),
            Err(err) => Scan::Failed(err.to_string()),
        };
    }

    /// The last scan's error text; `None` when it worked or has not answered.
    #[must_use]
    pub fn scan_error(&self) -> Option<&str> {
        match &self.scan {
            Scan::Failed(text) => Some(text),
            Scan::Pending | Scan::Peers(_) => None,
        }
    }

    /// This PC, then the saved hosts in saved order, then the scanned peers
    /// not already saved as tailnet hosts, by name. A peer dials
    /// [`DEFAULT_TAILNET_PORT`]; a saved tailnet host of the same name is
    /// listed once, with its own port.
    #[must_use]
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = vec![Row {
            kind: RowKind::Local,
            label: THIS_PC.to_owned(),
            target: Target::Local,
        }];
        rows.extend(self.saved.iter().filter_map(|host| {
            let target = host.target().ok()?;
            Some(Row {
                kind: RowKind::Saved,
                label: host.name.clone(),
                target,
            })
        }));
        if let Scan::Peers(peers) = &self.scan {
            rows.extend(
                peers
                    .iter()
                    .filter(|name| {
                        !self
                            .saved
                            .iter()
                            .any(|host| host.kind == HostKind::Tailnet && &host.name == *name)
                    })
                    .map(|name| Row {
                        kind: RowKind::Peer,
                        label: name.clone(),
                        target: Target::tailnet(name, Some(DEFAULT_TAILNET_PORT)),
                    }),
            );
        }
        rows
    }

    /// Whether the scan has not answered yet.
    #[must_use]
    pub fn scan_pending(&self) -> bool {
        self.scan == Scan::Pending
    }

    /// Saves `host` after the others; a saved host of its kind and name is
    /// replaced by it, in its place, so a new port or remote command is
    /// kept. A host of the other kind and the same name is left alone.
    pub fn remember(&mut self, host: SavedHost) {
        match self
            .saved
            .iter_mut()
            .find(|saved| saved.kind == host.kind && saved.name == host.name)
        {
            Some(saved) => *saved = host,
            None => self.saved.push(host),
        }
    }

    /// Forgets the saved host `target` dials. Returns whether it was
    /// `current`, so the caller switches to this PC.
    /// A tailnet name matches in its normal form, an SSH string as typed; a
    /// host of the other kind and the same name is left alone.
    pub fn forget(&mut self, target: &Target, current: &Target) -> bool {
        let before = self.saved.len();
        self.saved.retain(|host| !host.is_for(target));
        let removed = self.saved.len() != before;
        removed && SavedHost::from_target(target).is_some_and(|host| host.is_for(current))
    }
}

/// The full names of the peers the picker offers from `status`: online,
/// untagged, not shared in, and logged in as this node's own user, each
/// through the DNS-name check. Without this node's own entry the owner is
/// unknown and none is offered.
fn offered_peers(status: &Status) -> Vec<String> {
    let Some(me) = &status.self_node else {
        tracing::warn!("tailscale status has no entry for this PC; no peer is offered");
        return Vec::new();
    };
    let mut names: Vec<String> = status
        .peer
        .values()
        .filter(|peer| is_owner_device(peer, me))
        .filter_map(|peer| match Target::parse_tailnet(&peer.dns_name, None) {
            Ok(target) => target.tailnet_host().map(|host| host.name().to_owned()),
            Err(err) => {
                tracing::warn!(dns_name = %peer.dns_name, "a peer's DNS name is refused ({err})");
                None
            }
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Whether `peer` is one of the owner's own devices that can serve: online,
/// untagged, not shared in, and logged in as `me`'s user.
fn is_owner_device(peer: &PeerStatus, me: &PeerStatus) -> bool {
    peer.online && peer.tags.is_empty() && !peer.sharee_node && peer.user_id == me.user_id
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{
        ConnectionPicker, HostKind, PeerSource, Row, RowKind, SSH_TESTING, SavedHost, SshTest,
        pairs_on_switch, resolve_boot_target, typed_ssh_target, typed_target,
    };
    use crate::sidebar::UiState;
    use crate::target::Target;
    use tailnet::{PeerStatus, Status, TailnetError};

    const OWNER: i64 = 42;
    const GENERIC: &str = "That is not a host name: use letters, digits, hyphens and dots.";
    const BARE: &str = "Use the full name, for example box.tail1234.ts.net.";
    const PORT_IN_NAME: &str = "Put the port in the Port field, not in the name.";
    const PORT_RANGE: &str = "The port is a number from 1 to 65535.";

    /// This PC's own node.
    fn me() -> PeerStatus {
        PeerStatus {
            dns_name: "this-pc.tail1234.ts.net.".to_owned(),
            user_id: OWNER,
            online: true,
            ..PeerStatus::default()
        }
    }

    /// An online, untagged, unshared peer of the owner named `name`, with
    /// the trailing dot `status --json` gives it.
    fn peer(name: &str) -> PeerStatus {
        PeerStatus {
            dns_name: format!("{name}."),
            user_id: OWNER,
            online: true,
            ..PeerStatus::default()
        }
    }

    fn status(peers: Vec<PeerStatus>) -> Status {
        Status {
            self_node: Some(me()),
            peer: peers
                .into_iter()
                .enumerate()
                .map(|(i, peer)| (format!("nodekey:{i}"), peer))
                .collect(),
        }
    }

    fn saved(name: &str, port: u16) -> SavedHost {
        SavedHost {
            kind: HostKind::Tailnet,
            name: name.to_owned(),
            port,
            remote_command: None,
        }
    }

    fn row(kind: RowKind, name: &str, port: u16) -> Row {
        Row {
            kind,
            label: name.to_owned(),
            target: Target::tailnet(name, Some(port)),
        }
    }

    fn this_pc() -> Row {
        Row {
            kind: RowKind::Local,
            label: "This PC".to_owned(),
            target: Target::Local,
        }
    }

    /// The names of the peer rows `peers` give.
    fn peer_names(peers: Vec<PeerStatus>) -> Vec<String> {
        let mut picker = ConnectionPicker::new(Vec::new());
        picker.apply_scan(Ok(status(peers)));
        picker
            .rows()
            .into_iter()
            .filter(|row| row.kind == RowKind::Peer)
            .map(|row| row.label)
            .collect()
    }

    struct FailingSource(TailnetError);

    impl PeerSource for FailingSource {
        fn status(&self) -> Result<Status, TailnetError> {
            Err(self.0.clone())
        }
    }

    #[test]
    fn rows_list_local_then_saved_then_online_peers() {
        let mut picker = ConnectionPicker::new(vec![
            saved("zed.tail1234.ts.net", 8787),
            saved("alpha.tail1234.ts.net", 9443),
        ]);
        assert_eq!(
            picker.rows(),
            [
                this_pc(),
                row(RowKind::Saved, "zed.tail1234.ts.net", 8787),
                row(RowKind::Saved, "alpha.tail1234.ts.net", 9443),
            ],
            "before the scan: this PC and the saved hosts in saved order"
        );

        picker.apply_scan(Ok(status(vec![
            peer("mid.tail1234.ts.net"),
            peer("beta.tail1234.ts.net"),
            peer("zed.tail1234.ts.net"),
        ])));
        assert_eq!(
            picker.rows(),
            [
                this_pc(),
                row(RowKind::Saved, "zed.tail1234.ts.net", 8787),
                row(RowKind::Saved, "alpha.tail1234.ts.net", 9443),
                row(RowKind::Peer, "beta.tail1234.ts.net", 8787),
                row(RowKind::Peer, "mid.tail1234.ts.net", 8787),
            ],
            "then the peers not already saved, by DNS name"
        );
        assert_eq!(picker.scan_error(), None);
    }

    #[test]
    fn peer_rows_use_the_dns_name_without_the_trailing_dot() {
        let mut picker = ConnectionPicker::new(Vec::new());
        picker.apply_scan(Ok(status(vec![peer("Box.Tail1234.ts.net")])));
        assert_eq!(
            picker.rows(),
            [this_pc(), row(RowKind::Peer, "box.tail1234.ts.net", 8787)]
        );
        assert_eq!(
            peer_names(vec![peer(""), peer("box/ws.tail1234.ts.net")]),
            [] as [String; 0],
            "a peer whose DNS name fails the check is not offered"
        );
    }

    #[test]
    fn offline_tagged_and_shared_peers_are_not_offered() {
        let offline = PeerStatus {
            online: false,
            ..peer("offline.tail1234.ts.net")
        };
        let tagged = PeerStatus {
            tags: vec!["tag:server".to_owned()],
            ..peer("tagged.tail1234.ts.net")
        };
        let shared = PeerStatus {
            sharee_node: true,
            ..peer("shared.tail1234.ts.net")
        };
        let stranger = PeerStatus {
            user_id: OWNER + 1,
            ..peer("stranger.tail1234.ts.net")
        };
        assert_eq!(
            peer_names(vec![
                offline,
                tagged,
                shared,
                stranger,
                peer("ok.tail1234.ts.net")
            ]),
            ["ok.tail1234.ts.net"]
        );

        let mut picker = ConnectionPicker::new(Vec::new());
        picker.apply_scan(Ok(Status {
            self_node: None,
            ..status(vec![peer("ok.tail1234.ts.net")])
        }));
        assert_eq!(
            picker.rows(),
            [this_pc()],
            "without this PC's own node the owner is unknown: no peer is offered"
        );
    }

    #[test]
    fn a_saved_port_wins_for_a_listed_peer_of_that_name() {
        let mut picker = ConnectionPicker::new(vec![saved("box.tail1234.ts.net", 9443)]);
        picker.apply_scan(Ok(status(vec![peer("Box.tail1234.ts.net")])));
        assert_eq!(
            picker.rows(),
            [this_pc(), row(RowKind::Saved, "box.tail1234.ts.net", 9443)],
            "one row for the host, dialling the port it was saved with"
        );
    }

    #[test]
    fn typed_name_goes_through_the_dns_check_and_names_the_refusal() {
        let refusal = |name: &str| typed_target(name, "8787").map_err(|err| err.to_string());
        assert_eq!(refusal("box.tail1234.ts.net/ws"), Err(GENERIC.to_owned()));
        assert_eq!(refusal("user@box.tail1234.ts.net"), Err(GENERIC.to_owned()));
        assert_eq!(refusal("box"), Err(BARE.to_owned()));
        assert_eq!(
            refusal("box.tail1234.ts.net:8787"),
            Err(PORT_IN_NAME.to_owned())
        );
        assert_eq!(
            typed_target("Box.Tail1234.ts.net.", "9443"),
            Ok(Target::tailnet("box.tail1234.ts.net", Some(9443)))
        );
        assert_eq!(
            typed_target("box", "0").map_err(|err| err.to_string()),
            Err(BARE.to_owned()),
            "the name is checked first"
        );
    }

    #[test]
    fn typed_port_outside_1_to_65535_is_refused() {
        for port in ["0", "65536", "", "port", "-1", "8787.5"] {
            assert_eq!(
                typed_target("box.tail1234.ts.net", port).map_err(|err| err.to_string()),
                Err(PORT_RANGE.to_owned()),
                "{port:?}"
            );
        }
        assert_eq!(
            typed_target("box.tail1234.ts.net", "1"),
            Ok(Target::tailnet("box.tail1234.ts.net", Some(1)))
        );
        assert_eq!(
            typed_target("box.tail1234.ts.net", " 65535 "),
            Ok(Target::tailnet("box.tail1234.ts.net", Some(65535)))
        );
    }

    #[test]
    fn forget_removes_a_saved_host() {
        let mut picker = ConnectionPicker::new(vec![
            saved("a.tail1234.ts.net", 8787),
            saved("b.tail1234.ts.net", 9443),
        ]);
        assert!(
            !picker.forget(
                &Target::tailnet("a.tail1234.ts.net", Some(8787)),
                &Target::Local
            ),
            "a host that is not the current target"
        );
        assert_eq!(picker.saved(), [saved("b.tail1234.ts.net", 9443)]);
        assert_eq!(
            picker.rows(),
            [this_pc(), row(RowKind::Saved, "b.tail1234.ts.net", 9443)]
        );

        assert!(
            !picker.forget(
                &Target::tailnet("c.tail1234.ts.net", Some(8787)),
                &Target::Local
            ),
            "an unknown host"
        );
        assert_eq!(picker.saved(), [saved("b.tail1234.ts.net", 9443)]);

        let current = Target::tailnet("b.tail1234.ts.net", Some(9443));
        assert!(
            picker.forget(&current, &current),
            "the current host was forgotten"
        );
        assert_eq!(picker.saved(), [] as [SavedHost; 0]);
    }

    #[test]
    fn forget_matches_a_normalised_name() {
        let mut picker = ConnectionPicker::new(vec![saved("box.tail1234.ts.net", 9443)]);
        let current = Target::tailnet("box.tail1234.ts.net", Some(9443));
        assert!(
            picker.forget(
                &Target::tailnet("Box.Tail1234.ts.net.", Some(9443)),
                &current
            ),
            "the current host was forgotten under another spelling of its name"
        );
        assert_eq!(picker.saved(), [] as [SavedHost; 0]);
    }

    #[test]
    fn remember_adds_a_host_once_and_updates_its_port() {
        let mut picker = ConnectionPicker::new(vec![saved("a.tail1234.ts.net", 8787)]);
        picker.remember(saved("b.tail1234.ts.net", 9443));
        assert_eq!(
            picker.saved(),
            [
                saved("a.tail1234.ts.net", 8787),
                saved("b.tail1234.ts.net", 9443)
            ],
            "a new host goes last"
        );

        picker.remember(saved("b.tail1234.ts.net", 9443));
        assert_eq!(
            picker.saved(),
            [
                saved("a.tail1234.ts.net", 8787),
                saved("b.tail1234.ts.net", 9443)
            ],
            "the same name and port changes nothing"
        );

        picker.remember(saved("a.tail1234.ts.net", 1234));
        assert_eq!(
            picker.saved(),
            [
                saved("a.tail1234.ts.net", 1234),
                saved("b.tail1234.ts.net", 9443)
            ],
            "a saved name takes the new port, in its place"
        );
    }

    #[test]
    fn resolve_boot_target_uses_a_saved_host() {
        let ui = UiState {
            saved_hosts: vec![
                saved("a.tail1234.ts.net", 8787),
                saved("box.tail1234.ts.net", 9443),
            ],
            last_target: Some(saved("box.tail1234.ts.net", 9443)),
            ..UiState::default()
        };
        assert_eq!(
            resolve_boot_target(&ui),
            Target::tailnet("box.tail1234.ts.net", Some(9443))
        );
        assert_eq!(
            resolve_boot_target(&UiState {
                last_target: None,
                ..ui
            }),
            Target::Local,
            "no last target is this PC"
        );
    }

    #[test]
    fn boot_target_is_a_saved_ssh_host() {
        let devbox = SavedHost {
            kind: HostKind::Ssh,
            name: "devbox".to_owned(),
            port: 0,
            remote_command: None,
        };
        let ui = UiState {
            saved_hosts: vec![saved("devbox.tail1234.ts.net", 8787), devbox.clone()],
            last_target: Some(devbox.clone()),
            ..UiState::default()
        };
        assert_eq!(
            Some(resolve_boot_target(&ui)),
            Target::parse_ssh("devbox", None).ok()
        );
        let tailnet_only = UiState {
            saved_hosts: vec![SavedHost {
                kind: HostKind::Tailnet,
                port: 8787,
                ..devbox.clone()
            }],
            last_target: Some(devbox),
            ..UiState::default()
        };
        assert_eq!(
            resolve_boot_target(&tailnet_only),
            Target::Local,
            "a saved host of another kind is not the last target"
        );
    }

    #[test]
    fn missing_saved_host_falls_back_to_local() {
        let last = Some(saved("box.tail1234.ts.net", 9443));
        assert_eq!(
            resolve_boot_target(&UiState {
                last_target: last.clone(),
                ..UiState::default()
            }),
            Target::Local
        );
        assert_eq!(
            resolve_boot_target(&UiState {
                saved_hosts: vec![saved("other.tail1234.ts.net", 9443)],
                last_target: last,
                ..UiState::default()
            }),
            Target::Local
        );
    }

    #[test]
    fn tailscale_errors_show_their_display_text() {
        let mut picker = ConnectionPicker::new(vec![saved("box.tail1234.ts.net", 8787)]);
        picker.apply_scan(FailingSource(TailnetError::NotRunning).status());
        assert_eq!(
            picker.scan_error(),
            Some("the tailscale CLI could not reach tailscaled (not running?)")
        );
        assert_eq!(
            picker.rows(),
            [this_pc(), row(RowKind::Saved, "box.tail1234.ts.net", 8787)],
            "this PC and the saved hosts are still offered"
        );

        picker.apply_scan(Ok(status(Vec::new())));
        assert_eq!(picker.scan_error(), None, "a scan that works clears it");
    }

    const BOX: &str = "box.tail1234.ts.net";

    fn ssh_saved(name: &str, remote_command: Option<&str>) -> SavedHost {
        SavedHost {
            kind: HostKind::Ssh,
            name: name.to_owned(),
            port: 0,
            remote_command: remote_command.map(str::to_owned),
        }
    }

    fn ssh(name: &str) -> Target {
        Target::parse_ssh(name, None).expect("an SSH host string")
    }

    fn ssh_row(name: &str) -> Row {
        Row {
            kind: RowKind::Saved,
            label: name.to_owned(),
            target: ssh(name),
        }
    }

    #[test]
    fn rows_list_a_saved_ssh_host_in_saved_order_keyed_apart() {
        let picker = ConnectionPicker::new(vec![
            ssh_saved("me@devbox", None),
            saved("a.tail1234.ts.net", 8787),
        ]);
        let rows = picker.rows();
        assert_eq!(
            rows,
            [
                this_pc(),
                ssh_row("me@devbox"),
                row(RowKind::Saved, "a.tail1234.ts.net", 8787),
            ]
        );
        assert_eq!(
            rows.iter().map(Row::key).collect::<Vec<_>>(),
            ["This PC", "ssh:me@devbox", "a.tail1234.ts.net"]
        );
    }

    #[test]
    fn an_ssh_host_does_not_hide_a_tailnet_peer_of_the_same_name() {
        let mut picker = ConnectionPicker::new(vec![ssh_saved(BOX, None)]);
        picker.apply_scan(Ok(status(vec![peer(BOX)])));
        assert_eq!(
            picker.rows(),
            [this_pc(), ssh_row(BOX), row(RowKind::Peer, BOX, 8787)]
        );
    }

    #[test]
    fn remember_replaces_the_record_of_the_same_kind_and_name() {
        let mut picker = ConnectionPicker::new(vec![ssh_saved("devbox", Some("old.cmd"))]);
        picker.remember(ssh_saved("devbox", Some("new.cmd")));
        assert_eq!(
            picker.saved(),
            [ssh_saved("devbox", Some("new.cmd"))],
            "a changed remote command is kept"
        );
        picker.remember(ssh_saved("devbox", None));
        assert_eq!(picker.saved(), [ssh_saved("devbox", None)]);
    }

    #[test]
    fn remember_keeps_an_ssh_and_a_tailnet_host_of_one_name_apart() {
        let mut picker = ConnectionPicker::new(vec![saved(BOX, 8787)]);
        picker.remember(ssh_saved(BOX, None));
        assert_eq!(picker.saved(), [saved(BOX, 8787), ssh_saved(BOX, None)]);
        picker.remember(saved(BOX, 9443));
        assert_eq!(
            picker.saved(),
            [saved(BOX, 9443), ssh_saved(BOX, None)],
            "the tailnet record takes the port; the SSH one is untouched"
        );
    }

    #[test]
    fn forget_an_ssh_host_matches_its_string_as_typed_and_reports_the_current_one() {
        let mut picker = ConnectionPicker::new(vec![
            ssh_saved("Me@DevBox", None),
            ssh_saved("me@devbox", None),
        ]);
        assert!(
            picker.forget(&ssh("Me@DevBox"), &ssh("Me@DevBox")),
            "the current SSH host was forgotten"
        );
        assert_eq!(picker.saved(), [ssh_saved("me@devbox", None)]);
        assert!(!picker.forget(&ssh("me@devbox"), &Target::Local));
        assert_eq!(picker.saved(), [] as [SavedHost; 0]);
    }

    #[test]
    fn forget_leaves_the_other_kind_of_one_name() {
        let tailnet = Target::tailnet(BOX, Some(8787));
        let mut picker = ConnectionPicker::new(vec![saved(BOX, 8787), ssh_saved(BOX, None)]);
        assert!(
            !picker.forget(&ssh(BOX), &tailnet),
            "the tailnet host is current, not the SSH one"
        );
        assert_eq!(picker.saved(), [saved(BOX, 8787)]);

        let mut picker = ConnectionPicker::new(vec![saved(BOX, 8787), ssh_saved(BOX, None)]);
        assert!(!picker.forget(&tailnet, &ssh(BOX)));
        assert_eq!(picker.saved(), [ssh_saved(BOX, None)]);
    }

    #[test]
    fn only_a_switch_to_a_tailnet_host_arms_pairing() {
        assert!(pairs_on_switch(&Target::tailnet(BOX, Some(8787))));
        assert!(!pairs_on_switch(&ssh("devbox")));
        assert!(!pairs_on_switch(&Target::Local));
    }

    #[test]
    fn ssh_test_states_say_what_the_form_shows() {
        let target = ssh("me@devbox:2222");
        let host = target.ssh_host().expect("an SSH target");
        assert_eq!(SshTest::Idle.text(), None);
        assert_eq!(SshTest::Idle.button_label(), "Test connection");
        assert_eq!(SshTest::Testing.text().as_deref(), Some(SSH_TESTING));
        assert_eq!(SshTest::Testing.button_label(), "Cancel");
        let passed = SshTest::finished(host, Ok(()));
        assert_eq!(passed, SshTest::Passed("devbox".to_owned()));
        assert_eq!(passed.text().as_deref(), Some("Connected: devbox"));
        assert_eq!(passed.button_label(), "Test connection");
        let failed = SshTest::finished(host, Err("Permission denied (publickey).".to_owned()));
        assert_eq!(
            failed.text().as_deref(),
            Some("Permission denied (publickey)."),
            "ssh's text unchanged"
        );
    }

    #[test]
    fn typed_ssh_target_keeps_the_host_as_typed_and_a_blank_command_as_none() {
        assert_eq!(typed_ssh_target("Me@Box", "  "), Ok(ssh("Me@Box")));
        assert_eq!(
            typed_ssh_target("devbox", "rtd.cmd").ok(),
            Target::parse_ssh("devbox", Some("rtd.cmd")).ok()
        );
        let refusal = |host: &str| typed_ssh_target(host, "").map_err(|err| err.to_string());
        assert_eq!(
            refusal(""),
            Err("Type an SSH host: an alias from your SSH config, or user@host.".to_owned())
        );
        assert_eq!(
            refusal("-oProxyCommand=x"),
            Err("An SSH host cannot start with a dash.".to_owned())
        );
    }
}
