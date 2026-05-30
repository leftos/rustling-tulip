//! Remote (LAN) client transport for the Tauri app.
//!
//! `WebView2` has no JS API to pin or trust a self-signed certificate, so a
//! `wss://` straight from the frontend to the self-signed daemon cannot work.
//! Instead the pinned-TLS connection terminates here, on the Rust side, and is
//! bridged to the frontend over plaintext loopback:
//!
//! ```text
//! frontend  ws://127.0.0.1:<bridgePort>/ws   (plaintext, unchanged JS path)
//!    │
//!    ▼  this module: copy_bidirectional
//!    │
//!    ▼  wss://<host>:<lanPort>/ws  (rustls, custom verifier pins the fingerprint)
//! remote daemon
//! ```
//!
//! The tunnel is a byte-level TLS-terminating proxy — protocol-agnostic, it
//! never parses WS frames. Hostname verification is irrelevant because the exact
//! leaf-cert fingerprint is pinned (TOFU); a fixed dummy SNI is sent.
//!
//! This module also persists paired hosts in `remote-profiles.json` so the
//! laptop can reconnect without re-pasting the connection code.

use crate::{DaemonHandshake, config_dir};
use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;
use tracing::{debug, info, warn};

/// Version tag embedded in the connection code. Must match `LAN_CODE_VERSION`
/// in the desktop settings panel (`SettingsModal.tsx`).
const CONNECTION_CODE_VERSION: u32 = 1;

/// SNI sent on the upstream TLS handshake. The daemon's self-signed cert uses
/// this as its SAN; our verifier ignores the name entirely (it pins the
/// fingerprint), so the exact value only matters for tidiness.
const TUNNEL_SNI: &str = "rustling-tulip-lan";

/// How long to wait for the eager reachability + pinning probe before giving
/// up, so a black-hole host can't hang the `connect_remote` command.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// Resolved connection parameters for a remote daemon. The frontend obtains
/// these either by decoding a freshly pasted connection code
/// (`decode_connection_code`) or from a saved [`RemoteProfile`], then hands
/// them to [`connect_remote`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionParams {
    pub host: String,
    pub port: u16,
    pub token: String,
    /// SHA-256 of the leaf certificate, lowercase hex (64 chars).
    pub fingerprint: String,
}

/// Wire shape of the base64url connection code emitted by the desktop host.
#[derive(Debug, Deserialize)]
struct ConnectionCode {
    v: u32,
    host: String,
    port: u16,
    token: String,
    fp: String,
}

/// A paired remote host, persisted in `remote-profiles.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteProfile {
    /// Stable identifier; the frontend mints one when first saving a profile.
    pub id: String,
    /// User-facing label (defaults to the host address).
    pub name: String,
    pub host: String,
    pub port: u16,
    pub token: String,
    pub fingerprint: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RemoteProfileStore {
    #[serde(default)]
    profiles: Vec<RemoteProfile>,
}

/// The single active loopback bridge. Replacing it (or dropping the managed
/// state on app exit) aborts the accept loop.
struct ActiveTunnel {
    accept_task: tokio::task::JoinHandle<()>,
}

