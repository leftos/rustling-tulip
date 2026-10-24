//! The host picker's model: the rows it offers (this PC, the saved hosts and
//! the owner's online tailnet peers), the typed-host check, forgetting a
//! saved host, and the target the client boots onto. Plain Rust: the view
//! draws these rows and the Tailscale scan reaches it through [`PeerSource`].

use serde::{Deserialize, Serialize};
use tailnet::{PeerStatus, Status, TailnetError};

use crate::sidebar::UiState;
use crate::target::{DEFAULT_TAILNET_PORT, HostNameError, Target};

/// The label of the row that connects to this PC's own daemon.
const THIS_PC: &str = "This PC";

/// A tailnet host the user added, as `native-ui.json` stores it: its full
/// `MagicDNS` name and the port it was added with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedHost {
    pub name: String,
    pub port: u16,
}

impl SavedHost {
    /// The target this host dials, or why its name or port is not one.
    ///
    /// # Errors
    ///
    /// [`TypedHostError::Name`] when the name fails the DNS-name check,
    /// [`TypedHostError::Port`] when the port is 0.
    pub fn target(&self) -> Result<Target, TypedHostError> {
        if self.port == 0 {
            return Err(TypedHostError::Port);
        }
        Target::parse_tailnet(&self.name, Some(self.port)).map_err(TypedHostError::Name)
    }

    /// This host with its name in [`Target::parse_tailnet`]'s normal form:
    /// lower-cased, without a trailing dot.
    ///
    /// # Errors
    ///
    /// As [`SavedHost::target`].
    pub fn normalised(&self) -> Result<Self, TypedHostError> {
        let target = self.target()?;
        Ok(target.tailnet_host().map_or_else(
            || self.clone(),
            |host| Self {
                name: host.name().to_owned(),
                port: host.port(),
            },
        ))
    }
}

/// `name` in the normal form saved hosts are kept in; a name that fails the
/// DNS-name check is returned as it is, and so matches no saved host.
fn normalised_name(name: &str) -> String {
    Target::parse_tailnet(name, None)
        .ok()
        .and_then(|target| target.tailnet_host().map(|host| host.name().to_owned()))
        .unwrap_or_else(|| name.to_owned())
}

/// Where the picker's peer list comes from: `tailscale status --json` in
/// the product, a fake in tests.
pub trait PeerSource {
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
    /// "This PC", else the host's full `MagicDNS` name.
    pub label: String,
    /// What picking the row connects to.
    pub target: Target,
}

/// Why a typed host was refused: its `Display` is the message the picker
/// shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedHostError {
    /// The name failed the DNS-name check.
    Name(HostNameError),
    /// The port is not a number from 1 to 65535.
    Port,
}

impl std::fmt::Display for TypedHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Name(err) => err.fmt(f),
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
/// still a saved host, dialled on the port it was saved with; else this PC.
/// Tailscale is never asked, and a saved host that cannot be reached is
/// still the boot target.
#[must_use]
pub fn resolve_boot_target(ui: &UiState) -> Target {
    let Some(last) = &ui.last_target else {
        return Target::Local;
    };
    let Some(host) = ui.saved_hosts.iter().find(|host| host.name == last.name) else {
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
    /// not already saved, by name. A peer dials [`DEFAULT_TAILNET_PORT`]; a
    /// saved host of the same name is listed once, with its own port.
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
                    .filter(|name| !self.saved.iter().any(|host| &host.name == *name))
                    .map(|name| Row {
                        kind: RowKind::Peer,
                        label: name.clone(),
                        target: Target::tailnet(name, Some(DEFAULT_TAILNET_PORT)),
                    }),
            );
        }
        rows
    }

    /// Forgets the saved host `name`. Returns whether it was `current`, so
    /// the caller switches to this PC.
    pub fn forget(&mut self, name: &str, current: &Target) -> bool {
        let name = normalised_name(name);
        let name = name.as_str();
        let before = self.saved.len();
        self.saved.retain(|host| host.name != name);
        let removed = self.saved.len() != before;
        removed
            && current
                .tailnet_host()
                .is_some_and(|host| host.name() == name)
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
mod tests {
    use super::{
        ConnectionPicker, PeerSource, Row, RowKind, SavedHost, resolve_boot_target, typed_target,
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
            name: name.to_owned(),
            port,
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
            !picker.forget("a.tail1234.ts.net", &Target::Local),
            "a host that is not the current target"
        );
        assert_eq!(picker.saved(), [saved("b.tail1234.ts.net", 9443)]);
        assert_eq!(
            picker.rows(),
            [this_pc(), row(RowKind::Saved, "b.tail1234.ts.net", 9443)]
        );

        assert!(
            !picker.forget("c.tail1234.ts.net", &Target::Local),
            "an unknown host"
        );
        assert_eq!(picker.saved(), [saved("b.tail1234.ts.net", 9443)]);

        let current = Target::tailnet("b.tail1234.ts.net", Some(9443));
        assert!(
            picker.forget("b.tail1234.ts.net", &current),
            "the current host was forgotten"
        );
        assert_eq!(picker.saved(), [] as [SavedHost; 0]);
    }

    #[test]
    fn forget_matches_a_normalised_name() {
        let mut picker = ConnectionPicker::new(vec![saved("box.tail1234.ts.net", 9443)]);
        let current = Target::tailnet("box.tail1234.ts.net", Some(9443));
        assert!(
            picker.forget("Box.Tail1234.ts.net.", &current),
            "the current host was forgotten under another spelling of its name"
        );
        assert_eq!(picker.saved(), [] as [SavedHost; 0]);
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
}
