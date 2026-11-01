# Remote access over Tailscale

Design for Wave 3's remote desktop (Linear RT-33, RT-34 and their sub-issues). Remote use of the daemon is **Tailscale-first**: Tailscale is required, and the daemon's only remote listener serves its tailnet. Local use (loopback, the `daemon.json` token) is unchanged. The iPhone PWA (RT-143, `docs/research/pwa-iphone.md`) reaches the same listener.

## Owner rulings

- Tailscale is a requirement for remote use. The daemon serves TLS with its `tailscale cert` (`*.ts.net`, public CA), one listener for the desktop client and the PWA; clients verify the certificate normally and nothing is pinned.
- A connecting device is authorized by `tailscale whois` **plus a per-device token** stored on the device. No pairing codes. A device without a token (a new device, or the PWA after Safari evicts its storage) asks for one, and the token is issued only once the owner **approves that device on the host PC**: whois alone never mints a token, so another program or Windows account on an owner device cannot get one unseen.

  Approval is a **QR code** the host PC shows, which the device scans (the iPhone PWA with its camera), with a short **pairing code** beside it that a device with no camera types instead; the QR code and the code are one-time, short-lived and good only for a whois-authorized device. A wiped phone is approved again.

  The native client's views of that request live in `apps/native/src/pairing_view.rs` over the model in `pairing.rs`. Settings → Remote access shows a `Waiting devices · {n}` section (name and Tailscale login, a `m:ss left` countdown, Approve… and Deny; an approved row reads `Approved · waiting for the device` with only Deny), and the Remote access tab carries an amber count badge (`99+` cap, approved requests included).

  Approve… opens an approval dialog that sends nothing until Approve is pressed, then shows the QR code, the code as `735 160` and `expires in m:ss · this request only`, and closes by itself when the code runs out. Disconnecting from This PC's daemon closes it and forgets the waiting requests.

  On each connect to this PC's daemon whose `Welcome` lists `device_pairing` the client asks `ListPendingDevices` (local only, never on a tailnet or SSH host), and the daemon's `PendingDevices` and `DevicePairCode` feed the rows, the badge and the dialog. Each new unapproved request raises a Windows toast, "A device asks for remote access", with Approve… and Deny (`notify.rs`, raised from `pairing_view.rs`): Approve… brings the window forward and opens the dialog without opening Settings, Deny sends `DenyDevice`.

  The device's side of the same request is the native client's "Pair with {host}" dialog (`device_pairing.rs`, `device_pairing_view.rs`): it sends `DevicePairRequest`, waits for the owner's approval on the host PC, takes the pairing code typed from the host and stores the returned `DeviceToken` for that host before reconnecting (`docs/native-client.md`, "Pair dialog").
