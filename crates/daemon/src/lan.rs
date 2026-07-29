//! Opt-in LAN access: persistent configuration for the optional `0.0.0.0` TLS
//! listener.
//!
//! Loopback access (the default `127.0.0.1` `ws://` listener) is unaffected by
//! anything here — this module only governs the opt-in remote listener. The
//! auth token is persisted in `lan.json` (rather than regenerated every daemon
//! start) so a paired remote client's saved profile survives daemon restarts.
//! The self-signed certificate and TLS listener built on top of this config
//! live alongside it once LAN access is wired up.

use crate::paths::Dirs;
use anyhow::Context as _;
use rand::Rng as _;
use rand::distributions::Alphanumeric;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default port for the LAN TLS listener. Fixed (not ephemeral) so a remote
/// client's saved profile keeps pointing at the right port across restarts.
pub const DEFAULT_LAN_PORT: u16 = 8787;

const AUTH_TOKEN_LEN: usize = 48;

/// Backoff between LAN bind attempts while the previous daemon releases the
/// port.
const LAN_BIND_RETRY_BACKOFF: Duration = Duration::from_millis(100);
/// How long to keep retrying a LAN bind that fails with `AddrInUse`. Sized to
/// outlast the previous daemon's graceful-shutdown drain (3s) plus socket
/// teardown, so a fast supervisor handoff doesn't leave the new daemon's LAN
/// listener permanently down.
const LAN_BIND_RETRY_BUDGET: Duration = Duration::from_secs(10);

/// Persisted contents of `lan.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanConfig {
    /// Whether the LAN TLS listener should be bound. Off by default — the
    /// daemon stays loopback-only until the user explicitly enables this.
    pub enabled: bool,
    /// TCP port for the LAN TLS listener.
    pub port: u16,
    /// Persistent auth token. Stored here so a paired remote client survives
    /// daemon restarts; the loopback `daemon.json` handshake reflects the same
    /// value, so local clients are unaffected.
    pub auth_token: String,
}

/// Generate a fresh 48-char alphanumeric auth token (same shape as the legacy
/// per-start token).
#[must_use]
pub fn generate_auth_token() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(AUTH_TOKEN_LEN)
        .map(char::from)
        .collect()
}

/// Load `lan.json` if present and well-formed. Returns `None` when the file is
/// absent or corrupt; a corrupt file is logged and treated as absent so a bad
/// config never blocks daemon startup (loopback keeps working regardless).
#[must_use]
pub fn load(dirs: &Dirs) -> Option<LanConfig> {
    if !dirs.lan_config_file.exists() {
        return None;
    }
    let bytes = match std::fs::read(&dirs.lan_config_file) {
        Ok(b) => b,
        Err(err) => {
            tracing::warn!(
                ?err,
                "could not read lan.json; ignoring (LAN access disabled)"
            );
            return None;
        }
    };
    match serde_json::from_slice::<LanConfig>(&bytes) {
        Ok(cfg) => Some(cfg),
        Err(err) => {
            tracing::warn!(?err, "lan.json corrupt; ignoring (LAN access disabled)");
            None
        }
    }
}

/// Persist `lan.json` atomically (tmp + rename), mirroring `state.json`.
pub fn save(dirs: &Dirs, cfg: &LanConfig) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(cfg).context("serializing lan.json")?;
    let tmp = dirs.lan_config_file.with_extension("json.tmp");
    // Carries the auth token — owner-only, applied to the tmp path so the
    // rename lands an already-restricted file.
    crate::secret::write_private(&tmp, &bytes).context("writing lan.json tmp")?;
    std::fs::rename(&tmp, &dirs.lan_config_file).context("renaming lan.json")?;
    Ok(())
}

/// A loaded TLS identity for the LAN listener: the rustls server config plus
/// the SHA-256 fingerprint of the leaf certificate (lowercase hex, no
/// separators) that remote clients pin on first pairing (TOFU).
pub struct LanCert {
    pub server_config: Arc<rustls::ServerConfig>,
    pub fingerprint: String,
}

