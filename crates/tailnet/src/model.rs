//! The subset of the `tailscale` CLI's JSON this crate reads.
//!
//! Unknown fields are ignored, since Tailscale documents the JSON as subject to
//! change. A field Tailscale omits when it is zero or empty (`Sharer`, `Tags`,
//! `Hostinfo`, `ShareeNode`) defaults; the fields the owner rule compares
//! (`ID`, `StableID`, `User`, `Addresses`) are required, so a response missing
//! one fails to parse instead of reading as zero.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::IpAddr;

/// The output of `tailscale whois --json` and `tailscale whoami --json`.
///
/// Tailscale's `WhoIsResponse` (`client/tailscale/apitype/apitype.go`); the CLI
/// adds `CurrentTailnet`, which this crate does not read.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WhoIs {
    #[serde(rename = "Node")]
    pub node: Node,
    #[serde(rename = "UserProfile", default)]
    pub user_profile: UserProfile,
}

/// A tailnet node: Tailscale's `tailcfg.Node` (`tailcfg/tailcfg.go`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Node {
    /// The numeric node id (`tailcfg.NodeID`).
    #[serde(rename = "ID")]
    pub id: i64,
    /// The stable node id (`tailcfg.StableNodeID`), e.g. `nLaptopCNTRL`.
    #[serde(rename = "StableID")]
    pub stable_id: String,
    /// The node's `MagicDNS` name, with a trailing dot.
    #[serde(rename = "Name", default)]
    pub name: String,
    /// The user who created the node (`tailcfg.UserID`). For a tagged node it
    /// does not reflect the identity the node runs as.
    #[serde(rename = "User")]
    pub user: i64,
    /// The user who shared this node in, if different from `user`; 0 when unset.
    #[serde(rename = "Sharer", default)]
    pub sharer: i64,
    /// The node's own addresses as CIDR prefixes, e.g. `100.101.102.103/32`.
    #[serde(rename = "Addresses")]
    pub addresses: Vec<String>,
    /// ACL tags such as `tag:server`; absent when the node has none.
    #[serde(rename = "Tags", default)]
    pub tags: Vec<String>,
    #[serde(rename = "Hostinfo", default)]
    pub hostinfo: Hostinfo,
}

/// The part of Tailscale's `tailcfg.Hostinfo` this crate reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Hostinfo {
    /// The node is in the netmap because a shared-to user owns it.
    #[serde(rename = "ShareeNode", default)]
    pub sharee_node: bool,
}

/// A Tailscale user: `tailcfg.UserProfile`. Its login is for display only;
/// identity comparisons use the numeric `Node.User`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct UserProfile {
    #[serde(rename = "ID", default)]
    pub id: i64,
    #[serde(rename = "LoginName", default)]
    pub login_name: String,
    #[serde(rename = "DisplayName", default)]
    pub display_name: String,
}

/// The output of `tailscale status --json`: Tailscale's `ipnstate.Status`
/// (`ipn/ipnstate/ipnstate.go`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Status {
    /// This node; absent before the backend has a netmap.
    #[serde(rename = "Self", default)]
    pub self_node: Option<PeerStatus>,
    /// The other nodes, keyed by node public key (`nodekey:…`).
    #[serde(rename = "Peer", default)]
    pub peer: BTreeMap<String, PeerStatus>,
}