- "The owner" is a device logged in as the host node's own Tailscale user (numeric `UserID`, never the display login), untagged, not shared in, and not the host itself.
- The listener binds the host's own tailnet addresses only (v4 and v6), on the port `remote.json` configures (default 8787), rebinding when they change.
- Shutdown and turning remote access off are refused from tailnet connections: host-only.
- Revocation is a block list in rustling-tulip: Settings lists the approved tailnet devices (each with its `last_seen` and a `connected` count), and blocking one drops its live connections at once and refuses it at every pairing path until the owner forgets it (RT-96). The daemon half (RT-393), the native model (RT-391) and the Settings Devices section (RT-392) have landed.
- There is no LAN listener, `/pair` or mDNS: the daemon binds loopback and, with remote access on, the host's own tailnet addresses. The six wire variants (`ConfigureLan`, `StartPairing`, `CancelPairing`, `LanStatus`, `PairingStarted`, `PairingEnded`) stay decodable and inert.
- Placement: the daemon side (TS.1 to TS.5) lands on `main`, behind a `remote.json` switch that is off by default; the native client's remote mode (P6.1 to P6.4 and the tests) lands on `main` too. The installer and the docs pass follow.
- The native client dials TLS itself inside `net.rs`; there is no loopback bridge.
- The relay (RT-94), pinned TLS, mDNS discovery, pairing by code and add-by-pasted-code are dropped. RT-95 (Tailscale by MagicDNS name) folds into the host picker.
- **No Windows Firewall rule** (RT-157): the owner runs Fort Firewall, whose rule must cover the content-addressed binaries-cache folder (`%LOCALAPPDATA%\leftos\rustling-tulip\data\binaries\`), not one exe path, since each build's daemon and tracer are new files; P6.7's installer needs no elevated port rule.

## Orchestrator decisions (not owner rulings; open to the owner)

- A denied peer is dropped before the TLS handshake, never answered with a 403.
- whois goes through the `tailscale` CLI (`whois --json`, `whoami --json`, `cert`, `status --json`), run with `CREATE_NO_WINDOW`; the binary is `RUSTLING_TULIP_TAILSCALE`, else `%ProgramFiles%\Tailscale\tailscale.exe`, else PATH. The LocalAPI is not used: its Windows pipe path is undocumented.
- Certificates: checked at start and every 24 h with `tailscale cert --min-validity` of about 14 days, reloaded only when the files' SHA-256 changed, by building the daemon's own rustls `ServerConfig` (ALPN `http/1.1` only, since `RustlsConfig::reload_from_pem_file` forces h2) and swapping it with `RustlsConfig::reload_from_config`; the key lives under the machine-local data dir.
- The host picker reads `tailscale status --json` and also accepts a typed MagicDNS name, since the JSON is documented as subject to change.
- Remote tests: daemon in-process tests with a fake whois and a test CA, the native client's fake-TLS daemon, the live e2e tier (`apps/native/tests/e2e_remote.rs`) over the `tools/e2e/fake-tailscale/` shim against a daemon built with the off-by-default `e2e-loopback-peers` feature, and the hand checklist on real Tailscale ([remote-hand-checklist.md](../remote-hand-checklist.md): two machines and the iPhone). No production setting widens the allowed address ranges.

## Invariants and the sites that obey them

1. **Only tailnet peers reach the remote listener.** Bind the tailnet addresses from `whoami`; the acceptor rejects any peer outside `100.64.0.0/10` or `fd7a:115c:a1e0::/48` (IPv4-mapped addresses canonicalised) before whois. The daemon never binds `0.0.0.0` (`Ipv4Addr::UNSPECIFIED`): its only other listener is loopback. Site: new `crates/daemon/src/tailnet_listener.rs`.
2. **whois is asked about the real peer and fails closed.** The argument is the accepted socket's `peer_addr()`, never a header or a `Hello` field. Timeout (2 to 3 s), non-zero exit, unparsable JSON, a missing binary and the Windows "server mode" 401 all reject. Verdicts are cached per IP for at most 30 s. Sites: the custom `axum_server::accept::Accept` in the listener module; `crates/tailnet/src/cli.rs`.
3. **The owner rule** is a pure function: allow only when whois succeeded, the peer node has no tags, its `UserID` equals the host node's, it is neither a sharer's nor a sharee node, its `ID` differs from the host's (this rejects other Windows users and anything proxied on the host), and the peer IP is one of the node's own `Addresses` (this rejects subnet-routed sources). A tagged host binds nothing and asks for no certificate; the status says `host_tagged`. Site: `crates/tailnet/src/rule.rs`.
4. **The listener decides how a connection authenticates.** Loopback connections compare the `daemon.json` token; tailnet connections never look at it, and they present their device token instead. The loopback listener serves `build_loopback_router`; the tailnet listener has its own `build_tailnet_router`, and `/shutdown` is never on it.

   No router serves `/pair`. Site: `server/connection.rs` `AuthKind`, `server/handshake.rs` `handshake`, `server/remote.rs` `tailnet_ws_handler` and `build_tailnet_router`, `server.rs` `ws_handler` and `build_loopback_router`.
5. **A credential never goes to another host.** The native client sends the `daemon.json` token only to the loopback daemon; to each tailnet host it sends that host's device token, or an empty one when none is stored, and lands in "not paired" when the host has not approved the device. No status message carries a token. Site: `apps/native/src/net.rs` (`attempt`).
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
| RT-96 | Remote devices: list the approved tailnet devices, block one and drop its connections. Landed: daemon (RT-393), native model (RT-391), Settings section (RT-392) | yes | TS.4, P6.4 (landed) |
| P6.1 | Transport seam: `Target::{Local, Tailnet(host)}` in `net.rs` over public-CA TLS. Landed on main | no | TS.1 |
| P6.2a | Remote-mode rules: local-file actions disabled with a reason, quit only disconnects, Stop, handshake and `daemon.log` hidden, Reconnect label, the capability table. Landed on main (RT-151) | no | P6.1 |
| P6.2b | The typed host path in place of the pickers on a remote host. Landed on main (RT-257) | no | P6.2a |
| P6.3 | Host manager from the tailnet's machines, a typed-name fallback, every enabled saved host connected at launch | no | P6.1, P6.2a |
| P6.4 | Host Settings: Remote access (Tailscale) panel. Landed on main (RT-154) | no | TS.4 |
| P6.5 | Autostart: start the daemon on login | no | none |
| P6.6 | Remote tests over a fake Tailscale, the hand checklist, and the docs pass. Part A landed (RT-158): the `e2e-loopback-peers` seam, the `tools/e2e/fake-tailscale/` shim and the smoke spec `remote_listener_comes_up_for_the_fake_tailnet`; the hand checklist is [docs/remote-hand-checklist.md](../remote-hand-checklist.md) (part C); part B, the eight specs of `e2e_remote.rs`, and part D, the docs pass, have landed | no | all of the above |
| P6.7 | Cutover: installer (no firewall rule, per RT-157) | no | P6.5 |

File fetch (RT-35 to RT-37) follows P6.1 to P6.3; a host's download folder is keyed by its MagicDNS name, lower-cased.

### P6.3: the host manager (RT-350 to RT-354)

Landed on main. One window connects This PC and every enabled saved host at once, one connection per host (`docs/native-client.md` and `docs/architecture.md`, "Several hosts"); there is no target switch, no remembered last target and no boot target.

- **DNS-name check.** `Target::parse_tailnet(&str, Option<u16>)` in `apps/native/src/target.rs` accepts a MagicDNS name, trims one trailing dot, lowercases it, and refuses userinfo, a path, query or fragment, a port in the name, whitespace, percent signs, empty labels, bad label edges, IP literals and a bare label; `HostNameError`'s Display is the message the host manager shows. `Target::tailnet` stays infallible for test fixtures.
- **Saved hosts and peers.** `connection_picker.rs` holds the model: rows list This PC, then the saved hosts, then the online tailnet peers (offline, tagged and shared peers are not offered), over a `PeerSource` that runs `tailnet::Cli::locate().status()`. A typed name goes through the DNS check, and Forget removes a saved host. `saved_hosts` in `native-ui.json` stores each host's name and port, and a name that fails the check is dropped on load.
- **The view.** `connection_picker_view.rs` is the Connections host manager, drawn above the first-connect overlay. The typed-host entry has a Port field beside the name, prefilled with 8787; a saved host keeps the port it was added with, and a listed peer dials 8787 unless a saved host of that name records another, since the Remote access page lets a host change its port.
- **Connecting.** Each enabled saved host connects at launch and stays connected; its on/off switch is remembered in `saved_hosts`.

## Facts relied on

From Tailscale's docs (kb/1080 CLI, kb/1153 enabling HTTPS, kb/1084 sharing, the tags and identity docs, kb/1033 addresses) and source (`apitype.go`, `tailcfg.go`, `ipn/ipnlocal/cert.go`), checked when this design was written: `whois --json` returns `{Node, UserProfile, CapMap}`, never nil on success; `Node.User` does not reflect a tagged node's identity; a shared node appears as its sharer.

HTTPS certificates must be enabled in the admin console and are 90-day Let's Encrypt certificates that `tailscale cert` renews below `--min-validity`; machine names are published in CT logs; a node's IP is stable while it stays registered.

The facts not yet verified on a real tailnet are steps of the [hand checklist](../remote-hand-checklist.md) (sections 2, 7 and 8).