/// Load the persisted self-signed cert + key, generating and persisting them
/// on first use. The same cert is reused across restarts so the pinned
/// fingerprint stays stable. Built on the `ring` provider explicitly so it
/// works regardless of any process-default crypto provider.
pub fn ensure_cert(dirs: &Dirs) -> anyhow::Result<LanCert> {
    // An unreadable pair regenerates rather than propagating. The cert and key
    // are written as two separate files, so a crash during the key write leaves
    // both present with the key truncated — and propagating there would wedge
    // the LAN listener permanently, since nothing else ever deletes them.
    // Regenerating changes the fingerprint, so already-paired clients must pair
    // again; that is recoverable, a permanently dead listener is not.
    let persisted = if dirs.lan_cert_file.exists() && dirs.lan_key_file.exists() {
        match load_persisted_cert(dirs) {
            Ok(pair) => Some(pair),
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "LAN cert/key unreadable; regenerating — paired clients must re-pair"
                );
                None
            }
        }
    } else {
        None
    };
    let (cert, key) = match persisted {
        Some(pair) => pair,
        None => generate_and_persist_cert(dirs)?,
    };
    let fingerprint = fingerprint_hex(&cert);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server_config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("configuring rustls protocol versions")?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .context("building rustls server config from LAN cert")?;
    Ok(LanCert {
        server_config: Arc::new(server_config),
        fingerprint,
    })
}

fn generate_and_persist_cert(
    dirs: &Dirs,
) -> anyhow::Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["rustling-tulip-lan".to_string()])
            .context("generating self-signed LAN certificate")?;
    // The certificate is public (clients pin its fingerprint); the key is not.
    std::fs::write(&dirs.lan_cert_file, cert.pem().as_bytes()).context("writing lan-cert.pem")?;
    crate::secret::write_private(&dirs.lan_key_file, signing_key.serialize_pem().as_bytes())
        .context("writing lan-key.pem")?;
    let cert_der = cert.der().clone();
    let key_der: PrivateKeyDer<'static> =
        PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into();
    Ok((cert_der, key_der))
}

fn load_persisted_cert(
    dirs: &Dirs,
) -> anyhow::Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    use rustls::pki_types::pem::PemObject as _;
    let cert = CertificateDer::from_pem_file(&dirs.lan_cert_file)
        .context("reading/parsing lan-cert.pem")?;
    let key =
        PrivateKeyDer::from_pem_file(&dirs.lan_key_file).context("reading/parsing lan-key.pem")?;
    Ok((cert, key))
}

fn fingerprint_hex(cert: &CertificateDer<'_>) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(cert.as_ref());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// SHA-256 fingerprint (lowercase hex) of the persisted LAN cert, or `None`
/// when LAN has never been enabled (no cert on disk). Used to populate
/// `LanStatus` without requiring the listener to be running.
#[must_use]
pub fn fingerprint(dirs: &Dirs) -> Option<String> {
    if !dirs.lan_cert_file.exists() {
        return None;
    }
    match load_persisted_cert(dirs) {
        Ok((cert, _)) => Some(fingerprint_hex(&cert)),
        Err(err) => {
            tracing::warn!(?err, "could not read LAN cert for fingerprint");
            None
        }
    }
}

/// The daemon host's non-loopback, non-link-local IPv4 addresses, for building
/// the remote connection code. Empty (logged) if enumeration fails.
#[must_use]
pub fn detect_addresses() -> Vec<String> {
    match local_ip_address::list_afinet_netifas() {
        Ok(netifs) => netifs
            .into_iter()
            .filter_map(|(_, ip)| match ip {
                std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => {
                    Some(v4.to_string())
                }
                _ => None,
            })
            .collect(),
        Err(err) => {
            tracing::warn!(?err, "could not enumerate local IP addresses");
            Vec::new()
        }
    }
}

/// Bind a TCP listener for the LAN TLS server, retrying while the address is
/// still held by the previous daemon (`AddrInUse`).
///
/// On a supervisor handoff the outgoing daemon keeps `:<port>` bound for the
/// duration of its graceful-shutdown drain, so a single bind attempt by the
/// incoming daemon loses the race and the LAN listener would silently never
/// come up until the next restart. Retrying with backoff bridges that window.
/// Non-`AddrInUse` errors fail immediately. The returned listener is set
/// nonblocking, ready for [`axum_server::from_tcp_rustls`].
pub async fn bind_listener(addr: SocketAddr) -> anyhow::Result<std::net::TcpListener> {
    bind_listener_inner(addr, LAN_BIND_RETRY_BUDGET, LAN_BIND_RETRY_BACKOFF).await
}

async fn bind_listener_inner(
    addr: SocketAddr,
    budget: Duration,
    backoff: Duration,
) -> anyhow::Result<std::net::TcpListener> {
    let start = Instant::now();
    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        match std::net::TcpListener::bind(addr) {
            Ok(listener) => {
                listener
                    .set_nonblocking(true)
                    .context("setting LAN listener nonblocking")?;
                if attempts > 1 {
                    tracing::info!(
                        port = addr.port(),
                        attempts,
                        "LAN listener bound after retrying past the previous daemon"
                    );
                }
                return Ok(listener);
            }
            Err(err) if is_addr_in_use(&err) && start.elapsed() < budget => {
                tracing::debug!(port = addr.port(), "LAN port still in use; retrying bind");
                tokio::time::sleep(backoff).await;
            }
            Err(err) if is_addr_in_use(&err) => {
                return Err(anyhow::Error::new(err)).with_context(|| {
                    format!(
                        "LAN port {} still in use after {}s; another process may hold it or the \
                         previous daemon never released it",
                        addr.port(),
                        budget.as_secs()
                    )
                });
            }
            Err(err) => {
                return Err(anyhow::Error::new(err))
                    .with_context(|| format!("binding LAN listener on {addr}"));
            }
        }
    }
}