/// One node in `status --json`: Tailscale's `ipnstate.PeerStatus`
/// (`ipn/ipnstate/ipnstate.go`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct PeerStatus {
    /// The stable node id (`tailcfg.StableNodeID`); `status` carries the numeric
    /// id separately as `NodeID`.
    #[serde(rename = "ID", default)]
    pub id: String,
    /// The OS hostname, not a DNS name and not necessarily unique.
    #[serde(rename = "HostName", default)]
    pub host_name: String,
    /// The `MagicDNS` name, with a trailing dot.
    #[serde(rename = "DNSName", default)]
    pub dns_name: String,
    #[serde(rename = "TailscaleIPs", default)]
    pub tailscale_ips: Vec<IpAddr>,
    #[serde(rename = "UserID", default)]
    pub user_id: i64,
    #[serde(rename = "Tags", default)]
    pub tags: Vec<String>,
    /// Connected to the control plane.
    #[serde(rename = "Online", default)]
    pub online: bool,
    #[serde(rename = "ShareeNode", default)]
    pub sharee_node: bool,
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use crate::model::{Status, WhoIs};

    const WHOIS: &str = include_str!("fixtures/whois.json");
    const STATUS: &str = include_str!("fixtures/status.json");

    #[test]
    fn parses_whois_fixture() {
        let who: WhoIs = serde_json::from_str(WHOIS).expect("whois fixture parses");
        assert_eq!(who.node.id, 7_123_456_789_012_345);
        assert_eq!(who.node.stable_id, "nLaptopCNTRL");
        assert_eq!(who.node.name, "laptop.tail1234.ts.net.");
        assert_eq!(who.node.user, 61_234_567_890_123);
        assert_eq!(who.node.sharer, 0);
        assert_eq!(
            who.node.addresses,
            ["100.101.102.103/32", "fd7a:115c:a1e0::1234:5678/128"]
        );
        assert_eq!(who.node.tags, Vec::<String>::new());
        assert!(!who.node.hostinfo.sharee_node);
        assert_eq!(who.user_profile.id, 61_234_567_890_123);
        assert_eq!(who.user_profile.login_name, "owner@example.com");
        assert_eq!(who.user_profile.display_name, "Owner Name");
    }

    #[test]
    fn parses_status_fixture() {
        let status: Status = serde_json::from_str(STATUS).expect("status fixture parses");
        let me = status.self_node.expect("Self is present");
        assert_eq!(me.id, "nHostCNTRL");
        assert_eq!(me.dns_name, "desktop.tail1234.ts.net.");
        assert_eq!(me.tailscale_ips.len(), 2);
        assert_eq!(status.peer.len(), 2);
        let laptop = status
            .peer
            .values()
            .find(|p| p.host_name == "laptop")
            .expect("laptop peer is present");
        assert_eq!(laptop.user_id, 61_234_567_890_123);
        assert!(laptop.online);
        assert_eq!(laptop.tags, Vec::<String>::new());
        assert!(!laptop.sharee_node);
        let server = status
            .peer
            .values()
            .find(|p| p.host_name == "build-box")
            .expect("build-box peer is present");
        assert_eq!(server.tags, ["tag:server"]);
        assert!(server.sharee_node);
        assert!(!server.online);
    }

    #[test]
    fn missing_or_null_identity_fields_fail_to_parse() {
        let base: serde_json::Value = serde_json::from_str(WHOIS).expect("fixture is JSON");
        for field in ["ID", "StableID", "User", "Addresses"] {
            let mut missing = base.clone();
            missing["Node"]
                .as_object_mut()
                .expect("Node is an object")
                .remove(field);
            assert!(
                serde_json::from_value::<WhoIs>(missing).is_err(),
                "a whois without {field} must not parse"
            );
            let mut null = base.clone();
            null["Node"][field] = serde_json::Value::Null;
            assert!(
                serde_json::from_value::<WhoIs>(null).is_err(),
                "a whois with a null {field} must not parse"
            );
        }
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let json = r#"{
            "Node": {
                "ID": 1, "StableID": "n1", "Name": "a.ts.net.", "User": 2,
                "Addresses": ["100.64.0.1/32"], "FutureNodeField": {"x": [1, 2]},
                "Hostinfo": {"ShareeNode": true, "FutureHostinfoField": 3}
            },
            "UserProfile": {"ID": 2, "LoginName": "a@b", "FutureProfileField": "x"},
            "FutureTopField": null
        }"#;
        let who: WhoIs = serde_json::from_str(json).expect("unknown fields are ignored");
        assert_eq!(who.node.user, 2);
        assert!(who.node.hostinfo.sharee_node);
        let status: Status = serde_json::from_str(r#"{"Peer": {}, "FutureStatusField": 1}"#)
            .expect("unknown status fields are ignored");
        assert!(status.self_node.is_none());
    }
}
