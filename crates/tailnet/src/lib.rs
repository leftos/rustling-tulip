//! Tailscale identity for the daemon's remote listener.
//!
//! The crate runs the `tailscale` CLI ([`Cli`]: `whois`, `whoami`, `status`,
//! `cert`) and decides whether a connecting tailnet peer is the owner
//! ([`authorize`]): a device logged in as the host node's own Tailscale user,
//! untagged, not shared in, and not the host itself.
//!
//! Every failure is a typed error ([`TailnetError`], [`Denied`]), and a caller
//! treats every error as "deny": a peer is admitted only on an `Ok`.

mod cli;
mod model;
mod rule;

pub use cli::{CERT_TIMEOUT, Cli, DEFAULT_TIMEOUT, TailnetError};
pub use model::{Hostinfo, Node, PeerStatus, Status, UserProfile, WhoIs};
pub use rule::{Denied, PeerIdentity, authorize, host_can_serve, in_tailscale_range};
