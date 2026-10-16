//! The owner rule: which tailnet peer may use the host's remote listener.
//!
//! "The owner" is a device logged in as the host node's own Tailscale user
//! (the numeric user id, never the display login), untagged, not shared in,
//! and not the host itself. Everything here is pure, so the daemon's acceptor
//! feeds it the `whois` answers for the accepted socket's peer and for the host.

use crate::model::WhoIs;
use std::net::IpAddr;

/// Why a peer, or the host, was refused. Each variant names the check that
/// failed, for the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Denied {
    #[error("peer address is outside Tailscale's ranges")]
    NotTailnetAddress,
    #[error("peer node is tagged")]
    Tagged,
    #[error("peer node belongs to another Tailscale user")]
    OtherUser,
    #[error("peer node is shared in by another user")]
    SharedIn,
    #[error("peer node is a sharee node")]
    ShareeNode,
    #[error("peer is the host node itself")]
    HostNode,
    #[error("peer address is not one of the node's own addresses")]
    NotNodeAddress,
    #[error("host node is tagged")]
    HostTagged,
    #[error("whois answer lacks a positive user id, a positive node id or a stable node id")]
    IncompleteIdentity,
}

/// The tailnet device an allowed connection came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    /// The node's `StableNodeID`.
    pub stable_id: String,
    /// The node's `MagicDNS` name without its trailing dot.
    pub name: String,
    /// The numeric Tailscale user id.
    pub user_id: i64,
    /// The user's login, for display only.
    pub login: String,
}

/// Whether `ip` is in Tailscale's address ranges, `100.64.0.0/10` or
/// `fd7a:115c:a1e0::/48`. An IPv4-mapped IPv6 address is judged as its IPv4 form.
#[must_use]
pub fn in_tailscale_range(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            a == 100 && (b & 0xC0) == 64
        }
        IpAddr::V6(v6) => {
            let [a, b, c, ..] = v6.segments();
            a == 0xfd7a && b == 0x115c && c == 0xa1e0
        }
    }
}

/// Decide whether the peer at `peer_ip`, which `tailscale whois` identified as
/// `peer`, is the owner of the host described by `host` (`tailscale whoami`).
///
/// # Errors
///
/// Returns the first check that fails: the peer's address is outside
/// Tailscale's ranges, the host is tagged, either answer lacks a positive user
/// id (or the peer a positive node id or a stable id), the peer's node is
/// tagged, belongs to another user, is shared in or a sharee node, is the host
/// itself, or the address is not one of the node's own host routes.
pub fn authorize(peer_ip: IpAddr, peer: &WhoIs, host: &WhoIs) -> Result<PeerIdentity, Denied> {
    let peer_ip = peer_ip.to_canonical();
    let node = &peer.node;
    if !in_tailscale_range(peer_ip) {
        return Err(Denied::NotTailnetAddress);
    }
    host_can_serve(host)?;
    check_owner_node(peer, host)?;
    if !node
        .addresses
        .iter()
        .any(|prefix| is_host_route(prefix, peer_ip))
    {
        return Err(Denied::NotNodeAddress);
    }
    Ok(PeerIdentity {
        stable_id: node.stable_id.clone(),
        name: node.name.trim_end_matches('.').to_owned(),
        user_id: node.user,
        login: peer.user_profile.login_name.clone(),
    })
}

/// The node checks of [`authorize`], in its order: a complete identity, then an
/// untagged node of the host's user, not shared in, not a sharee, not the host.
fn check_owner_node(peer: &WhoIs, host: &WhoIs) -> Result<(), Denied> {
    let node = &peer.node;
    if node.user <= 0 || host.node.user <= 0 || node.id <= 0 || node.stable_id.is_empty() {
        return Err(Denied::IncompleteIdentity);
    }
    if !node.tags.is_empty() {
        return Err(Denied::Tagged);
    }
    if node.user != host.node.user {
        return Err(Denied::OtherUser);
    }
    if node.sharer != 0 {
        return Err(Denied::SharedIn);
    }
    if node.hostinfo.sharee_node {
        return Err(Denied::ShareeNode);
    }
    if node.id == host.node.id {
        return Err(Denied::HostNode);
    }
    Ok(())
}

/// Whether the host may turn remote access on: a tagged host refuses, since its
/// `User` does not name the identity it runs as.
///
/// # Errors
///
/// Returns [`Denied::HostTagged`] when the host node has any tag.
pub fn host_can_serve(host: &WhoIs) -> Result<(), Denied> {
    if host.node.tags.is_empty() {
        Ok(())
    } else {
        Err(Denied::HostTagged)
    }
}