impl Drop for ActiveTunnel {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// Tauri-managed state holding at most one active bridge. A second
/// `connect_remote` (profile switch or reconnect) swaps the previous tunnel
/// out, which aborts its accept loop.
#[derive(Default)]
pub struct RemoteState {
    active: Mutex<Option<ActiveTunnel>>,
}

/// Pins a single leaf-certificate fingerprint (TOFU). Any cert whose SHA-256
/// matches is accepted regardless of hostname, expiry, or chain; everything
/// else is rejected. Signature verification still delegates to the crypto
/// provider so the TLS session itself is sound.
#[derive(Debug)]
struct PinnedServerCertVerifier {
    expected_fingerprint: [u8; 32],
    supported_algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let actual = Sha256::digest(end_entity.as_ref());
        if actual.as_slice() == self.expected_fingerprint.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General(
                "LAN certificate fingerprint does not match the pinned value".to_string(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.supported_algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.supported_algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_algs.supported_schemes()
    }
}

fn build_client_config(fingerprint: [u8; 32]) -> anyhow::Result<rustls::ClientConfig> {
    let provider = rustls::crypto::ring::default_provider();
    let supported_algs = provider.signature_verification_algorithms;
    let verifier = Arc::new(PinnedServerCertVerifier {
        expected_fingerprint: fingerprint,
        supported_algs,
    });
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .context("configuring rustls protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(config)
}

/// Decode a base64url connection code into a parsed [`ConnectionCode`]. The
/// alphabet + stripped padding match the desktop `buildConnectionCode`.
fn decode_code(code: &str) -> anyhow::Result<ConnectionCode> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(code.trim())
        .context("connection code is not valid base64url")?;
    let parsed: ConnectionCode =
        serde_json::from_slice(&bytes).context("connection code payload is not valid JSON")?;
    if parsed.v != CONNECTION_CODE_VERSION {
        bail!(
            "unsupported connection code version {} (this app speaks version {CONNECTION_CODE_VERSION})",
            parsed.v
        );
    }
    Ok(parsed)
}

/// Parse a 64-char lowercase-hex SHA-256 fingerprint into raw bytes.
fn parse_fingerprint(hex: &str) -> anyhow::Result<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        bail!("fingerprint must be 64 hex characters, got {}", hex.len());
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).context("fingerprint contains non-UTF-8 bytes")?;
        out[i] = u8::from_str_radix(pair, 16).context("fingerprint contains non-hex characters")?;
    }
    Ok(out)
}

/// Eagerly prove the host is reachable and the cert fingerprint matches before
/// handing a bridge port back to the frontend. A failed TLS handshake here is
/// either an unreachable host or a fingerprint mismatch — both surface as a
/// clear error in the launcher instead of a silent failed WS connect.
async fn probe_remote(
    connector: &TlsConnector,
    host: &str,
    port: u16,
    server_name: &ServerName<'static>,
) -> anyhow::Result<()> {
    let attempt = async {
        let tcp = TcpStream::connect((host, port))
            .await
            .context("could not reach the remote daemon")?;
        connector.connect(server_name.clone(), tcp).await.context(
            "TLS handshake failed (host unreachable or certificate fingerprint mismatch)",
        )?;
        anyhow::Ok(())
    };
    tokio::time::timeout(PROBE_TIMEOUT, attempt)
        .await
        .context("connection to the remote daemon timed out")?
}

/// Accept loop for the loopback bridge: every inbound frontend connection gets
/// its own upstream TLS connection to the remote daemon.
async fn run_tunnel(
    listener: TcpListener,
    connector: TlsConnector,
    server_name: ServerName<'static>,
    host: String,
    port: u16,
) {
    loop {
        let inbound = match listener.accept().await {
            Ok((stream, _peer)) => stream,
            Err(err) => {
                warn!(%err, "remote bridge accept failed; tunnel stopping");
                break;
            }
        };
        let connector = connector.clone();
        let server_name = server_name.clone();
        let host = host.clone();
        tokio::spawn(async move {
            if let Err(err) = bridge_one(inbound, connector, server_name, host, port).await {
                debug!(error = format!("{err:#}"), "remote bridge connection ended");
            }
        });
    }
}

async fn bridge_one(
    mut inbound: TcpStream,
    connector: TlsConnector,
    server_name: ServerName<'static>,
    host: String,
    port: u16,
) -> anyhow::Result<()> {
    let upstream = TcpStream::connect((host.as_str(), port))
        .await
        .context("connecting to remote daemon")?;
    let _ = upstream.set_nodelay(true);
    let mut tls = connector
        .connect(server_name, upstream)
        .await
        .context("remote TLS handshake")?;
    let _ = inbound.set_nodelay(true);
    tokio::io::copy_bidirectional(&mut inbound, &mut tls)
        .await
        .context("bridging bytes between frontend and remote daemon")?;
    Ok(())
}

