//! mDNS advertising for LAN discovery.
//!
//! When the opt-in LAN TLS listener is bound, the daemon advertises a
//! `_rustling-tulip._tcp.local.` service so a laptop on the same network can
//! find this host without anyone hand-copying an address. The cert fingerprint
//! rides along in a TXT record: a discovering client pins it to open the
//! pairing TLS channel, and the short pairing code exchanged over that channel
//! is what actually authorizes the auth-token transfer. Advertising starts and
//! stops together with the LAN listener (see `server::start_lan_listener`).

use anyhow::Context as _;
use mdns_sd::{ServiceDaemon, ServiceInfo};

/// mDNS service type the daemon advertises and the Tauri app browses for.
pub const SERVICE_TYPE: &str = "_rustling-tulip._tcp.local.";

/// A live mDNS registration. Dropping it unregisters the service and shuts the
/// responder thread down, so the LAN listener and its advertisement share a
/// lifetime.
pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

/// Begin advertising the LAN listener over mDNS.
///
/// `fingerprint` is the SHA-256 (lowercase hex) of the LAN cert, published in a
/// TXT record so a discovering client can pin it before pairing. `addresses`
/// are the host's known non-loopback IPv4 addresses; address auto-refresh is
/// also enabled so interface changes after startup are picked up.
pub fn advertise(port: u16, fingerprint: &str, addresses: &[String]) -> anyhow::Result<Advertiser> {
    let daemon = ServiceDaemon::new().context("creating mDNS responder")?;
    let hostname = sysinfo::System::host_name().unwrap_or_else(|| "rustling-tulip".to_string());
    let host_target = format!("{}.local.", sanitize_label(&hostname));
    let joined = addresses.join(",");
    let props = [("fp", fingerprint), ("name", hostname.as_str()), ("v", "1")];
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        &hostname,
        &host_target,
        joined.as_str(),
        port,
        &props[..],
    )
    .context("building mDNS service info")?
    .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    daemon.register(info).context("registering mDNS service")?;
    tracing::info!(%fullname, port, "mDNS advertising started");
    Ok(Advertiser { daemon, fullname })
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        // Both calls just hand a command to the responder thread and return a
        // status receiver we don't await — non-blocking, safe in Drop.
        if let Err(err) = self.daemon.unregister(&self.fullname) {
            tracing::debug!(?err, "mDNS unregister failed (responder may be gone)");
        }
        if let Err(err) = self.daemon.shutdown() {
            tracing::debug!(?err, "mDNS responder shutdown failed");
        }
    }
}

/// Reduce a hostname to a safe DNS label: keep ASCII alphanumerics and hyphens,
/// collapse everything else to a hyphen, trim leading/trailing hyphens. Falls
/// back to a constant when nothing usable remains.
fn sanitize_label(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches('-');
    if trimmed.is_empty() {
        "rustling-tulip".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_label;

    #[test]
    fn sanitizes_hostnames_to_dns_labels() {
        assert_eq!(sanitize_label("DESKTOP-PC"), "DESKTOP-PC");
        assert_eq!(sanitize_label("my pc!"), "my-pc");
        assert_eq!(sanitize_label("a.b.c"), "a-b-c");
        assert_eq!(sanitize_label("--"), "rustling-tulip");
        assert_eq!(sanitize_label(""), "rustling-tulip");
    }
}