/// Whether a node's `Addresses` entry is the host route of `peer_ip`: an IPv4
/// `/32` or an IPv6 `/128` naming exactly that address. An entry with no length,
/// another length, a v4-mapped IPv6 address or text that does not parse is no
/// match; the entry is compared as written, never canonicalised.
fn is_host_route(prefix: &str, peer_ip: IpAddr) -> bool {
    let Some((address, length)) = prefix.split_once('/') else {
        return false;
    };
    let (Ok(address), Ok(length)) = (address.parse::<IpAddr>(), length.parse::<u8>()) else {
        return false;
    };
    let host_route = match address {
        IpAddr::V4(_) => length == 32,
        IpAddr::V6(v6) => length == 128 && v6.to_ipv4_mapped().is_none(),
    };
    host_route && address == peer_ip
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use crate::model::{Hostinfo, Node, UserProfile, WhoIs};
    use crate::rule::{Denied, authorize, host_can_serve, in_tailscale_range};
    use std::net::IpAddr;

    const OWNER: i64 = 61_234_567_890_123;
    const PEER_V4: &str = "100.101.102.103";
    const PEER_V6: &str = "fd7a:115c:a1e0::1234:5678";

    fn who(id: i64, name: &str, addresses: &[&str]) -> WhoIs {
        WhoIs {
            node: Node {
                id,
                stable_id: format!("n{id}CNTRL"),
                name: name.to_owned(),
                user: OWNER,
                sharer: 0,
                addresses: addresses.iter().map(|a| (*a).to_owned()).collect(),
                tags: Vec::new(),
                hostinfo: Hostinfo::default(),
            },
            user_profile: UserProfile {
                id: OWNER,
                login_name: "owner@example.com".to_owned(),
                display_name: "Owner".to_owned(),
            },
        }
    }

    fn host() -> WhoIs {
        who(
            1,
            "desktop.tail1234.ts.net.",
            &["100.90.80.70/32", "fd7a:115c:a1e0::aaaa:bbbb/128"],
        )
    }

    fn peer() -> WhoIs {
        who(
            2,
            "laptop.tail1234.ts.net.",
            &["100.101.102.103/32", "fd7a:115c:a1e0::1234:5678/128"],
        )
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("test address parses")
    }

    #[test]
    fn rejects_non_host_route_entry() {
        let peer = who(2, "laptop.tail1234.ts.net.", &["100.101.0.0/16"]);
        assert_eq!(
            authorize(ip("100.101.0.0"), &peer, &host()),
            Err(Denied::NotNodeAddress)
        );
        let peer = who(2, "laptop.tail1234.ts.net.", &["fd7a:115c:a1e0::/64"]);
        assert_eq!(
            authorize(ip("fd7a:115c:a1e0::"), &peer, &host()),
            Err(Denied::NotNodeAddress)
        );
    }

    #[test]
    fn rejects_v4_mapped_address_entry() {
        let peer = who(
            2,
            "laptop.tail1234.ts.net.",
            &["::ffff:100.101.102.103/128"],
        );
        assert_eq!(
            authorize(ip(PEER_V4), &peer, &host()),
            Err(Denied::NotNodeAddress)
        );
    }

    #[test]
    fn rejects_address_entry_without_length() {
        let peer = who(2, "laptop.tail1234.ts.net.", &[PEER_V4, PEER_V6]);
        assert_eq!(
            authorize(ip(PEER_V4), &peer, &host()),
            Err(Denied::NotNodeAddress)
        );
        assert_eq!(
            authorize(ip(PEER_V6), &peer, &host()),
            Err(Denied::NotNodeAddress)
        );
    }

    #[test]
    fn authorize_denies_when_host_is_tagged() {
        let mut host = host();
        host.node.tags = vec!["tag:server".to_owned()];
        assert_eq!(
            authorize(ip(PEER_V4), &peer(), &host),
            Err(Denied::HostTagged)
        );
    }

    #[test]
    fn rejects_non_positive_peer_user() {
        for user in [0, -1] {
            let mut peer = peer();
            peer.node.user = user;
            assert_eq!(
                authorize(ip(PEER_V4), &peer, &host()),
                Err(Denied::IncompleteIdentity),
                "peer user {user}"
            );
        }
    }

    #[test]
    fn rejects_non_positive_host_user() {
        for user in [0, -1] {
            let (mut peer, mut host) = (peer(), host());
            peer.node.user = user;
            host.node.user = user;
            assert_eq!(
                authorize(ip(PEER_V4), &peer, &host),
                Err(Denied::IncompleteIdentity),
                "host user {user}"
            );
        }
    }

    #[test]
    fn rejects_non_positive_peer_id() {
        for id in [0, -5] {
            let mut peer = peer();
            peer.node.id = id;
            assert_eq!(
                authorize(ip(PEER_V4), &peer, &host()),
                Err(Denied::IncompleteIdentity),
                "peer id {id}"
            );
        }
    }

    #[test]
    fn rejects_empty_peer_stable_id() {
        let mut peer = peer();
        peer.node.stable_id = String::new();
        assert_eq!(
            authorize(ip(PEER_V4), &peer, &host()),
            Err(Denied::IncompleteIdentity)
        );
    }

    #[test]
    fn rejects_tagged_node_created_by_owner() {
        let mut peer = peer();
        peer.node.tags = vec!["tag:server".to_owned()];
        assert_eq!(authorize(ip(PEER_V4), &peer, &host()), Err(Denied::Tagged));
    }

    #[test]
    fn rejects_other_user() {
        let mut peer = peer();
        peer.node.user = OWNER + 1;
        assert_eq!(
            authorize(ip(PEER_V4), &peer, &host()),
            Err(Denied::OtherUser)
        );
    }

    #[test]
    fn rejects_sharer_node() {
        let mut peer = peer();
        peer.node.sharer = OWNER + 7;
        assert_eq!(
            authorize(ip(PEER_V4), &peer, &host()),
            Err(Denied::SharedIn)
        );
    }

    #[test]
    fn rejects_sharee_node() {
        let mut peer = peer();
        peer.node.hostinfo.sharee_node = true;
        assert_eq!(
            authorize(ip(PEER_V4), &peer, &host()),
            Err(Denied::ShareeNode)
        );
    }

    #[test]
    fn rejects_self_node() {
        assert_eq!(
            authorize(ip("100.90.80.70"), &host(), &host()),
            Err(Denied::HostNode)
        );
    }

    #[test]
    fn rejects_peer_ip_outside_node_addresses() {
        assert_eq!(
            authorize(ip("100.101.102.104"), &peer(), &host()),
            Err(Denied::NotNodeAddress)
        );
        assert_eq!(
            authorize(ip("fd7a:115c:a1e0::1"), &peer(), &host()),
            Err(Denied::NotNodeAddress)
        );
    }

    #[test]
    fn rejects_non_tailscale_range() {
        let lan_peer = who(2, "laptop.tail1234.ts.net.", &["192.168.1.5/32"]);
        assert_eq!(
            authorize(ip("192.168.1.5"), &lan_peer, &host()),
            Err(Denied::NotTailnetAddress)
        );
        for outside in [
            "100.63.255.255",
            "100.128.0.0",
            "127.0.0.1",
            "fd7a:115c:a1e1::1",
            "::1",
        ] {
            assert!(!in_tailscale_range(ip(outside)), "{outside} is outside");
        }
        for inside in [
            "100.64.0.0",
            "100.127.255.255",
            "fd7a:115c:a1e0::",
            "fd7a:115c:a1e0:ffff::1",
        ] {
            assert!(in_tailscale_range(ip(inside)), "{inside} is inside");
        }
    }

    #[test]
    fn canonicalises_v4_mapped() {
        assert!(in_tailscale_range(ip("::ffff:100.64.0.1")));
        assert!(!in_tailscale_range(ip("::ffff:192.168.1.5")));
        let allowed = authorize(ip("::ffff:100.101.102.103"), &peer(), &host())
            .expect("mapped peer address is allowed");
        assert_eq!(allowed.stable_id, "n2CNTRL");
    }

    #[test]
    fn allows_own_untagged_device() {
        let allowed = authorize(ip(PEER_V4), &peer(), &host()).expect("own device is allowed");
        assert_eq!(allowed.stable_id, "n2CNTRL");
        assert_eq!(allowed.name, "laptop.tail1234.ts.net");
        assert_eq!(allowed.user_id, OWNER);
        assert_eq!(allowed.login, "owner@example.com");
        assert!(authorize(ip(PEER_V6), &peer(), &host()).is_ok());
    }

    #[test]
    fn tagged_host_cannot_serve() {
        assert_eq!(host_can_serve(&host()), Ok(()));
        let mut tagged = host();
        tagged.node.tags = vec!["tag:server".to_owned()];
        assert_eq!(host_can_serve(&tagged), Err(Denied::HostTagged));
    }
}