async fn connect_remote_inner(
    state: &RemoteState,
    params: ConnectionParams,
) -> anyhow::Result<DaemonHandshake> {
    let fingerprint = parse_fingerprint(&params.fingerprint)?;
    let config = build_client_config(fingerprint)?;
    let connector = TlsConnector::from(Arc::new(config));
    let server_name = ServerName::try_from(TUNNEL_SNI)
        .context("building TLS server name")?
        .to_owned();

    probe_remote(&connector, &params.host, params.port, &server_name).await?;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("binding loopback bridge listener")?;
    let local_port = listener
        .local_addr()
        .context("reading loopback bridge port")?
        .port();

    let accept_task = tokio::spawn(run_tunnel(
        listener,
        connector,
        server_name,
        params.host.clone(),
        params.port,
    ));

    {
        let mut active = state
            .active
            .lock()
            .map_err(|_| anyhow!("remote tunnel state lock poisoned"))?;
        // Dropping the previous tunnel aborts its accept loop.
        *active = Some(ActiveTunnel { accept_task });
    }

    info!(
        host = %params.host,
        remote_port = params.port,
        bridge_port = local_port,
        "remote bridge established"
    );

    Ok(DaemonHandshake {
        protocol_version: protocol::PROTOCOL_VERSION,
        port: local_port,
        auth_token: params.token,
        pid: 0,
    })
}

fn profiles_path() -> Result<PathBuf, String> {
    Ok(config_dir()?.join("remote-profiles.json"))
}

fn load_store() -> anyhow::Result<RemoteProfileStore> {
    let path = profiles_path().map_err(|e| anyhow!(e))?;
    if !path.exists() {
        return Ok(RemoteProfileStore::default());
    }
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).context("parsing remote-profiles.json")
}

