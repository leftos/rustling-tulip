# Remote access over Tailscale

Design for Wave 3's remote desktop (Linear RT-33, RT-34 and their sub-issues). Remote use of the daemon is **Tailscale-first**: Tailscale is required, and the daemon's only remote listener serves its tailnet. Local use (loopback, the `daemon.json` token) is unchanged. The iPhone PWA (RT-143, `docs/research/pwa-iphone.md`) reaches the same listener.

## Owner rulings

- Tailscale is a requirement for remote use. The daemon serves TLS with its `tailscale cert` (`*.ts.net`, public CA), one listener for the desktop client and the PWA; clients verify the certificate normally and nothing is pinned.
- A connecting device is authorized by `tailscale whois` **plus a per-device token** stored on the device. No pairing codes. A device without a token (a new device, or the PWA after Safari evicts its storage) asks for one, and the token is issued only once the owner **approves that device on the host PC**: whois alone never mints a token, so another program or Windows account on an owner device cannot get one unseen. Approval is a **QR code** the host PC shows, which the device scans (the iPhone PWA with its camera), with a short **pairing code** beside it that a device with no camera types instead; the QR code and the code are one-time, short-lived and good only for a whois-authorized device. A wiped phone is approved again.
- "The owner" is a device logged in as the host node's own Tailscale user (numeric `UserID`, never the display login), untagged, not shared in, and not the host itself.
- The listener binds the host's own tailnet addresses only (v4 and v6), on the port `remote.json` configures (default 8787), rebinding when they change.
- Shutdown and turning remote access off are refused from tailnet connections: host-only.
- Revocation is a block list in rustling-tulip: Settings lists the tailnet devices that have connected, and blocking one drops its live connections at once (RT-96, rewritten).
- There is no LAN listener, `/pair` or mDNS: the daemon binds loopback and, with remote access on, the host's own tailnet addresses. The six wire variants (`ConfigureLan`, `StartPairing`, `CancelPairing`, `LanStatus`, `PairingStarted`, `PairingEnded`) stay decodable and inert.
- Placement: the daemon side (TS.1 to TS.5) lands on `main`, behind a `remote.json` switch that is off by default; the native client's remote mode (P6.1 to P6.4 and the tests) is built on `feat/remote-mode` and merged by `/ship` after a green full suite. The installer and the docs pass land on `main` after the merge.
- The native client dials TLS itself inside `net.rs`; there is no loopback bridge.
- The relay (RT-94), pinned TLS, mDNS discovery, pairing by code and add-by-pasted-code are dropped. RT-95 (Tailscale by MagicDNS name) folds into the host picker.
- **No Windows Firewall rule** (RT-157): the owner runs Fort Firewall, whose rule must cover the content-addressed binaries-cache folder (`%LOCALAPPDATA%\leftos\rustling-tulip\data\binaries\`), not one exe path, since each build's daemon and tracer are new files; P6.7's installer needs no elevated port rule.

## Orchestrator decisions (not owner rulings; open to the owner)

- A denied peer is dropped before the TLS handshake, never answered with a 403.
- whois goes through the `tailscale` CLI (`whois --json`, `whoami --json`, `cert`, `status --json`), run with `CREATE_NO_WINDOW`; the binary is `RUSTLING_TULIP_TAILSCALE`, else `%ProgramFiles%\Tailscale\tailscale.exe`, else PATH. The LocalAPI is not used: its Windows pipe path is undocumented.
- Certificates: checked at start and every 24 h with `tailscale cert --min-validity` of about 14 days, reloaded only when the files' SHA-256 changed, by building the daemon's own rustls `ServerConfig` (ALPN `http/1.1` only, since `RustlsConfig::reload_from_pem_file` forces h2) and swapping it with `RustlsConfig::reload_from_config`; the key lives under the machine-local data dir.
- The host picker reads `tailscale status --json` and also accepts a typed MagicDNS name, since the JSON is documented as subject to change.
- Remote tests: in-process tests with a fake whois and a test CA, a fake-TLS daemon for the client, and a hand checklist on real Tailscale (two machines and the iPhone). No production setting widens the allowed address ranges.

## Invariants and the sites that obey them

1. **Only tailnet peers reach the remote listener.** Bind the tailnet addresses from `whoami`; the acceptor rejects any peer outside `100.64.0.0/10` or `fd7a:115c:a1e0::/48` (IPv4-mapped addresses canonicalised) before whois. The daemon never binds `0.0.0.0` (`Ipv4Addr::UNSPECIFIED`): its only other listener is loopback. Site: new `crates/daemon/src/tailnet_listener.rs`.
2. **whois is asked about the real peer and fails closed.** The argument is the accepted socket's `peer_addr()`, never a header or a `Hello` field. Timeout (2 to 3 s), non-zero exit, unparsable JSON, a missing binary and the Windows "server mode" 401 all reject. Verdicts are cached per IP for at most 30 s. Sites: the custom `axum_server::accept::Accept` in the listener module; `crates/tailnet/src/cli.rs`.
3. **The owner rule** is a pure function: allow only when whois succeeded, the peer node has no tags, its `UserID` equals the host node's, it is neither a sharer's nor a sharee node, its `ID` differs from the host's (this rejects other Windows users and anything proxied on the host), and the peer IP is one of the node's own `Addresses` (this rejects subnet-routed sources). A tagged host binds nothing and asks for no certificate; the status says `host_tagged`. Site: `crates/tailnet/src/rule.rs`.
4. **The listener decides how a connection authenticates.** Loopback connections compare the `daemon.json` token; tailnet connections never look at it, and they present their device token instead. The loopback listener serves `build_loopback_router`; the tailnet listener has its own `build_tailnet_router`, and `/shutdown` is never on it. No router serves `/pair`. Site: `server/connection.rs` `AuthKind`, `server/handshake.rs` `handshake`, `server/remote.rs` `tailnet_ws_handler` and `build_tailnet_router`, `server.rs` `ws_handler` and `build_loopback_router`.
5. **A credential never goes to another host.** The native client sends the `daemon.json` token only to the loopback daemon; on a tailnet target it sends its device token for that host. No status message carries a token. Site: `apps/native/src/net.rs` (`attempt`).
6. **Browsers cannot act for an attacker.** The host's `*.ts.net` name is public in Certificate Transparency logs, so the tailnet `/ws` upgrade requires `Origin` absent (native) or exactly `https://{name}:{port}` (the PWA), on the configured port (default 8787), and `Host` equal to the own name. No CORS layer on the tailnet router; any future state-changing HTTP route requires same-origin.
7. **TLS only, with the `tailscale cert` certificate.** No plaintext path, no self-signed fallback; with HTTPS disabled in the admin console the listener stays down and the status says why.
8. **Tailscale's state never touches loopback.** All Tailscale work runs off the boot path; loopback serving never waits on it.
9. **A remote client does not manage the host's process.** Stop (the pid kill in `daemon_client::stop`) and Restart (`Shutdown`) are Local-only; quitting on a remote host only disconnects.
10. **The host stays available.** While remote access is on, idle exit is held (`crates/daemon/src/idle_exit.rs`); autostart covers the time after login.
11. **The PWA's origin is stable:** the configured port and the MagicDNS name. Changing either creates a new origin, and the status says so.
12. **A remote client is full control** (it can spawn a shell), so invariants 1 to 7 are the whole boundary; no per-message check is one.

## Items, in build order

Hotspot items (touching `crates/protocol/src/lib.rs` or `crates/daemon/src/server.rs`, whose `dispatch` routes every message) run one at a time.

| Id | Title | Hotspot | Depends on |
|---|---|---|---|
| TS.1 | `crates/tailnet`: the Tailscale CLI wrapper (typed errors for every failure) and the owner rule, table-tested. Landed, 4d18a51 | no | none |
| TS.2 | Daemon tailnet listener module: cert, bind with rebind, whois acceptor attaching a peer identity. Landed, 6e1c847, dffb25d, 08ab90b; started by TS.3 | no | TS.1 |
| TS.3 | Wire the listener into the daemon: boot, the auth kind per listener, a separate router, the `Origin`/`Host` check, idle exit held, `remote.json`. Landed, 77362be | yes | TS.2 |
| TS.3b | Device tokens: a whois-authorized device without a token waits as a pending request; the token is issued once it presents the one-time approval the host PC shows (a QR code to scan, or the pairing code beside it for a device with no camera), started from a local (loopback) client; tokens are stored hashed and checked in `Hello` | yes | TS.3, TS.4 |
| TS.4 | `ConfigureRemote` / `RemoteStatus` messages: every state (not installed, not running, other Windows user, not connected, HTTPS disabled, host tagged, serving with name, port and cert expiry). Landed, 48c9ef9 | yes | TS.3 |
| TS.5 | Retire the LAN listener, `/pair` and mDNS; the six variants inert. Landed, 7d4556e | yes | TS.3 |
| RT-96 | Remote devices: list the tailnet devices that connected, block one and drop its connections | yes | TS.4, P6.4 |
| P6.1 | Transport seam: `Target::{Local, Tailnet(host)}` in `net.rs` over public-CA TLS. Landed on feat/remote-mode | no | TS.1 |
| P6.2a | Remote-mode rules: local-file actions disabled with a reason, quit only disconnects, Stop, handshake and `daemon.log` hidden, Reconnect label, the capability table. Landed on feat/remote-mode (RT-151) | no | P6.1 |
| P6.2b | The typed host path in place of the pickers on a remote host (RT-257) | no | P6.2a |
| P6.3 | Host picker from the tailnet's machines, a typed-name fallback, boot reconnect | no | P6.1, P6.2a |
| P6.4 | Host Settings: Remote access (Tailscale) panel | no | TS.4 |
| P6.5 | Autostart: start the daemon on login | no | none |
| P6.6 | Remote tests over a fake Tailscale, the hand checklist, and the docs pass | no | all of the above |
| P6.7 | Cutover: installer (no firewall rule, per RT-157) | no | P6.5 |

File fetch (RT-35 to RT-37) follows P6.1 to P6.3; a host's download folder is keyed by its Tailscale StableNodeID.

## Facts relied on

From Tailscale's docs (kb/1080 CLI, kb/1153 enabling HTTPS, kb/1084 sharing, the tags and identity docs, kb/1033 addresses) and source (`apitype.go`, `tailcfg.go`, `ipn/ipnlocal/cert.go`), checked when this design was written: `whois --json` returns `{Node, UserProfile, CapMap}`, never nil on success; `Node.User` does not reflect a tagged node's identity; a shared node appears as its sharer; HTTPS certificates must be enabled in the admin console and are 90-day Let's Encrypt certificates that `tailscale cert` renews below `--min-validity`; machine names are published in CT logs; a node's IP is stable while it stays registered. Not yet verified: whether `tailscale cert` runs unelevated for the Windows user who owns Tailscale, how fast a removed device's live TCP connection dies, and the exact IPv4 range text in Tailscale's docs.