/// Whether a bind error means the port is only momentarily held (retry) rather
/// than a genuine, permanent failure (give up).
fn is_addr_in_use(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::AddrInUse
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use super::*;
    use crate::paths::Dirs;

    fn scratch_dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("rt-lan-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch dir");
        Dirs {
            config: root.clone(),
            state_file: root.join("state.json"),
            handshake_file: root.join("daemon.json"),
            lan_config_file: root.join("lan.json"),
            lan_cert_file: root.join("lan-cert.pem"),
            lan_key_file: root.join("lan-key.pem"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
        }
    }

    #[test]
    fn lan_config_round_trips() {
        let dirs = scratch_dirs("cfg");
        assert!(load(&dirs).is_none(), "absent config loads as None");
        let cfg = LanConfig {
            enabled: true,
            port: 9001,
            auth_token: "round-trip-token".to_string(),
        };
        save(&dirs, &cfg).expect("save lan.json");
        let loaded = load(&dirs).expect("load lan.json");
        assert!(loaded.enabled);
        assert_eq!(loaded.port, 9001);
        assert_eq!(loaded.auth_token, "round-trip-token");
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn truncated_key_regenerates_instead_of_wedging() {
        // The cert and key are two separate writes, so a crash during the key
        // write leaves both files present with the key unusable. Propagating
        // that error would leave the LAN listener permanently unable to start,
        // since nothing else deletes the pair.
        let dirs = scratch_dirs("cert-truncated");
        let first = ensure_cert(&dirs).expect("generate cert").fingerprint;
        // Deliberately not a realistic PEM header: `from_pem_file` rejects this
        // just as well, and a literal BEGIN-PRIVATE-KEY line in a source file
        // trips the detect-private-key hook.
        std::fs::write(&dirs.lan_key_file, b"not a usable key").expect("truncate key");
        let second = ensure_cert(&dirs)
            .expect("a corrupt key must regenerate, not error")
            .fingerprint;
        assert_eq!(second.len(), 64);
        assert_ne!(first, second, "regeneration mints a fresh identity");
        // ...and the regenerated pair is itself reloadable.
        let third = ensure_cert(&dirs)
            .expect("reload regenerated cert")
            .fingerprint;
        assert_eq!(second, third);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn cert_fingerprint_is_stable_across_reload() {
        let dirs = scratch_dirs("cert");
        assert!(
            fingerprint(&dirs).is_none(),
            "no fingerprint before a cert exists"
        );
        // First call generates + persists; second loads the persisted cert.
        // The fingerprint must be identical so pinned remote clients keep
        // validating across daemon restarts.
        let first = ensure_cert(&dirs).expect("generate cert").fingerprint;
        let second = ensure_cert(&dirs).expect("reload cert").fingerprint;
        assert_eq!(first, second, "fingerprint stable across reload");
        assert_eq!(first.len(), 64, "sha-256 hex is 64 chars");
        assert_eq!(fingerprint(&dirs), Some(first));
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn classifies_addr_in_use() {
        assert!(is_addr_in_use(&std::io::Error::from(
            std::io::ErrorKind::AddrInUse
        )));
        assert!(!is_addr_in_use(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
    }

    #[tokio::test]
    async fn bind_listener_succeeds_on_free_port() {
        let addr = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0));
        let listener = bind_listener(addr).await.expect("bind a free port");
        assert!(
            listener.local_addr().expect("local addr").port() > 0,
            "OS assigned an ephemeral port"
        );
    }

    #[tokio::test]
    async fn bind_listener_gives_up_when_port_stays_busy() {
        // Hold a port for the whole attempt so every retry sees AddrInUse.
        let occupied = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("grab a port to occupy");
        let addr = occupied.local_addr().expect("addr of occupied port");
        // Short budget so the test doesn't wait the production 10s.
        let result =
            bind_listener_inner(addr, Duration::from_millis(250), Duration::from_millis(25)).await;
        assert!(
            result.is_err(),
            "bind must fail while the port stays occupied"
        );
        drop(occupied);
    }
}