fn save_store(store: &RemoteProfileStore) -> anyhow::Result<()> {
    let path = profiles_path().map_err(|e| anyhow!(e))?;
    let bytes = serde_json::to_vec_pretty(store).context("serializing remote-profiles.json")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Build a loopback TLS-terminating bridge to a remote daemon and return a
/// synthetic handshake the frontend dials over plaintext loopback. Replaces any
/// previously active bridge.
#[tauri::command]
pub async fn connect_remote(
    state: tauri::State<'_, RemoteState>,
    params: ConnectionParams,
) -> Result<DaemonHandshake, String> {
    connect_remote_inner(&state, params)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Tear down the active remote bridge, if any. Used when switching back to a
/// local connection so the abandoned listener doesn't linger.
#[tauri::command]
pub async fn disconnect_remote(state: tauri::State<'_, RemoteState>) -> Result<(), String> {
    let mut active = state
        .active
        .lock()
        .map_err(|_| "remote tunnel state lock poisoned".to_string())?;
    *active = None;
    Ok(())
}

/// Decode a pasted connection code into connection parameters (no network).
#[tauri::command]
pub async fn decode_connection_code(code: String) -> Result<ConnectionParams, String> {
    let decoded = decode_code(&code).map_err(|e| format!("{e:#}"))?;
    Ok(ConnectionParams {
        host: decoded.host,
        port: decoded.port,
        token: decoded.token,
        fingerprint: decoded.fp,
    })
}

/// List saved remote profiles.
#[tauri::command]
pub async fn list_remote_profiles() -> Result<Vec<RemoteProfile>, String> {
    load_store()
        .map(|s| s.profiles)
        .map_err(|e| format!("{e:#}"))
}

/// Insert or update a remote profile (keyed by `id`).
#[tauri::command]
pub async fn save_remote_profile(profile: RemoteProfile) -> Result<(), String> {
    let mut store = load_store().map_err(|e| format!("{e:#}"))?;
    if let Some(existing) = store.profiles.iter_mut().find(|p| p.id == profile.id) {
        *existing = profile;
    } else {
        store.profiles.push(profile);
    }
    save_store(&store).map_err(|e| format!("{e:#}"))
}

/// Remove a saved remote profile by `id`. Removing an unknown id is a no-op.
#[tauri::command]
pub async fn delete_remote_profile(id: String) -> Result<(), String> {
    let mut store = load_store().map_err(|e| format!("{e:#}"))?;
    store.profiles.retain(|p| p.id != id);
    save_store(&store).map_err(|e| format!("{e:#}"))
}

/// How long to listen for mDNS responses before returning the hosts found.
const DISCOVERY_WINDOW: Duration = Duration::from_millis(2500);

/// A daemon host discovered on the LAN via mDNS.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredHost {
    /// Human-readable label: the host's mDNS TXT `name`, else its instance name.
    pub name: String,
    /// First IPv4 address the host advertised.
    pub host: String,
    pub port: u16,
    /// SHA-256 of the LAN cert (lowercase hex) from the TXT record, to pin.
    pub fingerprint: String,
}

/// The leading label of an mDNS fullname (`DESKTOP-PC._rustling-tulip._tcp.local.`
/// -> `DESKTOP-PC`), used when a host didn't publish a `name` TXT record.
fn instance_label(fullname: &str) -> String {
    fullname.split('.').next().unwrap_or(fullname).to_string()
}

/// Turn a resolved mDNS service into a [`DiscoveredHost`], or `None` if it
/// lacks the fingerprint TXT record or any IPv4 address (we can't pair without
/// both).
fn resolve_host(rs: &ResolvedService) -> Option<DiscoveredHost> {
    let fingerprint = rs
        .get_property_val_str(protocol::MDNS_TXT_FINGERPRINT)?
        .to_string();
    let ip = rs.get_addresses_v4().into_iter().min()?;
    let name = rs
        .get_property_val_str(protocol::MDNS_TXT_NAME)
        .map_or_else(|| instance_label(&rs.fullname), str::to_string);
    Some(DiscoveredHost {
        name,
        host: ip.to_string(),
        port: rs.port,
        fingerprint,
    })
}

async fn discover_inner() -> anyhow::Result<Vec<DiscoveredHost>> {
    let daemon = ServiceDaemon::new().context("creating mDNS browser")?;
    let receiver = daemon
        .browse(protocol::MDNS_SERVICE_TYPE)
        .context("starting mDNS browse")?;
    // Keyed by fullname so re-announcements (multiple interfaces, repeated TTL
    // refreshes) collapse to one entry per host.
    let mut found: HashMap<String, DiscoveredHost> = HashMap::new();
    let deadline = tokio::time::Instant::now() + DISCOVERY_WINDOW;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, receiver.recv_async()).await {
            Ok(Ok(ServiceEvent::ServiceResolved(rs))) => {
                if let Some(host) = resolve_host(&rs) {
                    found.insert(rs.fullname.clone(), host);
                }
            }
            // Other events (search started, service found-but-unresolved) are
            // not actionable; keep listening until the window closes.
            Ok(Ok(_)) => {}
            // Channel closed or window elapsed: stop and return what we have.
            Ok(Err(_)) | Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    Ok(found.into_values().collect())
}

/// Browse the LAN for daemon hosts advertising over mDNS. Returns every host
/// resolved within [`DISCOVERY_WINDOW`]; an empty list means none were found
/// (mDNS blocked, no host advertising, or a slow network).
#[tauri::command]
pub async fn discover_lan_hosts() -> Result<Vec<DiscoveredHost>, String> {
    discover_inner().await.map_err(|e| format!("{e:#}"))
}

#[derive(Debug, Deserialize)]
struct PairResponseBody {
    token: String,
}

/// Split a raw HTTP/1.1 response into its status code and body bytes. Expects a
/// Content-Length / `Connection: close` framed response (what axum emits for
/// small JSON or string bodies) — not chunked transfer encoding.
fn split_http_response(raw: &[u8]) -> anyhow::Result<(u16, &[u8])> {
    let sep = b"\r\n\r\n";
    let head_end = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .context("malformed HTTP response (no header terminator)")?;
    let body = &raw[head_end + sep.len()..];
    let status_line = raw[..head_end]
        .split(|&b| b == b'\n')
        .next()
        .context("empty HTTP response")?;
    let status_line = std::str::from_utf8(status_line).context("non-UTF-8 HTTP status line")?;
    let code = status_line
        .split_whitespace()
        .nth(1)
        .context("HTTP status line missing status code")?
        .parse::<u16>()
        .context("HTTP status code is not a number")?;
    Ok((code, body))
}

async fn pair_inner(
    host: String,
    port: u16,
    fingerprint_hex: String,
    code: String,
) -> anyhow::Result<ConnectionParams> {
    let fingerprint = parse_fingerprint(&fingerprint_hex)?;
    let config = build_client_config(fingerprint)?;
    let connector = TlsConnector::from(Arc::new(config));
    let server_name = ServerName::try_from(TUNNEL_SNI)
        .context("building TLS server name")?
        .to_owned();
    let body = serde_json::to_vec(&serde_json::json!({ "code": code.trim() }))
        .context("encoding pairing request")?;

    let exchange = async {
        let tcp = TcpStream::connect((host.as_str(), port))
            .await
            .context("could not reach the remote host")?;
        let mut tls = connector
            .connect(server_name, tcp)
            .await
            .context("TLS handshake failed (host unreachable or fingerprint mismatch)")?;
        let header = format!(
            "POST /pair HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        tls.write_all(header.as_bytes())
            .await
            .context("sending pairing request")?;
        tls.write_all(&body).await.context("sending pairing body")?;
        tls.flush().await.context("flushing pairing request")?;
        let mut raw = Vec::new();
        tls.read_to_end(&mut raw)
            .await
            .context("reading pairing response")?;
        anyhow::Ok(raw)
    };
    let raw = tokio::time::timeout(PROBE_TIMEOUT, exchange)
        .await
        .context("pairing request timed out")??;

    let (status, http_body) = split_http_response(&raw)?;
    if status != 200 {
        let msg = String::from_utf8_lossy(http_body);
        bail!(
            "host rejected the pairing code (HTTP {status}): {}",
            msg.trim()
        );
    }
    let parsed: PairResponseBody =
        serde_json::from_slice(http_body).context("parsing pairing response JSON")?;
    Ok(ConnectionParams {
        host,
        port,
        token: parsed.token,
        fingerprint: fingerprint_hex,
    })
}

/// Exchange a host's short pairing code for its auth token over the pinned-TLS
/// LAN channel, returning ready-to-use [`ConnectionParams`] the caller can save
/// as a profile and connect with. `fingerprint` is the value discovered over
/// mDNS, which is pinned for this handshake.
#[tauri::command]
pub async fn pair_with_host(
    host: String,
    port: u16,
    fingerprint: String,
    code: String,
) -> Result<ConnectionParams, String> {
    pair_inner(host, port, fingerprint, code)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect for clearer failure output"
)]
mod tests {
    use super::{
        CONNECTION_CODE_VERSION, build_client_config, decode_code, instance_label,
        parse_fingerprint, split_http_response,
    };
    use base64::Engine as _;

    fn encode_code(json: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.as_bytes())
    }

    #[test]
    fn decodes_a_well_formed_connection_code() {
        let json = format!(
            r#"{{"v":{CONNECTION_CODE_VERSION},"host":"192.168.1.20","port":8787,"token":"abc","fp":"{}"}}"#,
            "ab".repeat(32)
        );
        let decoded = decode_code(&encode_code(&json)).expect("decode");
        assert_eq!(decoded.host, "192.168.1.20");
        assert_eq!(decoded.port, 8787);
        assert_eq!(decoded.token, "abc");
        assert_eq!(decoded.fp.len(), 64);
    }

    #[test]
    fn rejects_unknown_code_version() {
        let json = r#"{"v":999,"host":"h","port":1,"token":"t","fp":"ab"}"#;
        assert!(decode_code(&encode_code(json)).is_err());
    }

    #[test]
    fn rejects_non_base64_code() {
        assert!(decode_code("not valid base64 !!!").is_err());
    }

    #[test]
    fn parses_valid_fingerprint() {
        let hex = "ab".repeat(32);
        let bytes = parse_fingerprint(&hex).expect("parse");
        assert_eq!(bytes, [0xab_u8; 32]);
    }

    #[test]
    fn rejects_wrong_length_fingerprint() {
        assert!(parse_fingerprint("abcd").is_err());
    }

    #[test]
    fn rejects_non_hex_fingerprint() {
        let bad = "zz".repeat(32);
        assert!(parse_fingerprint(&bad).is_err());
    }

    #[test]
    fn instance_label_takes_leading_dns_label() {
        assert_eq!(
            instance_label("DESKTOP-PC._rustling-tulip._tcp.local."),
            "DESKTOP-PC"
        );
        assert_eq!(instance_label("solo"), "solo");
    }

    #[test]
    fn splits_a_200_response_into_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 16\r\n\r\n{\"token\":\"abc\"}\n";
        let (status, body) = split_http_response(raw).expect("split");
        assert_eq!(status, 200);
        assert_eq!(body, b"{\"token\":\"abc\"}\n");
    }

    #[test]
    fn splits_an_error_response_status() {
        let raw = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 19\r\n\r\ninvalid pairing code";
        let (status, body) = split_http_response(raw).expect("split");
        assert_eq!(status, 403);
        assert_eq!(body, b"invalid pairing code");
    }

    #[test]
    fn rejects_a_response_without_a_header_terminator() {
        assert!(split_http_response(b"HTTP/1.1 200 OK\r\n").is_err());
    }

    #[test]
    fn builds_a_client_config_with_pinned_verifier() {
        // Smoke test: the ring provider + custom verifier wiring compiles and
        // produces a usable config. Building it exercises
        // `with_safe_default_protocol_versions` + the dangerous verifier path,
        // which is where a provider/feature mismatch would blow up.
        let config = build_client_config([0u8; 32]).expect("build config");
        assert!(
            config.alpn_protocols.is_empty(),
            "no ALPN protocols are configured by default"
        );
    }

    #[test]
    fn pinned_verifier_accepts_matching_cert_and_rejects_a_different_one() {
        use super::PinnedServerCertVerifier;
        use rustls::client::danger::ServerCertVerifier as _;
        use rustls::pki_types::{ServerName, UnixTime};
        use sha2::{Digest as _, Sha256};

        // A throwaway self-signed cert stands in for the daemon's LAN cert.
        let generated =
            rcgen::generate_simple_self_signed(vec!["test".to_string()]).expect("generate cert");
        let der = generated.cert.der().clone();
        let digest = Sha256::digest(der.as_ref());
        let mut expected = [0u8; 32];
        expected.copy_from_slice(&digest);

        let algs = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        let name = ServerName::try_from("test").expect("server name");
        // Fixed timestamp keeps the test deterministic (Date.now is unavailable
        // in some sandboxes and irrelevant — we never check expiry).
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_700_000_000));

        let matching = PinnedServerCertVerifier {
            expected_fingerprint: expected,
            supported_algs: algs,
        };
        assert!(
            matching
                .verify_server_cert(&der, &[], &name, &[], now)
                .is_ok(),
            "cert with the pinned fingerprint must be accepted"
        );

        let mut wrong = expected;
        wrong[0] ^= 0xff;
        let mismatched = PinnedServerCertVerifier {
            expected_fingerprint: wrong,
            supported_algs: algs,
        };
        assert!(
            mismatched
                .verify_server_cert(&der, &[], &name, &[], now)
                .is_err(),
            "cert whose fingerprint differs from the pin must be rejected"
        );
    }
}
